//! `gt` — database monitoring daemon and one-shot runner.
//! Subcommands: check (validate), run (once), watch (daemon), mcp (stdio server).

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::SystemTime;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use clap::{Parser, Subcommand};
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use groundtruth::auth::{AuthState, auth_middleware};

use groundtruth::app::{build_sources, notify_failures, run_checks, run_once};
use groundtruth::config::{
    StateConfig, parse_checks, parse_connections, parse_heartbeat, parse_notifiers,
    parse_state_config,
};
use groundtruth::mcp;
use groundtruth::metrics;
use groundtruth::report::{
    Format, filter, http_status, parse_format, parse_status, render, render_as, render_json,
};
use groundtruth::runner::{CheckResult, Status};
use groundtruth::schedule::{interval_secs, is_due, parse_schedule, schedule_is_due};
use groundtruth::source::Source;
use groundtruth::state::{AnyPool, StateStore};

#[derive(Parser)]
#[command(name = "gt", about = "Database health monitor", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse config only — print "OK: N checks, M connections" or the error.
    Check {
        /// Path to the HCL config file.
        config: String,
    },
    /// Run all checks once and print the report. Exits non-zero on FAIL/ERROR.
    Run {
        /// Path to the HCL config file.
        config: String,
        /// Output as JSON instead of the terminal report.
        #[arg(long)]
        json: bool,
    },
    /// Daemon: run checks on schedule, expose /metrics and /healthz.
    Watch {
        /// Path to the HCL config file.
        config: String,
        /// Address for the HTTP server.
        #[arg(long, default_value = "127.0.0.1:9090")]
        addr: String,
    },
    /// MCP (Model Context Protocol) stdio server for AI agent integration.
    Mcp {
        /// Path to the HCL config file.
        config: String,
    },
    /// Scaffold a starter config + GitHub Actions workflow for free scheduled monitoring.
    Init {
        /// Directory to scaffold into (default: current directory).
        #[arg(default_value = ".")]
        dir: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    // Log to stderr so stdout stays clean for `run --json` and MCP stdio.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Check { config } => cmd_check(&config).await,
        Command::Run { config, json } => cmd_run(&config, json).await,
        Command::Watch { config, addr } => cmd_watch(&config, &addr).await,
        Command::Mcp { config } => cmd_mcp(&config).await,
        Command::Init { dir } => cmd_init(&dir).await,
    }
}

/// Build a StateStore: `state.dsn` unset → Memory; else a Sql backend connected
/// from that DSN. This is groundtruth's own writable bookkeeping DB, independent of
/// the connectorx check connections.
async fn build_state_store(cfg: &StateConfig) -> Result<StateStore, String> {
    match &cfg.dsn {
        None => Ok(StateStore::memory()),
        Some(dsn) => {
            let pool = AnyPool::connect(dsn)
                .await
                .map_err(|e| format!("state store: {e:#}"))?;
            StateStore::sql(pool)
                .await
                .map_err(|e| format!("failed to open Sql state store: {e:#}"))
        }
    }
}

async fn cmd_check(path: &str) -> ExitCode {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            error!(path, err = %e, "cannot read config file");
            return ExitCode::FAILURE;
        }
    };

    let connections = match parse_connections(&src) {
        Ok(c) => c,
        Err(e) => {
            error!(err = %e, "config parse error");
            return ExitCode::FAILURE;
        }
    };
    let checks = match parse_checks(&src) {
        Ok(c) => c,
        Err(e) => {
            error!(err = %e, "config parse error");
            return ExitCode::FAILURE;
        }
    };
    let notifiers = match parse_notifiers(&src) {
        Ok(n) => n,
        Err(e) => {
            error!(err = %e, "config parse error");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = parse_state_config(&src) {
        error!(err = %e, "config parse error");
        return ExitCode::FAILURE;
    }
    // Validate the heartbeat block too, so a malformed fail_body template fails
    // closed here rather than silently at run time.
    let heartbeat = match parse_heartbeat(&src) {
        Ok(h) => h,
        Err(e) => {
            error!(err = %e, "config parse error");
            return ExitCode::FAILURE;
        }
    };

    println!(
        "OK: {} check(s), {} connection(s), {} notifier(s), {} heartbeat",
        checks.len(),
        connections.len(),
        notifiers.len(),
        if heartbeat.is_some() { 1 } else { 0 }
    );
    ExitCode::SUCCESS
}

async fn cmd_run(path: &str, json: bool) -> ExitCode {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            error!(path, err = %e, "cannot read config file");
            return ExitCode::FAILURE;
        }
    };

    match run_once(&src).await {
        Ok(results) => {
            if json {
                println!("{}", render_json(&results));
            } else {
                println!("{}", render(&results));
            }

            // Best-effort liveness ping — never affects the exit code.
            fire_heartbeat(&src, &results, path).await;

            let bad = results
                .iter()
                .any(|r| matches!(r.status, Status::Fail | Status::Error));
            if bad {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            error!(err = %e, "run failed");
            ExitCode::FAILURE
        }
    }
}

/// Fire one best-effort heartbeat for a completed run, if the config declares a
/// `heartbeat` block. A missing block is a no-op; a ping failure or an invalid
/// block is logged and swallowed — the heartbeat never changes the exit code.
async fn fire_heartbeat(src: &str, results: &[CheckResult], config_name: &str) {
    match parse_heartbeat(src) {
        Ok(Some(hb)) => {
            if let Err(e) = groundtruth::heartbeat::ping(&hb, results, config_name).await {
                warn!(err = %e, "heartbeat ping failed");
            }
        }
        Ok(None) => {}
        Err(e) => warn!(err = %e, "invalid heartbeat block; skipping ping"),
    }
}

type SharedResults = Arc<RwLock<Vec<CheckResult>>>;

/// Shared application state for the HTTP handlers.
pub type AppState = SharedResults;

/// Build the `watch` router. Protected routes require the token (when set);
/// `/healthz` is always open.
pub fn build_router(state: AppState, token: Option<String>) -> Router {
    let auth_state = AuthState { token };

    let protected = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/checks", get(checks_handler))
        .route("/checks/{name}", get(checks_name_handler))
        .route_layer(middleware::from_fn_with_state(auth_state, auth_middleware))
        .with_state(state.clone());

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(protected)
}

/// Resolves on SIGINT or SIGTERM (SIGINT only on non-Unix).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install CTRL+C handler");
    };

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");

        tokio::select! {
            () = ctrl_c => {},
            _ = sigterm.recv() => {},
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;
}

/// Everything the tick loop needs, bundled to stay under Clippy's argument limit.
pub struct TickLoopCtx {
    /// Config source parsed at startup; the loop's initial last-good config.
    pub src: String,
    /// Path to the config file, re-read each tick to hot-reload check edits.
    pub config_path: String,
    pub live: HashMap<String, Source>,
    pub dead: HashMap<String, String>,
    pub first_connection_name: String,
    pub shared: SharedResults,
    pub notifiers: Vec<(String, groundtruth::notify::Notifier)>,
    pub state: StateStore,
}

/// The `watch` daemon tick loop. Flip `shutdown_rx` to `true` for a clean exit
/// after the current tick; standalone for testability.
pub async fn run_tick_loop(ctx: TickLoopCtx, mut shutdown_rx: tokio::sync::watch::Receiver<bool>) {
    let TickLoopCtx {
        src,
        config_path,
        live,
        dead,
        first_connection_name,
        shared,
        notifiers,
        state,
    } = ctx;
    // The last config that parsed cleanly; a bad edit never replaces it.
    let mut current_src = src;
    let default_interval: u64 = 60;
    let mut last_run: HashMap<String, i64> = HashMap::new();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    // Drop ticks missed while a previous one was still running.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }

        let now = unix_now();

        // Hot-reload: re-read the config file so check edits take effect without a
        // restart. Connections, the state store, and notifiers are bound at
        // startup and are NOT reloaded — changing those still needs a restart. On
        // a read or parse error we keep the last-good config and log it, so a
        // half-written or broken edit never takes a running monitor down.
        match std::fs::read_to_string(&config_path) {
            Ok(disk) if disk != current_src => match parse_checks(&disk) {
                Ok(_) => {
                    info!("config change detected — reloading checks");
                    current_src = disk;
                }
                Err(e) => {
                    error!(err = %e, "reloaded config failed to parse — keeping last-good config");
                }
            },
            Ok(_) => {}
            Err(e) => {
                error!(err = %e, "cannot re-read config file — keeping last-good config");
            }
        }

        let checks = match parse_checks(&current_src) {
            Ok(c) => c,
            Err(e) => {
                error!(err = %e, "config parse error on tick");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        let due: Vec<groundtruth::config::Check> = checks
            .into_iter()
            .filter(|c| {
                let lr = last_run.get(c.name.as_str()).copied();
                match c.every.as_deref().and_then(parse_schedule) {
                    Some(sched) => schedule_is_due(&sched, lr, now),
                    None => {
                        let every = c
                            .every
                            .as_deref()
                            .and_then(interval_secs)
                            .unwrap_or(default_interval);
                        is_due(every, lr, now)
                    }
                }
            })
            .collect();

        if !due.is_empty() {
            let results = run_checks(&live, &dead, &due, &first_connection_name).await;

            for c in &due {
                last_run.insert(c.name.clone(), now);
            }

            let n_pass = results.iter().filter(|r| r.status == Status::Pass).count();
            let n_warn = results.iter().filter(|r| r.status == Status::Warn).count();
            let n_fail = results.iter().filter(|r| r.status == Status::Fail).count();
            let n_err = results.iter().filter(|r| r.status == Status::Error).count();
            info!(
                pass = n_pass,
                warn = n_warn,
                fail = n_fail,
                error = n_err,
                total = results.len(),
                "tick complete"
            );

            for r in &results {
                match r.status {
                    Status::Fail => {
                        warn!(check = %r.name, detail = %r.detail, "check FAIL");
                    }
                    Status::Error => {
                        error!(check = %r.name, detail = %r.detail, "check ERROR");
                    }
                    _ => {}
                }
            }

            if let Err(e) = notify_failures(&results, &due, &notifiers, &state, now).await {
                error!(err = %e, "notification dispatch error");
            }

            // Publish for /metrics.
            *shared.write().await = results;
        }
    }
}

async fn cmd_watch(path: &str, addr: &str) -> ExitCode {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            error!(path, err = %e, "cannot read config file");
            return ExitCode::FAILURE;
        }
    };

    let shared: SharedResults = Arc::new(RwLock::new(Vec::new()));

    // Notifiers don't change at runtime — parse once.
    let notifiers = match parse_notifiers(&src) {
        Ok(n) => n,
        Err(e) => {
            error!(err = %e, "config parse error");
            return ExitCode::FAILURE;
        }
    };

    // Build connection pools once and reuse across every tick.
    let connections = match parse_connections(&src) {
        Ok(c) => c,
        Err(e) => {
            error!(err = %e, "config parse error");
            return ExitCode::FAILURE;
        }
    };

    if connections.is_empty() {
        error!("no `connection` block found in config");
        return ExitCode::FAILURE;
    }

    let first_connection_name = connections[0].name.clone();
    let (live, dead) = build_sources(&connections).await;

    let state_cfg = match parse_state_config(&src) {
        Ok(c) => c,
        Err(e) => {
            error!(err = %e, "config parse error");
            return ExitCode::FAILURE;
        }
    };
    let state = match build_state_store(&state_cfg).await {
        Ok(s) => s,
        Err(e) => {
            error!(err = %e, "state store initialisation failed");
            return ExitCode::FAILURE;
        }
    };

    for name in live.keys() {
        info!(connection = %name, "connection established");
    }
    for (name, err) in &dead {
        warn!(connection = %name, err = %err, "connection failed at startup");
    }

    let token = std::env::var("GROUNDTRUTH_TOKEN").ok();
    if token.is_none() {
        warn!("GROUNDTRUTH_TOKEN not set — HTTP endpoints are UNAUTHENTICATED");
    }

    // One sender fans shutdown out to both the server and the tick loop.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let server_shared = shared.clone();
    let app = build_router(server_shared, token);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(addr, err = %e, "cannot bind HTTP listener");
            return ExitCode::FAILURE;
        }
    };
    let checks_count = match parse_checks(&src) {
        Ok(c) => c.len(),
        Err(_) => 0,
    };
    info!(addr = %addr, checks = checks_count, "watch server listening");

    let server_shutdown_rx = shutdown_rx.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let mut rx = server_shutdown_rx;
                loop {
                    if rx.changed().await.is_err() || *rx.borrow() {
                        break;
                    }
                }
            })
            .await
            .unwrap_or_else(|e| {
                error!(err = %e, "HTTP server error");
            });
    });

    let tick_handle = tokio::spawn(run_tick_loop(
        TickLoopCtx {
            src,
            config_path: path.to_string(),
            live,
            dead,
            first_connection_name,
            shared,
            notifiers,
            state,
        },
        shutdown_rx,
    ));

    shutdown_signal().await;
    info!("shutting down");

    let _ = shutdown_tx.send(true);

    // Let the tick loop drain any in-flight tick.
    let _ = tick_handle.await;

    ExitCode::SUCCESS
}

async fn metrics_handler(State(shared): State<SharedResults>) -> impl IntoResponse {
    let results = shared.read().await;
    metrics::render(&results)
}

/// Render a result set per the `format`/`status`/`limit` params; 400 on bad `format`.
fn checks_response(set: Vec<&CheckResult>, params: &HashMap<String, String>) -> Response {
    let format = match params.get("format").map(|s| s.as_str()) {
        Some(s) => match parse_format(s) {
            Some(f) => f,
            None => {
                let body = format!("unknown format: {s:?}; valid values are json, yaml, text");
                return (StatusCode::BAD_REQUEST, body).into_response();
            }
        },
        None => Format::Json,
    };

    let content_type = match &format {
        Format::Json => "application/json",
        Format::Yaml => "application/yaml",
        Format::Text => "text/plain",
    };

    // render_as takes &[CheckResult], so clone the borrowed set.
    let owned: Vec<CheckResult> = set
        .into_iter()
        .map(|r| CheckResult {
            name: r.name.clone(),
            status: match r.status {
                Status::Pass => Status::Pass,
                Status::Warn => Status::Warn,
                Status::Fail => Status::Fail,
                Status::Error => Status::Error,
            },
            detail: r.detail.clone(),
            sample: r.sample.clone(),
        })
        .collect();

    let code = StatusCode::from_u16(http_status(&owned)).unwrap_or(StatusCode::OK);
    let body = render_as(&owned, format);

    let mut resp = (code, body).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    resp
}

async fn checks_handler(
    State(shared): State<SharedResults>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let results = shared.read().await;

    let status_filter = params.get("status").and_then(|s| parse_status(s));
    let limit = params.get("limit").and_then(|s| s.parse::<usize>().ok());
    let set = filter(&results, status_filter, limit);

    checks_response(set, &params)
}

async fn checks_name_handler(
    State(shared): State<SharedResults>,
    Path(name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let results = shared.read().await;

    let Some(found) = results.iter().find(|r| r.name == name) else {
        return (StatusCode::NOT_FOUND, format!("check not found: {name}")).into_response();
    };

    checks_response(vec![found], &params)
}

/// Run the MCP stdio server via rmcp.
async fn cmd_mcp(path: &str) -> ExitCode {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            error!(path, err = %e, "cannot read config file");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = mcp::serve_stdio(src).await {
        error!(err = %e, "MCP server error");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

/// Scaffold a starter `groundtruth.hcl` and a scheduled GitHub Actions workflow
/// into `dir`. Refuses to overwrite either file if it already exists.
async fn cmd_init(dir: &str) -> ExitCode {
    let root = std::path::Path::new(dir);
    let hcl_path = root.join("groundtruth.hcl");
    let wf_path = root.join(".github/workflows/groundtruth.yml");

    for (path, contents) in [
        (&hcl_path, include_str!("../templates/groundtruth.hcl")),
        (&wf_path, include_str!("../templates/github-workflow.yml")),
    ] {
        if path.exists() {
            error!(path = %path.display(), "refusing to overwrite existing file");
            return ExitCode::FAILURE;
        }
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            error!(err = %e, "cannot create directory");
            return ExitCode::FAILURE;
        }
        if let Err(e) = std::fs::write(path, contents) {
            error!(err = %e, "cannot write file");
            return ExitCode::FAILURE;
        }
    }

    println!("Scaffolded groundtruth.hcl and .github/workflows/groundtruth.yml");
    println!("Next: set DATABASE_URL and HEARTBEAT_URL secrets, then push.");
    ExitCode::SUCCESS
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use groundtruth::auth::check_bearer;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    /// Router with a token set and an empty results store.
    fn router_with_token(token: &str) -> Router {
        let shared: SharedResults = Arc::new(RwLock::new(Vec::new()));
        build_router(shared, Some(token.to_string()))
    }

    /// Router with no token (open mode).
    fn router_open() -> Router {
        let shared: SharedResults = Arc::new(RwLock::new(Vec::new()));
        build_router(shared, None)
    }

    #[tokio::test]
    async fn auth_correct_token_returns_200_on_checks() {
        let app = router_with_token("supersecret");
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/checks")
                    .header(header::AUTHORIZATION, "Bearer supersecret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_missing_header_returns_401() {
        let app = router_with_token("supersecret");
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/checks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers()
                .get(header::WWW_AUTHENTICATE)
                .map(|v| v.as_bytes()),
            Some(b"Bearer".as_ref()),
        );
    }

    #[tokio::test]
    async fn auth_wrong_token_returns_401() {
        let app = router_with_token("supersecret");
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/checks")
                    .header(header::AUTHORIZATION, "Bearer wrongtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_malformed_header_no_bearer_scheme_returns_401() {
        let app = router_with_token("supersecret");
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/checks")
                    // Raw token, no "Bearer " prefix.
                    .header(header::AUTHORIZATION, "supersecret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_bearer_lowercase_scheme_returns_200() {
        // Scheme keyword is case-insensitive (RFC 7235).
        let app = router_with_token("supersecret");
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/checks")
                    .header(header::AUTHORIZATION, "bearer supersecret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn healthz_always_open_even_when_token_is_set() {
        let app = router_with_token("supersecret");
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn no_token_set_checks_returns_200_without_auth() {
        let app = router_open();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/checks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn check_bearer_correct_token_returns_true() {
        assert!(check_bearer(Some("Bearer correcttoken"), "correcttoken"));
    }

    #[test]
    fn check_bearer_wrong_token_returns_false() {
        assert!(!check_bearer(Some("Bearer wrongtoken"), "correcttoken"));
    }

    #[test]
    fn check_bearer_wrong_length_token_returns_false() {
        assert!(!check_bearer(Some("Bearer short"), "correcttoken"));
        assert!(!check_bearer(
            Some("Bearer correcttokenwithextra"),
            "correcttoken"
        ));
    }

    #[test]
    fn check_bearer_missing_header_returns_false() {
        assert!(!check_bearer(None, "correcttoken"));
    }

    #[test]
    fn check_bearer_garbage_header_returns_false() {
        assert!(!check_bearer(Some("garbage!"), "correcttoken"));
        assert!(!check_bearer(Some("Basic correcttoken"), "correcttoken"));
        assert!(!check_bearer(Some(""), "correcttoken"));
    }

    #[test]
    fn check_bearer_empty_token_after_scheme_returns_false() {
        assert!(!check_bearer(Some("Bearer "), ""));
        assert!(!check_bearer(Some("Bearer "), "anything"));
    }

    #[test]
    fn check_bearer_multibyte_header_does_not_panic() {
        // Byte 7 inside a multibyte char: must reject safely, never panic on attacker input.
        assert!(!check_bearer(Some("Bearer€token"), "token"));
        assert!(!check_bearer(Some("Béarer x"), "x"));
        assert!(!check_bearer(Some("€€€€€€€€"), "x"));
    }

    /// `gt init` writes both scaffold files with the expected content, then
    /// refuses to clobber them on a second run.
    #[tokio::test]
    async fn init_writes_scaffold_and_refuses_overwrite() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gt-init-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();

        let code = cmd_init(dir.to_str().unwrap()).await;
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(dir.join("groundtruth.hcl").exists());
        assert!(dir.join(".github/workflows/groundtruth.yml").exists());

        let hcl = std::fs::read_to_string(dir.join("groundtruth.hcl")).unwrap();
        assert!(hcl.contains("heartbeat {"), "config must wire a heartbeat");
        let wf = std::fs::read_to_string(dir.join(".github/workflows/groundtruth.yml")).unwrap();
        assert!(
            wf.contains("gt run groundtruth.hcl"),
            "workflow must run gt"
        );

        // Second run must refuse rather than overwrite existing files.
        let again = cmd_init(dir.to_str().unwrap()).await;
        assert_eq!(again, ExitCode::FAILURE, "must not clobber existing files");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `fire_heartbeat` POSTs the green base URL when the config declares a
    /// heartbeat and every check passed. Exercises the exact wiring `cmd_run`
    /// uses, with no database.
    #[tokio::test]
    async fn fire_heartbeat_pings_base_url_on_success() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });

        unsafe { std::env::set_var("GT_MAIN_HB_URL", format!("http://{addr}")) };
        let src = "heartbeat {\n  url = env(\"GT_MAIN_HB_URL\")\n}";
        let results = vec![CheckResult {
            name: "ok".into(),
            status: Status::Pass,
            detail: "1 row".into(),
            sample: vec![],
        }];

        fire_heartbeat(src, &results, "test.hcl").await;

        let req = server.await.unwrap();
        assert!(
            req.starts_with("POST / "),
            "expected green ping to base url, got: {}",
            req.lines().next().unwrap_or("")
        );
    }

    /// No `heartbeat` block → `fire_heartbeat` is a silent no-op. If it tried to
    /// ping anything this would hang or panic; it must return promptly.
    #[tokio::test]
    async fn fire_heartbeat_noop_without_block() {
        use tokio::time::{Duration, timeout};

        let src = "check \"x\" { query = \"select 1\" }";
        let results = vec![CheckResult {
            name: "x".into(),
            status: Status::Pass,
            detail: String::new(),
            sample: vec![],
        }];

        let done = timeout(
            Duration::from_secs(1),
            fire_heartbeat(src, &results, "c.hcl"),
        )
        .await;
        assert!(done.is_ok(), "no-heartbeat path must return immediately");
    }

    /// run_tick_loop exits cleanly shortly after shutdown is broadcast.
    #[tokio::test]
    async fn tick_loop_exits_on_shutdown_signal() {
        use std::collections::HashMap;
        use tokio::sync::watch;
        use tokio::time::{Duration, timeout};

        // No DB needed; the loop idles.
        let live: HashMap<String, Source> = HashMap::new();
        let dead: HashMap<String, String> = HashMap::new();
        let notifiers: Vec<(String, groundtruth::notify::Notifier)> = vec![];
        let shared: SharedResults = Arc::new(RwLock::new(vec![]));
        let state = StateStore::memory();
        // No checks → tick body is a no-op.
        let src = "connection \"sqlite\" \"main\" {\n  path = \":memory:\"\n}\n".to_string();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let handle = tokio::spawn(run_tick_loop(
            TickLoopCtx {
                src,
                config_path: String::new(),
                live,
                dead,
                first_connection_name: "main".to_string(),
                shared,
                notifiers,
                state,
            },
            shutdown_rx,
        ));

        // One tick to settle, then signal shutdown.
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown_tx.send(true).unwrap();

        // Must exit within 2s (it idles at 1s intervals).
        let result = timeout(Duration::from_secs(2), handle).await;
        assert!(
            result.is_ok(),
            "tick loop must exit within 2s after shutdown signal"
        );
    }

    /// A check added to the config file while the loop is running is picked up on
    /// a later tick, without a restart.
    #[tokio::test]
    async fn tick_loop_hot_reloads_checks_from_file() {
        use std::collections::HashMap;
        use tokio::sync::watch;
        use tokio::time::{Duration, sleep};

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("gt_reload_{nanos}.hcl"));
        let no_checks = "# no checks yet\n";
        std::fs::write(&path, no_checks).unwrap();

        // DB-free Static source stands in for the default "main" connection.
        let mut row = hcl::Map::new();
        row.insert("n".to_string(), hcl::Value::from(0_i64));
        let mut live: HashMap<String, Source> = HashMap::new();
        live.insert(
            "main".to_string(),
            Source::Static(vec![hcl::Value::Object(row)]),
        );
        let dead: HashMap<String, String> = HashMap::new();
        let notifiers: Vec<(String, groundtruth::notify::Notifier)> = vec![];
        let shared: SharedResults = Arc::new(RwLock::new(vec![]));
        let state = StateStore::memory();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(run_tick_loop(
            TickLoopCtx {
                src: no_checks.to_string(),
                config_path: path.to_string_lossy().to_string(),
                live,
                dead,
                first_connection_name: "main".to_string(),
                shared: shared.clone(),
                notifiers,
                state,
            },
            shutdown_rx,
        ));

        // No checks yet → nothing published after a couple of ticks.
        sleep(Duration::from_millis(1200)).await;
        assert!(
            shared.read().await.is_empty(),
            "no checks yet → no results published"
        );

        // Add a check by editing the file; the loop must reload and run it.
        std::fs::write(
            &path,
            "check \"reloaded\" {\n  every = \"1s\"\n  query = \"select 0 as n\"\n  fail = row.n == 0\n}\n",
        )
        .unwrap();

        let mut ran = false;
        for _ in 0..30 {
            sleep(Duration::from_millis(200)).await;
            if !shared.read().await.is_empty() {
                ran = true;
                break;
            }
        }
        shutdown_tx.send(true).unwrap();
        let _ = handle.await;
        let _ = std::fs::remove_file(&path);

        assert!(ran, "check added by file edit should run after hot-reload");
        let results = shared.read().await;
        assert_eq!(results.len(), 1, "exactly the one reloaded check ran");
        assert_eq!(results[0].name, "reloaded");
    }
}
