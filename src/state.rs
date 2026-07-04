//! Pluggable state store for sustained-failure tracking.
//!
//! `Memory` (default, process-local) or `Sql` (persisted to a database);
//! `state { dsn = ... }` selects Sql. `gt run` always uses Memory, so its
//! tracking is best-effort; prefer Sql for production `gt watch`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};

/// Connect timeout for the state-store pool — caps how long a bad state DSN
/// stalls startup.
const STATE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A Postgres or SQLite pool, unifying the two without the `sqlx/any` feature.
/// This is the *only* writable database in groundtruth — the state store. Check
/// queries never use it (they go through connectorx).
#[derive(Clone)]
pub enum AnyPool {
    Sqlite(sqlx::SqlitePool),
    Pg(sqlx::PgPool),
}

impl AnyPool {
    /// Connect a state-store pool from a DSN, routing by URL scheme:
    /// `postgres://` / `postgresql://` → Postgres; `sqlite:` or a bare file path
    /// → SQLite. A `sqlite:NAME?mode=rwc` DSN creates the file if absent.
    pub async fn connect(dsn: &str) -> Result<Self> {
        if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .acquire_timeout(STATE_CONNECT_TIMEOUT)
                .connect(dsn)
                .await
                .context("connecting state store to Postgres")?;
            Ok(AnyPool::Pg(pool))
        } else {
            let url = if dsn.starts_with("sqlite:") {
                dsn.to_string()
            } else {
                format!("sqlite:{dsn}")
            };
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .acquire_timeout(STATE_CONNECT_TIMEOUT)
                .connect(&url)
                .await
                .context("connecting state store to SQLite")?;
            Ok(AnyPool::Sqlite(pool))
        }
    }
}

/// The sustained-failure state store (enum to avoid dyn/async-trait friction).
#[derive(Clone)]
pub enum StateStore {
    Memory(std::sync::Arc<MemoryState>),
    Sql(SqlState),
}

impl StateStore {
    /// The Memory backend (default).
    pub fn memory() -> Self {
        StateStore::Memory(std::sync::Arc::new(MemoryState::default()))
    }

    /// A Sql backend on an existing connection pool.
    pub async fn sql(pool: AnyPool) -> Result<Self> {
        let state = SqlState { pool };
        state.migrate().await?;
        Ok(StateStore::Sql(state))
    }

    /// When the current failing streak began, or `None` if not failing.
    pub async fn failing_since(&self, check: &str) -> Result<Option<i64>> {
        match self {
            StateStore::Memory(m) => m.failing_since(check),
            StateStore::Sql(s) => s.failing_since(check).await,
        }
    }

    /// Record failing/recovered; returns the streak-start timestamp when failing, `None` when recovered.
    pub async fn note_failing(&self, check: &str, failing: bool, at: i64) -> Result<Option<i64>> {
        match self {
            StateStore::Memory(m) => m.note_failing(check, failing, at),
            StateStore::Sql(s) => s.note_failing(check, failing, at).await,
        }
    }
}

/// In-process map of check name → failure-onset timestamp (present only while failing).
#[derive(Default)]
pub struct MemoryState {
    inner: Mutex<HashMap<String, i64>>,
}

impl MemoryState {
    fn failing_since(&self, check: &str) -> Result<Option<i64>> {
        let map = self.inner.lock().unwrap();
        Ok(map.get(check).copied())
    }

    fn note_failing(&self, check: &str, failing: bool, at: i64) -> Result<Option<i64>> {
        let mut map = self.inner.lock().unwrap();
        if !failing {
            map.remove(check);
            return Ok(None);
        }
        // Keep the original onset across repeated failing calls.
        if let Some(&since) = map.get(check) {
            return Ok(Some(since));
        }
        map.insert(check.to_string(), at);
        Ok(Some(at))
    }
}

/// SQL-backed state store. Schema: `failing_state("check" TEXT PK, since INTEGER NOT NULL)`.
#[derive(Clone)]
pub struct SqlState {
    pool: AnyPool,
}

impl SqlState {
    /// Create the `failing_state` table if it doesn't exist.
    async fn migrate(&self) -> Result<()> {
        let sql = r#"
CREATE TABLE IF NOT EXISTS failing_state (
    "check" TEXT PRIMARY KEY,
    since   INTEGER NOT NULL
)"#;
        match &self.pool {
            AnyPool::Sqlite(pool) => {
                sqlx::query(sql)
                    .execute(pool)
                    .await
                    .context("creating failing_state table (SQLite)")?;
            }
            AnyPool::Pg(pool) => {
                sqlx::query(sql)
                    .execute(pool)
                    .await
                    .context("creating failing_state table (Postgres)")?;
            }
        }
        Ok(())
    }

    async fn failing_since(&self, check: &str) -> Result<Option<i64>> {
        match &self.pool {
            AnyPool::Sqlite(pool) => {
                let row: Option<(i64,)> =
                    sqlx::query_as("SELECT since FROM failing_state WHERE \"check\" = ?")
                        .bind(check)
                        .fetch_optional(pool)
                        .await
                        .context("reading failing_since (SQLite)")?;
                Ok(row.map(|(since,)| since))
            }
            AnyPool::Pg(pool) => {
                let row: Option<(i64,)> =
                    sqlx::query_as("SELECT since FROM failing_state WHERE \"check\" = $1")
                        .bind(check)
                        .fetch_optional(pool)
                        .await
                        .context("reading failing_since (Postgres)")?;
                Ok(row.map(|(since,)| since))
            }
        }
    }

    async fn note_failing(&self, check: &str, failing: bool, at: i64) -> Result<Option<i64>> {
        match &self.pool {
            AnyPool::Sqlite(pool) => self.note_failing_sqlite(pool, check, failing, at).await,
            AnyPool::Pg(pool) => self.note_failing_pg(pool, check, failing, at).await,
        }
    }

    async fn note_failing_sqlite(
        &self,
        pool: &sqlx::SqlitePool,
        check: &str,
        failing: bool,
        at: i64,
    ) -> Result<Option<i64>> {
        if !failing {
            sqlx::query("DELETE FROM failing_state WHERE \"check\" = ?")
                .bind(check)
                .execute(pool)
                .await
                .context("clearing failing state (SQLite)")?;
            return Ok(None);
        }

        let existing: Option<(i64,)> =
            sqlx::query_as("SELECT since FROM failing_state WHERE \"check\" = ?")
                .bind(check)
                .fetch_optional(pool)
                .await
                .context("reading failing state (SQLite)")?;

        if let Some((since,)) = existing {
            return Ok(Some(since));
        }

        sqlx::query("INSERT INTO failing_state (\"check\", since) VALUES (?, ?)")
            .bind(check)
            .bind(at)
            .execute(pool)
            .await
            .context("inserting failing state (SQLite)")?;

        Ok(Some(at))
    }

    async fn note_failing_pg(
        &self,
        pool: &sqlx::PgPool,
        check: &str,
        failing: bool,
        at: i64,
    ) -> Result<Option<i64>> {
        if !failing {
            sqlx::query("DELETE FROM failing_state WHERE \"check\" = $1")
                .bind(check)
                .execute(pool)
                .await
                .context("clearing failing state (Postgres)")?;
            return Ok(None);
        }

        let existing: Option<(i64,)> =
            sqlx::query_as("SELECT since FROM failing_state WHERE \"check\" = $1")
                .bind(check)
                .fetch_optional(pool)
                .await
                .context("reading failing state (Postgres)")?;

        if let Some((since,)) = existing {
            return Ok(Some(since));
        }

        sqlx::query("INSERT INTO failing_state (\"check\", since) VALUES ($1, $2)")
            .bind(check)
            .bind(at)
            .execute(pool)
            .await
            .context("inserting failing state (Postgres)")?;

        Ok(Some(at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_backend_failing_since_transitions() {
        let state = StateStore::memory();
        let t0 = 1000_i64;
        let t1 = 2000_i64;
        let t2 = 3000_i64;

        let r = state.note_failing("web", true, t0).await.unwrap();
        assert_eq!(r, Some(t0));

        // Repeated failing keeps the original onset.
        let r = state.note_failing("web", true, t1).await.unwrap();
        assert_eq!(r, Some(t0));

        let r = state.note_failing("web", false, t1).await.unwrap();
        assert_eq!(r, None);

        // New streak after recovery → new onset.
        let r = state.note_failing("web", true, t2).await.unwrap();
        assert_eq!(r, Some(t2));
    }

    #[tokio::test]
    async fn memory_backend_failing_since_is_read_only() {
        let state = StateStore::memory();

        // Unknown check returns None without inserting anything.
        let r = state.failing_since("never_seen").await.unwrap();
        assert_eq!(r, None, "unknown check should return None");

        let r2 = state.note_failing("never_seen", false, 1000).await.unwrap();
        assert_eq!(
            r2, None,
            "no entry should have been created by failing_since"
        );

        state.note_failing("web", true, 500).await.unwrap();
        let r3 = state.failing_since("web").await.unwrap();
        assert_eq!(
            r3,
            Some(500),
            "failing_since should return the since timestamp"
        );
    }

    #[tokio::test]
    async fn sql_backend_sqlite_failing_since_transitions() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let state = StateStore::sql(AnyPool::Sqlite(pool)).await.unwrap();

        let t0 = 1000_i64;
        let t1 = 2000_i64;
        let t2 = 3000_i64;

        let r = state.note_failing("db", true, t0).await.unwrap();
        assert_eq!(r, Some(t0));

        let r = state.note_failing("db", true, t1).await.unwrap();
        assert_eq!(r, Some(t0), "should return original since");

        let r = state.note_failing("db", false, t1).await.unwrap();
        assert_eq!(r, None);

        let r = state.note_failing("db", true, t2).await.unwrap();
        assert_eq!(r, Some(t2));
    }

    #[tokio::test]
    async fn sql_backend_sqlite_failing_since_is_read_only() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let state = StateStore::sql(AnyPool::Sqlite(pool)).await.unwrap();

        let r = state.failing_since("x").await.unwrap();
        assert_eq!(r, None);

        state.note_failing("x", true, 100).await.unwrap();
        let r = state.failing_since("x").await.unwrap();
        assert_eq!(r, Some(100));
    }
}
