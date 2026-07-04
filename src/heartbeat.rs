//! Heartbeat: a run-level liveness sink. Pings an external cron-monitor
//! (Better Stack, healthchecks.io, Cronitor, ...) green on success, fail on
//! FAIL/ERROR. Best-effort — never changes the run's exit code.

use crate::runner::{CheckResult, Status};
use reqwest::Client;
use serde_json::json;
use tokio::time::{Duration, sleep};
use tracing::{info, warn};

/// The 12 template variable names, in one place so parse-time validation and
/// run-time resolution agree.
const VARS: [&str; 12] = [
    "status",
    "total",
    "failed",
    "passed",
    "warned",
    "errored",
    "config",
    "summary",
    "failures",
    "json",
    "summary_json",
    "failures_json",
];

const MAX_BODY: usize = 8192;

/// Content type of the fail body; picks the escaping context for the resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    Text,
    Json,
}

impl ContentType {
    pub fn header(self) -> &'static str {
        match self {
            ContentType::Text => "text/plain",
            ContentType::Json => "application/json",
        }
    }
}

/// Parsed `heartbeat { ... }` block.
#[derive(Debug)]
pub struct HeartbeatConfig {
    /// Pinged when the run is green.
    pub url: String,
    /// Pinged on FAIL/ERROR. `None` → `format!("{url}/fail")`.
    pub fail_url: Option<String>,
    /// Optional raw `{{...}}` template for the fail POST body. `None` → default text summary.
    pub fail_body: Option<String>,
    /// Escaping context + Content-Type header for the fail body.
    pub content_type: ContentType,
}

impl HeartbeatConfig {
    /// The URL used for a failure ping.
    pub fn fail_target(&self) -> String {
        self.fail_url
            .clone()
            .unwrap_or_else(|| format!("{}/fail", self.url.trim_end_matches('/')))
    }
}

/// Resolved template variables for one run.
pub struct Ctx {
    pub status: String,
    pub total: usize,
    pub failed: usize,
    pub passed: usize,
    pub warned: usize,
    pub errored: usize,
    pub config: String,
    pub summary: String,
    pub failures: String,
    pub json: String,
    pub summary_json: String,
    pub failures_json: String,
}

/// True when the run should fire the failure ping.
pub fn is_failed(results: &[CheckResult]) -> bool {
    results
        .iter()
        .any(|r| matches!(r.status, Status::Fail | Status::Error))
}

pub fn build_ctx(results: &[CheckResult], config: &str) -> Ctx {
    let count = |f: fn(&Status) -> bool| results.iter().filter(|r| f(&r.status)).count();
    let failed = count(|s| matches!(s, Status::Fail | Status::Error));
    let passed = count(|s| matches!(s, Status::Pass));
    let warned = count(|s| matches!(s, Status::Warn));
    let errored = count(|s| matches!(s, Status::Error));

    let failures = failures_text(results);
    let summary = summary_text(results, config, failed);

    // Structured payload for the JSON variants.
    let json_val = json!({
        "config": config,
        "status": if failed > 0 { "fail" } else { "pass" },
        "total": results.len(),
        "failed": failed,
        "checks": results.iter().map(|r| json!({
            "name": r.name,
            "status": status_word(&r.status),
            "detail": r.detail,
            "sample": r.sample,
        })).collect::<Vec<_>>(),
    });
    let failures_val: Vec<_> = results
        .iter()
        .filter(|r| matches!(r.status, Status::Fail | Status::Error))
        .map(|r| json!({"name": r.name, "status": status_word(&r.status), "detail": r.detail}))
        .collect();

    Ctx {
        status: if failed > 0 { "fail" } else { "pass" }.into(),
        total: results.len(),
        failed,
        passed,
        warned,
        errored,
        config: config.to_string(),
        summary_json: serde_json::to_string(&summary).unwrap(),
        summary,
        failures,
        json: json_val.to_string(),
        failures_json: serde_json::to_string(&failures_val).unwrap(),
    }
}

fn status_word(s: &Status) -> &'static str {
    match s {
        Status::Pass => "PASS",
        Status::Warn => "WARN",
        Status::Fail => "FAIL",
        Status::Error => "ERROR",
    }
}

/// One sample row as a compact single line (`{"id":3,"order_id":999}`). Uses
/// serde so keys/values are JSON-escaped and the row never sprawls across lines
/// the way `hcl::Value`'s multi-line Display would.
fn row_line(row: &hcl::Value) -> String {
    serde_json::to_string(row).unwrap_or_else(|_| String::from("{}"))
}

/// Plain-text block of failing checks, with a couple of sample rows each.
fn failures_text(results: &[CheckResult]) -> String {
    let mut out = String::new();
    for r in results
        .iter()
        .filter(|r| matches!(r.status, Status::Fail | Status::Error))
    {
        out.push_str(&format!(
            "{}  {} — {}\n",
            status_word(&r.status),
            r.name,
            r.detail
        ));
        for row in r.sample.iter().take(2) {
            out.push_str(&format!("  {}\n", row_line(row)));
        }
    }
    out
}

/// Headline-first, size-bounded default fail body (never JSON).
fn summary_text(results: &[CheckResult], config: &str, failed: usize) -> String {
    let mut body = format!(
        "groundtruth: {}/{} checks FAILED on \"{}\"\n",
        failed,
        results.len(),
        config
    );
    let fails: Vec<_> = results
        .iter()
        .filter(|r| matches!(r.status, Status::Fail | Status::Error))
        .collect();
    let mut shown = 0;
    for r in &fails {
        let line = format!("{}  {} — {}\n", status_word(&r.status), r.name, r.detail);
        if body.len() + line.len() > MAX_BODY - 32 {
            break;
        }
        body.push_str(&line);
        for row in r.sample.iter().take(2) {
            let rl = format!("  {}\n", row_line(row));
            if body.len() + rl.len() > MAX_BODY - 32 {
                break;
            }
            body.push_str(&rl);
        }
        shown += 1;
    }
    if shown < fails.len() {
        let more = fails.len() - shown;
        let noun = if more == 1 { "check" } else { "checks" };
        body.push_str(&format!("(+{more} more {noun})\n"));
    }
    // Final safety net: the loop above only enforces a running budget, so a
    // pathological headline (e.g. an enormous `config` path) could still push
    // `body` past MAX_BODY. Guarantee the hard cap here, truncating at the
    // largest valid UTF-8 char boundary at or before MAX_BODY.
    if body.len() > MAX_BODY {
        let mut end = MAX_BODY;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
    }
    body
}

pub fn default_text_body(ctx: &Ctx) -> String {
    ctx.summary.clone()
}

/// Reject a template that references an unknown `{{var}}`.
pub fn validate_template(tmpl: &str) -> anyhow::Result<()> {
    let mut rest = tmpl;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let close = after
            .find("}}")
            .ok_or_else(|| anyhow::anyhow!("heartbeat: unterminated `{{{{` in fail_body"))?;
        let name = after[..close].trim();
        if !VARS.contains(&name) {
            anyhow::bail!("heartbeat: unknown template variable `{{{{{name}}}}}` in fail_body");
        }
        rest = &after[close + 2..];
    }
    Ok(())
}

/// Single-pass `{{var}}` substitution with escaping keyed to `ct`.
pub fn resolve(tmpl: &str, ctx: &Ctx, ct: ContentType) -> anyhow::Result<String> {
    let mut out = String::with_capacity(tmpl.len() + 64);
    let mut rest = tmpl;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let close = after
            .find("}}")
            .ok_or_else(|| anyhow::anyhow!("heartbeat: unterminated `{{{{` in fail_body"))?;
        let name = after[..close].trim();
        out.push_str(&render_var(name, ctx, ct)?);
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn render_var(name: &str, ctx: &Ctx, ct: ContentType) -> anyhow::Result<String> {
    // Numeric — always safe as-is.
    let num = |n: usize| n.to_string();
    // A raw string var: raw in text mode, inner-JSON-escaped in JSON mode.
    let s = |v: &str| -> String {
        match ct {
            ContentType::Text => v.to_string(),
            ContentType::Json => {
                let quoted = serde_json::to_string(v).unwrap(); // "...."
                quoted[1..quoted.len() - 1].to_string() // strip surrounding quotes
            }
        }
    };
    Ok(match name {
        "status" => s(&ctx.status),
        "total" => num(ctx.total),
        "failed" => num(ctx.failed),
        "passed" => num(ctx.passed),
        "warned" => num(ctx.warned),
        "errored" => num(ctx.errored),
        "config" => s(&ctx.config),
        "summary" => s(&ctx.summary),
        "failures" => s(&ctx.failures),
        // Complete JSON values — already escaped by serde_json.
        "json" => ctx.json.clone(),
        "summary_json" => ctx.summary_json.clone(),
        "failures_json" => ctx.failures_json.clone(),
        other => anyhow::bail!("heartbeat: unknown template variable `{{{{{other}}}}}`"),
    })
}

/// Fire exactly one heartbeat for a completed run. Best-effort: returns Err only
/// after all retries fail; callers log and ignore it (never affects exit code).
pub async fn ping(
    cfg: &HeartbeatConfig,
    results: &[CheckResult],
    config: &str,
) -> anyhow::Result<()> {
    let failed = is_failed(results);
    let (target, body, ct) = if failed {
        let ctx = build_ctx(results, config);
        let body = match &cfg.fail_body {
            Some(tmpl) => resolve(tmpl, &ctx, cfg.content_type)?,
            None => default_text_body(&ctx),
        };
        (cfg.fail_target(), body, cfg.content_type)
    } else {
        // Green ping: liveness only, empty body.
        (cfg.url.clone(), String::new(), ContentType::Text)
    };

    info!(target = %target, bytes = body.len(), failed, "heartbeat ping");

    let client = Client::new();
    let delays = [0u64, 200, 400];
    let mut last_err = anyhow::anyhow!("no attempts made");
    for (i, &delay_ms) in delays.iter().enumerate() {
        if delay_ms > 0 {
            sleep(Duration::from_millis(delay_ms)).await;
        }
        let req = client
            .post(&target)
            .header("content-type", ct.header())
            .body(body.clone());
        match req.send().await {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) => {
                last_err = anyhow::anyhow!("heartbeat returned {}", r.status());
                warn!(attempt = i + 1, err = %last_err, "heartbeat attempt failed");
            }
            Err(e) => {
                last_err = anyhow::anyhow!("heartbeat request failed: {e}");
                warn!(attempt = i + 1, err = %last_err, "heartbeat attempt failed");
            }
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CheckResult, Status};

    fn results() -> Vec<CheckResult> {
        vec![
            CheckResult {
                name: "orders_present".into(),
                status: Status::Pass,
                detail: "1 row".into(),
                sample: vec![],
            },
            CheckResult {
                name: "no_orphans".into(),
                status: Status::Fail,
                detail: "3 rows".into(),
                sample: vec![hcl::value!({ id = 3 }), hcl::value!({ id = 4 })],
            },
            CheckResult {
                name: "recon".into(),
                status: Status::Error,
                detail: "timeout \"x\"".into(),
                sample: vec![],
            },
        ]
    }

    #[test]
    fn ctx_counts_and_status() {
        let c = build_ctx(&results(), "orders.hcl");
        assert_eq!(c.total, 3);
        assert_eq!(c.failed, 2); // Fail + Error
        assert_eq!(c.passed, 1);
        assert_eq!(c.status, "fail");
        assert_eq!(c.config, "orders.hcl");
    }

    #[test]
    fn default_body_headline_first_and_bounded() {
        let c = build_ctx(&results(), "orders.hcl");
        let body = default_text_body(&c);
        assert!(
            body.lines().next().unwrap().contains("2/3 checks FAILED"),
            "headline: {body}"
        );
        assert!(body.contains("no_orphans"));
        assert!(body.len() <= 8192);
    }

    #[test]
    fn default_body_sample_rows_are_compact_single_line() {
        // Each sample row must render on one line as compact JSON, not sprawl
        // across multiple lines the way hcl::Value's Display would.
        let c = build_ctx(&results(), "orders.hcl");
        let body = default_text_body(&c);
        assert!(
            body.contains("  {\"id\":3}\n"),
            "expected a compact one-line sample row, got:\n{body}"
        );
        // No sample line should contain a bare `=` (HCL multi-line object marker).
        for line in body.lines().filter(|l| l.trim_start().starts_with('{')) {
            assert!(
                !line.contains(" = "),
                "row rendered as HCL, not JSON: {line}"
            );
        }
    }

    #[test]
    fn warn_only_run_is_green() {
        // WARN mirrors the exit-code rule: it is green, so no fail ping fires.
        let warn_only = vec![
            CheckResult {
                name: "a".into(),
                status: Status::Pass,
                detail: String::new(),
                sample: vec![],
            },
            CheckResult {
                name: "b".into(),
                status: Status::Warn,
                detail: "slow".into(),
                sample: vec![],
            },
        ];
        assert!(!is_failed(&warn_only), "WARN-only run must be green");
    }

    #[test]
    fn resolve_text_is_raw() {
        let c = build_ctx(&results(), "orders.hcl");
        let out = resolve("failed={{failed}} status={{status}}", &c, ContentType::Text).unwrap();
        assert_eq!(out, "failed=2 status=fail");
    }

    #[test]
    fn resolve_json_escapes_inner_and_emits_valid_json() {
        let c = build_ctx(&results(), "orders.hcl");
        // summary_json is a complete quoted JSON string; failed is a number.
        let out = resolve(
            "{\"text\": {{summary_json}}, \"n\": {{failed}}}",
            &c,
            ContentType::Json,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(v["n"], 2);
        assert!(v["text"].as_str().unwrap().contains("no_orphans"));
    }

    #[test]
    fn resolve_json_raw_var_cannot_break_out() {
        // A detail containing a quote must not break the JSON when used via a raw var inside quotes.
        let c = build_ctx(&results(), "orders.hcl");
        let out = resolve("{\"s\": \"{{failures}}\"}", &c, ContentType::Json).unwrap();
        serde_json::from_str::<serde_json::Value>(&out)
            .expect("valid JSON despite quotes in detail");
    }

    #[test]
    fn resolve_is_single_pass() {
        // An injected value that looks like a placeholder is NOT re-expanded.
        let mut c = build_ctx(&results(), "orders.hcl");
        c.config = "{{failed}}".into();
        let out = resolve("{{config}}", &c, ContentType::Text).unwrap();
        assert_eq!(out, "{{failed}}");
    }

    #[test]
    fn validate_rejects_unknown_var() {
        assert!(validate_template("{{nope}}").is_err());
        assert!(validate_template("{{summary}} ok {{failed}}").is_ok());
    }

    #[test]
    fn default_body_hard_capped_under_pathological_input() {
        // Many failing checks with long names/details, plus a very long config
        // path, should still yield a body that is (a) never bigger than
        // MAX_BODY bytes and (b) valid UTF-8 (no char-boundary panic).
        let long_config = "x".repeat(4096);
        let many_results: Vec<CheckResult> = (0..500)
            .map(|i| CheckResult {
                name: format!("check_with_a_very_long_name_number_{i}_{}", "n".repeat(64)),
                status: Status::Fail,
                detail: format!("failure detail padding {}", "d".repeat(128)),
                sample: vec![hcl::value!({ id = i, note = "sample row padding" })],
            })
            .collect();

        let ctx = build_ctx(&many_results, &long_config);
        let body = default_text_body(&ctx);

        assert!(
            body.len() <= 8192,
            "body exceeded hard cap: {} bytes",
            body.len()
        );
        assert!(
            body.is_char_boundary(body.len()),
            "body is not valid UTF-8 at its end"
        );
    }

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn capture(status: u16) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = s.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = format!("HTTP/1.1 {status} OK\r\ncontent-length: 0\r\n\r\n");
            s.write_all(resp.as_bytes()).await.unwrap();
            req
        });
        (url, handle)
    }

    #[tokio::test]
    async fn ping_green_hits_base_url() {
        let (url, server) = capture(200).await;
        let cfg = HeartbeatConfig {
            url: url.clone(),
            fail_url: None,
            fail_body: None,
            content_type: ContentType::Text,
        };
        let ok = vec![CheckResult {
            name: "a".into(),
            status: Status::Pass,
            detail: "ok".into(),
            sample: vec![],
        }];
        ping(&cfg, &ok, "c.hcl").await.unwrap();
        let req = server.await.unwrap();
        assert!(
            req.starts_with("POST / "),
            "expected POST to base url, got: {}",
            req.lines().next().unwrap()
        );
    }

    #[tokio::test]
    async fn ping_fail_hits_fail_url_with_default_body() {
        let (url, server) = capture(200).await;
        let cfg = HeartbeatConfig {
            url,
            fail_url: None,
            fail_body: None,
            content_type: ContentType::Text,
        };
        let bad = vec![CheckResult {
            name: "orphans".into(),
            status: Status::Fail,
            detail: "3 rows".into(),
            sample: vec![],
        }];
        ping(&cfg, &bad, "c.hcl").await.unwrap();
        let req = server.await.unwrap();
        assert!(
            req.starts_with("POST /fail "),
            "expected POST /fail, got: {}",
            req.lines().next().unwrap()
        );
        assert!(
            req.contains("checks FAILED"),
            "default summary body missing"
        );
    }

    #[tokio::test]
    async fn ping_fail_uses_json_template_and_header() {
        let (url, server) = capture(200).await;
        let cfg = HeartbeatConfig {
            url,
            fail_url: None,
            fail_body: Some("{\"n\": {{failed}}}".into()),
            content_type: ContentType::Json,
        };
        let bad = vec![CheckResult {
            name: "x".into(),
            status: Status::Error,
            detail: "boom".into(),
            sample: vec![],
        }];
        ping(&cfg, &bad, "c.hcl").await.unwrap();
        let req = server.await.unwrap();
        assert!(
            req.contains("content-type: application/json"),
            "json content-type missing: {req}"
        );
        assert!(
            req.contains("{\"n\": 1}"),
            "resolved json body missing: {req}"
        );
    }
}
