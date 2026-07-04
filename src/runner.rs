//! Turns a check plus its result rows into a final status.
//! `decide` is pure (unit-testable without a DB); `run_check_full` is the IO wrapper.

use std::future::Future;
use std::time::Duration;

use crate::config::Check;
use crate::context::build_context;
use crate::eval::{Outcome, evaluate_when};
use crate::source::Source;
use hcl::eval::{Context, Evaluate};
use hcl::{Map, Value};

/// Default query timeout when `check.timeout` is unset or unparseable.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Run `fut` with a `secs`-second deadline; `Err(())` on timeout. Named for testability.
pub async fn with_timeout<F, T>(secs: u64, fut: F) -> Result<T, ()>
where
    F: Future<Output = T>,
{
    tokio::time::timeout(Duration::from_secs(secs), fut)
        .await
        .map_err(|_| ())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Error,
}

#[derive(Debug)]
pub struct CheckResult {
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// Failing-row samples captured when a tier with `sample = N` fires.
    pub sample: Vec<Value>,
}

/// Decide a check's status from its result rows. Precedence: a broken condition
/// is ERROR; otherwise FAIL beats WARN beats PASS.
pub fn decide(check: &Check, rows: &[Value]) -> CheckResult {
    let ctx = build_context(rows);

    // fail wins over warn.
    if let Some(tier) = &check.fail {
        match evaluate_when(&tier.when, &ctx) {
            Outcome::Fail => {
                let sample = tier
                    .sample
                    .map(|n| rows.iter().take(n).cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                return result(
                    check,
                    Status::Fail,
                    format!("{} row(s)", rows.len()),
                    sample,
                );
            }
            Outcome::Error(e) => return result(check, Status::Error, e, vec![]),
            Outcome::Pass => {}
        }
    }
    if let Some(tier) = &check.warn {
        match evaluate_when(&tier.when, &ctx) {
            Outcome::Fail => {
                let sample = tier
                    .sample
                    .map(|n| rows.iter().take(n).cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                return result(
                    check,
                    Status::Warn,
                    format!("{} row(s)", rows.len()),
                    sample,
                );
            }
            Outcome::Error(e) => return result(check, Status::Error, e, vec![]),
            Outcome::Pass => {}
        }
    }
    result(
        check,
        Status::Pass,
        format!("{} row(s)", rows.len()),
        vec![],
    )
}

fn result(check: &Check, status: Status, detail: String, sample: Vec<Value>) -> CheckResult {
    CheckResult {
        name: check.name.clone(),
        status,
        detail,
        sample,
    }
}

/// Context for resolving the SQL expression. Declares `each` for `for_each`-expanded checks.
pub fn build_query_context(check: &Check) -> Context<'static> {
    let mut ctx = Context::new();
    crate::functions::register(&mut ctx);
    if let Some(item) = &check.each_value {
        let mut each_obj = Map::new();
        each_obj.insert("value".to_string(), Value::String(item.clone()));
        ctx.declare_var("each", Value::Object(each_obj));
    }
    ctx
}

/// Run a check end to end: resolve SQL, query, then [`decide`].
pub async fn run_check_full(source: &Source, check: &Check) -> CheckResult {
    // Check-level timeout, falling back to default on absence or bad string.
    let timeout_secs = check
        .timeout
        .as_deref()
        .and_then(crate::schedule::interval_secs)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);

    let query_ctx = build_query_context(check);
    let sql = match check.query.evaluate(&query_ctx) {
        Ok(Value::String(s)) => s,
        Ok(other) => {
            return result(
                check,
                Status::Error,
                format!("query is not a string: {other:?}"),
                vec![],
            );
        }
        Err(e) => return result(check, Status::Error, e.to_string(), vec![]),
    };

    let rows = match with_timeout(timeout_secs, source.query(&sql)).await {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => return result(check, Status::Error, e.to_string(), vec![]),
        Err(()) => {
            return result(
                check,
                Status::Error,
                format!("query exceeded timeout ({}s)", timeout_secs),
                vec![],
            );
        }
    };

    if let Some(v) = &check.validate {
        return crate::validate::validate_rows(check, v, &rows);
    }

    decide(check, &rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_checks;

    fn obj(pairs: &[(&str, Value)]) -> Value {
        let mut m = hcl::Map::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v.clone());
        }
        Value::Object(m)
    }

    fn check(src: &str) -> Check {
        parse_checks(src).unwrap().into_iter().next().unwrap()
    }

    #[test]
    fn fail_condition_true_yields_fail() {
        let c = check("check \"x\" {\n  query = \"q\"\n  fail = row.recent == 0\n}");
        let r = decide(&c, &[obj(&[("recent", 0.into())])]);
        assert_eq!(r.status, Status::Fail);
        assert!(r.sample.is_empty(), "bare fail has no sample");
    }

    #[test]
    fn warn_fires_when_only_warn_matches() {
        let c =
            check("check \"x\" {\n  query = \"q\"\n  warn = row.n > 5\n  fail = row.n > 100\n}");
        let r = decide(&c, &[obj(&[("n", 10.into())])]);
        assert_eq!(r.status, Status::Warn);
        assert!(r.sample.is_empty());
    }

    #[test]
    fn fail_beats_warn_when_both_match() {
        let c =
            check("check \"x\" {\n  query = \"q\"\n  warn = row.n > 5\n  fail = row.n > 100\n}");
        let r = decide(&c, &[obj(&[("n", 200.into())])]);
        assert_eq!(r.status, Status::Fail);
    }

    #[test]
    fn neither_matching_is_pass() {
        let c = check("check \"x\" {\n  query = \"q\"\n  fail = row.n > 100\n}");
        let r = decide(&c, &[obj(&[("n", 1.into())])]);
        assert_eq!(r.status, Status::Pass);
        assert!(r.sample.is_empty(), "pass result has no sample");
    }

    #[test]
    fn broken_condition_is_error_not_pass() {
        let c = check("check \"x\" {\n  query = \"q\"\n  fail = row.typo > 0\n}");
        let r = decide(&c, &[obj(&[("n", 1.into())])]);
        assert_eq!(r.status, Status::Error);
        assert!(r.sample.is_empty());
    }

    #[test]
    fn decide_captures_sample_when_tier_fires() {
        // sample=2 over 5 rows → 2 captured.
        let c = check(
            r#"check "x" {
  query = "q"
  fail {
    when   = rows.count > 0
    sample = 2
  }
}"#,
        );
        let rows: Vec<Value> = (1..=5).map(|i| obj(&[("n", i.into())])).collect();
        let r = decide(&c, &rows);
        assert_eq!(r.status, Status::Fail);
        assert_eq!(
            r.sample.len(),
            2,
            "expected 2 sample rows, got {}",
            r.sample.len()
        );

        let c_pass = check(
            r#"check "x" {
  query = "q"
  fail = rows.count > 100
}"#,
        );
        let r_pass = decide(&c_pass, &rows);
        assert_eq!(r_pass.status, Status::Pass);
        assert!(
            r_pass.sample.is_empty(),
            "passing check must have empty sample"
        );
    }

    #[test]
    fn decide_captures_sample_at_most_n() {
        // sample=10 but only 3 rows → 3 captured.
        let c = check(
            r#"check "x" {
  query = "q"
  warn {
    when   = rows.count > 0
    sample = 10
  }
}"#,
        );
        let rows: Vec<Value> = (1..=3).map(|i| obj(&[("id", i.into())])).collect();
        let r = decide(&c, &rows);
        assert_eq!(r.status, Status::Warn);
        assert_eq!(r.sample.len(), 3);
    }

    #[tokio::test]
    async fn query_timeout_completes_in_time() {
        let result = with_timeout(5, async { 42u32 }).await;
        assert_eq!(result, Ok(42u32));
    }

    #[tokio::test]
    async fn query_timeout_elapse_returns_err() {
        let result =
            with_timeout(0, tokio::time::sleep(std::time::Duration::from_secs(9999))).await;
        assert!(result.is_err(), "timed-out future must return Err");
    }

    #[test]
    fn timeout_config_parses_explicit() {
        let c = check(
            r#"check "x" {
  query   = "select 1"
  timeout = "5s"
}"#,
        );
        assert_eq!(c.timeout.as_deref(), Some("5s"));
    }

    /// `defaults { timeout = "5s" }` propagates to a check without its own timeout.
    #[test]
    fn timeout_config_parses_from_defaults() {
        use crate::config::parse_checks;
        let checks = parse_checks(
            r#"
defaults {
  timeout = "5s"
}
check "x" {
  query = "select 1"
}
"#,
        )
        .unwrap();
        assert_eq!(
            checks[0].timeout.as_deref(),
            Some("5s"),
            "check should inherit defaults.timeout"
        );
    }

    /// A check's own `timeout` wins over defaults.
    #[test]
    fn timeout_config_check_overrides_default() {
        use crate::config::parse_checks;
        let checks = parse_checks(
            r#"
defaults {
  timeout = "60s"
}
check "x" {
  query   = "select 1"
  timeout = "5s"
}
"#,
        )
        .unwrap();
        assert_eq!(checks[0].timeout.as_deref(), Some("5s"));
    }

    /// An unparseable timeout string falls back to DEFAULT_TIMEOUT_SECS.
    #[test]
    fn unknown_timeout_value_falls_back_to_default() {
        use crate::schedule::interval_secs;
        let secs = Some("banana")
            .and_then(interval_secs)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        assert_eq!(
            secs, DEFAULT_TIMEOUT_SECS,
            "bad timeout string must fall back to default"
        );
    }
}
