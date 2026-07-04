//! Renders check results as a terminal report, JSON, or YAML.

use crate::runner::{CheckResult, Status};
use hcl::Value;

pub enum Format {
    Json,
    Yaml,
    Text,
}

pub fn parse_format(s: &str) -> Option<Format> {
    match s {
        "json" => Some(Format::Json),
        "yaml" => Some(Format::Yaml),
        "text" => Some(Format::Text),
        _ => None,
    }
}

pub fn parse_status(s: &str) -> Option<Status> {
    match s {
        "pass" => Some(Status::Pass),
        "warn" => Some(Status::Warn),
        "fail" => Some(Status::Fail),
        "error" => Some(Status::Error),
        _ => None,
    }
}

/// 503 if any result is Fail or Error, else 200.
pub fn http_status(results: &[CheckResult]) -> u16 {
    let bad = results
        .iter()
        .any(|r| matches!(r.status, Status::Fail | Status::Error));
    if bad { 503 } else { 200 }
}

pub fn filter(
    results: &[CheckResult],
    status: Option<Status>,
    limit: Option<usize>,
) -> Vec<&CheckResult> {
    let iter = results.iter().filter(|r| match &status {
        Some(s) => &r.status == s,
        None => true,
    });
    match limit {
        Some(n) => iter.take(n).collect(),
        None => iter.collect(),
    }
}

/// One line per check; any `sample` rows render indented below as `key=val` lists.
pub fn render(results: &[CheckResult]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for r in results {
        lines.push(format!(
            "  [{}] {:32} {}",
            icon(&r.status),
            r.name,
            r.detail
        ));
        for row in &r.sample {
            lines.push(format!("      {}", render_row(row)));
        }
    }
    lines.join("\n")
}

/// Render one row as `key=val key=val ...`.
fn render_row(val: &Value) -> String {
    match val {
        Value::Object(m) => m
            .iter()
            .map(|(k, v)| format!("{}={}", k, render_scalar(v)))
            .collect::<Vec<_>>()
            .join(" "),
        other => render_scalar(other),
    }
}

fn render_scalar(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(arr) => format!(
            "[{}]",
            arr.iter().map(render_scalar).collect::<Vec<_>>().join(",")
        ),
        Value::Object(_) => "<object>".to_string(),
    }
}

fn icon(status: &Status) -> &'static str {
    match status {
        Status::Pass => "PASS",
        Status::Warn => "WARN",
        Status::Fail => "FAIL",
        Status::Error => "ERR ",
    }
}

/// Convert an `hcl::Value` to `serde_json::Value`.
fn hcl_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::Value::Number(serde_json::Number::from(i))
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Null
            }
        }
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Array(arr) => serde_json::Value::Array(arr.iter().map(hcl_to_json).collect()),
        Value::Object(m) => {
            let map: serde_json::Map<String, serde_json::Value> =
                m.iter().map(|(k, v)| (k.clone(), hcl_to_json(v))).collect();
            serde_json::Value::Object(map)
        }
    }
}

fn status_str(status: &Status) -> &'static str {
    match status {
        Status::Pass => "pass",
        Status::Warn => "warn",
        Status::Fail => "fail",
        Status::Error => "error",
    }
}

/// Build the JSON array Value for a slice of results (unserialized).
pub fn results_to_json_value(results: &[CheckResult]) -> serde_json::Value {
    serde_json::Value::Array(
        results
            .iter()
            .map(|r| {
                let mut m = serde_json::Map::new();
                m.insert(
                    "name".to_string(),
                    serde_json::Value::String(r.name.clone()),
                );
                m.insert(
                    "status".to_string(),
                    serde_json::Value::String(status_str(&r.status).to_string()),
                );
                m.insert(
                    "detail".to_string(),
                    serde_json::Value::String(r.detail.clone()),
                );
                m.insert(
                    "sample".to_string(),
                    serde_json::Value::Array(r.sample.iter().map(hcl_to_json).collect()),
                );
                serde_json::Value::Object(m)
            })
            .collect(),
    )
}

/// Render results as a JSON array string.
pub fn render_json(results: &[CheckResult]) -> String {
    serde_json::to_string(&results_to_json_value(results))
        .expect("serializing results to JSON must not fail")
}

/// Render results in the requested format.
pub fn render_as(results: &[CheckResult], format: Format) -> String {
    match format {
        Format::Json => render_json(results),
        Format::Text => render(results),
        Format::Yaml => {
            let val = results_to_json_value(results);
            serde_yaml_ng::to_string(&val).unwrap_or_else(|e| format!("yaml error: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcl::{Map, Value};

    fn res(name: &str, status: Status, detail: &str) -> CheckResult {
        CheckResult {
            name: name.into(),
            status,
            detail: detail.into(),
            sample: vec![],
        }
    }

    fn res_with_sample(
        name: &str,
        status: Status,
        detail: &str,
        sample: Vec<Value>,
        _measure: Option<f64>,
    ) -> CheckResult {
        CheckResult {
            name: name.into(),
            status,
            detail: detail.into(),
            sample,
        }
    }

    fn obj(pairs: &[(&str, Value)]) -> Value {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v.clone());
        }
        Value::Object(m)
    }

    // --- parse_format ---

    #[test]
    fn parse_format_valid() {
        assert!(matches!(parse_format("json"), Some(Format::Json)));
        assert!(matches!(parse_format("yaml"), Some(Format::Yaml)));
        assert!(matches!(parse_format("text"), Some(Format::Text)));
    }

    #[test]
    fn parse_format_invalid() {
        assert!(parse_format("xml").is_none());
        assert!(parse_format("").is_none());
        assert!(parse_format("JSON").is_none());
    }

    // --- parse_status ---

    #[test]
    fn parse_status_valid() {
        assert!(matches!(parse_status("pass"), Some(Status::Pass)));
        assert!(matches!(parse_status("warn"), Some(Status::Warn)));
        assert!(matches!(parse_status("fail"), Some(Status::Fail)));
        assert!(matches!(parse_status("error"), Some(Status::Error)));
    }

    #[test]
    fn parse_status_invalid() {
        assert!(parse_status("ok").is_none());
        assert!(parse_status("").is_none());
        assert!(parse_status("PASS").is_none());
    }

    // --- http_status ---

    #[test]
    fn http_status_all_pass_is_200() {
        let results = vec![res("a", Status::Pass, ""), res("b", Status::Warn, "")];
        assert_eq!(http_status(&results), 200);
    }

    #[test]
    fn http_status_warn_only_is_200() {
        let results = vec![res("a", Status::Warn, "")];
        assert_eq!(http_status(&results), 200);
    }

    #[test]
    fn http_status_fail_is_503() {
        let results = vec![res("a", Status::Pass, ""), res("b", Status::Fail, "")];
        assert_eq!(http_status(&results), 503);
    }

    #[test]
    fn http_status_error_is_503() {
        let results = vec![res("a", Status::Error, "")];
        assert_eq!(http_status(&results), 503);
    }

    #[test]
    fn http_status_empty_is_200() {
        assert_eq!(http_status(&[]), 200);
    }

    // --- filter ---

    #[test]
    fn filter_by_status() {
        let results = vec![
            res("a", Status::Pass, ""),
            res("b", Status::Fail, ""),
            res("c", Status::Fail, ""),
        ];
        let fails = filter(&results, Some(Status::Fail), None);
        assert_eq!(fails.len(), 2);
        assert!(fails.iter().all(|r| matches!(r.status, Status::Fail)));
    }

    #[test]
    fn filter_by_limit() {
        let results = vec![
            res("a", Status::Pass, ""),
            res("b", Status::Pass, ""),
            res("c", Status::Pass, ""),
        ];
        let limited = filter(&results, None, Some(2));
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn filter_status_and_limit() {
        let results = vec![
            res("a", Status::Fail, ""),
            res("b", Status::Fail, ""),
            res("c", Status::Fail, ""),
        ];
        let filtered = filter(&results, Some(Status::Fail), Some(2));
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filter_no_match_returns_empty() {
        let results = vec![res("a", Status::Pass, "")];
        let filtered = filter(&results, Some(Status::Fail), None);
        assert!(filtered.is_empty());
    }

    // --- render_as ---

    #[test]
    fn render_as_json_nonempty_and_round_trips() {
        let results = vec![res("foo", Status::Pass, "1 row(s)")];
        let s = render_as(&results, Format::Json);
        assert!(!s.is_empty());
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr[0]["name"], "foo");
        assert_eq!(arr[0]["status"], "pass");
    }

    #[test]
    fn render_as_text_nonempty() {
        let results = vec![res("bar", Status::Warn, "3 row(s)")];
        let s = render_as(&results, Format::Text);
        assert!(!s.is_empty());
        assert!(s.contains("bar"));
    }

    #[test]
    fn render_as_yaml_nonempty_and_parses() {
        let results = vec![res("baz", Status::Fail, "5 row(s)")];
        let s = render_as(&results, Format::Yaml);
        assert!(!s.is_empty());
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&s).expect("valid YAML");
        // top-level should be a sequence
        assert!(parsed.is_sequence());
    }

    // --- existing render tests ---

    #[test]
    fn renders_status_and_name_per_check() {
        let out = render(&[
            res("orders_present", Status::Pass, "1 row(s)"),
            res("no_orphans", Status::Fail, "3 row(s)"),
        ]);
        assert!(out.contains("PASS"), "missing PASS: {out}");
        assert!(out.contains("orders_present"), "missing name: {out}");
        assert!(out.contains("FAIL"), "missing FAIL: {out}");
        assert!(out.contains("no_orphans"), "missing name: {out}");
    }

    #[test]
    fn report_renders_sample_rows_indented() {
        let sample = vec![
            obj(&[("id", Value::from(1_i64)), ("status", Value::from("bad"))]),
            obj(&[("id", Value::from(2_i64)), ("status", Value::from("worse"))]),
        ];
        let r = res_with_sample("orphans", Status::Fail, "2 row(s)", sample, None);
        let out = render(&[r]);

        assert!(out.contains("[FAIL]"), "missing FAIL line: {out}");
        assert!(out.contains("orphans"), "missing name: {out}");

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines.len(),
            3,
            "expected 3 lines (header + 2 sample rows): {out}"
        );
        assert!(
            lines[1].starts_with(' '),
            "sample line 1 should be indented: {:?}",
            lines[1]
        );
        assert!(
            lines[2].starts_with(' '),
            "sample line 2 should be indented: {:?}",
            lines[2]
        );
        assert!(
            lines[1].contains("id=") || lines[1].contains("status="),
            "sample content missing: {:?}",
            lines[1]
        );
    }

    #[test]
    fn report_pass_has_no_sample_lines() {
        let r = res("ok_check", Status::Pass, "1 row(s)");
        let out = render(&[r]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1, "pass result should render as 1 line: {out}");
    }

    #[test]
    fn render_json_round_trips() {
        let results = vec![
            res_with_sample(
                "foo",
                Status::Fail,
                "3 row(s)",
                vec![obj(&[("n", Value::from(42_i64))])],
                Some(42.5),
            ),
            res("bar", Status::Pass, "1 row(s)"),
        ];

        let json_str = render_json(&results);
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("render_json must produce valid JSON");

        let arr = parsed.as_array().expect("top-level must be array");
        assert_eq!(arr.len(), 2);

        let first = &arr[0];
        assert_eq!(first["name"], "foo");
        assert_eq!(first["status"], "fail");
        assert_eq!(first["detail"], "3 row(s)");
        let sample = first["sample"].as_array().expect("sample must be array");
        assert_eq!(sample.len(), 1);
        assert_eq!(sample[0]["n"], 42);

        let second = &arr[1];
        assert_eq!(second["name"], "bar");
        assert_eq!(second["status"], "pass");
        assert_eq!(second["sample"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn render_json_empty_results() {
        let json_str = render_json(&[]);
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed, serde_json::Value::Array(vec![]));
    }

    #[test]
    fn render_json_all_statuses() {
        let results = vec![
            res("a", Status::Pass, ""),
            res("b", Status::Warn, ""),
            res("c", Status::Fail, ""),
            res("d", Status::Error, ""),
        ];
        let json_str = render_json(&results);
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let statuses: Vec<&str> = parsed
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["status"].as_str().unwrap())
            .collect();
        assert_eq!(statuses, ["pass", "warn", "fail", "error"]);
    }
}
