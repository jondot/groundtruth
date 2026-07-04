//! Renders check results as Prometheus text exposition format.

use crate::runner::{CheckResult, Status};

fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn status_code(s: &Status) -> u8 {
    match s {
        Status::Pass => 0,
        Status::Warn => 1,
        Status::Fail => 2,
        Status::Error => 3,
    }
}

fn is_up(s: &Status) -> u8 {
    match s {
        Status::Pass | Status::Warn => 1,
        Status::Fail | Status::Error => 0,
    }
}

/// Render results as Prometheus exposition format text.
pub fn render(results: &[CheckResult]) -> String {
    let mut out = String::new();

    out.push_str("# HELP groundtruth_check_status Check status: 0=pass 1=warn 2=fail 3=error\n");
    out.push_str("# TYPE groundtruth_check_status gauge\n");
    for r in results {
        out.push_str(&format!(
            "groundtruth_check_status{{check=\"{}\"}} {}\n",
            escape_label(&r.name),
            status_code(&r.status)
        ));
    }

    out.push_str("# HELP groundtruth_check_up 1 if check is passing or warning, 0 otherwise\n");
    out.push_str("# TYPE groundtruth_check_up gauge\n");
    for r in results {
        out.push_str(&format!(
            "groundtruth_check_up{{check=\"{}\"}} {}\n",
            escape_label(&r.name),
            is_up(&r.status)
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(name: &str, status: Status) -> CheckResult {
        CheckResult {
            name: name.into(),
            status,
            detail: String::new(),
            sample: vec![],
        }
    }

    #[test]
    fn one_help_and_type_per_metric() {
        let out = render(&[res("x", Status::Pass)]);
        assert_eq!(out.matches("# HELP groundtruth_check_status").count(), 1);
        assert_eq!(
            out.matches("# TYPE groundtruth_check_status gauge").count(),
            1
        );
        assert_eq!(out.matches("# HELP groundtruth_check_up").count(), 1);
        assert_eq!(out.matches("# TYPE groundtruth_check_up gauge").count(), 1);
    }

    #[test]
    fn pass_status_0_up_1() {
        let out = render(&[res("db", Status::Pass)]);
        assert!(
            out.contains("groundtruth_check_status{check=\"db\"} 0"),
            "got:\n{out}"
        );
        assert!(
            out.contains("groundtruth_check_up{check=\"db\"} 1"),
            "got:\n{out}"
        );
    }

    #[test]
    fn warn_status_1_up_1() {
        let out = render(&[res("db", Status::Warn)]);
        assert!(
            out.contains("groundtruth_check_status{check=\"db\"} 1"),
            "got:\n{out}"
        );
        assert!(
            out.contains("groundtruth_check_up{check=\"db\"} 1"),
            "got:\n{out}"
        );
    }

    #[test]
    fn fail_status_2_up_0() {
        let out = render(&[res("db", Status::Fail)]);
        assert!(
            out.contains("groundtruth_check_status{check=\"db\"} 2"),
            "got:\n{out}"
        );
        assert!(
            out.contains("groundtruth_check_up{check=\"db\"} 0"),
            "got:\n{out}"
        );
    }

    #[test]
    fn error_status_3_up_0() {
        let out = render(&[res("db", Status::Error)]);
        assert!(
            out.contains("groundtruth_check_status{check=\"db\"} 3"),
            "got:\n{out}"
        );
        assert!(
            out.contains("groundtruth_check_up{check=\"db\"} 0"),
            "got:\n{out}"
        );
    }

    #[test]
    fn label_value_with_quote_is_escaped() {
        let out = render(&[res("say \"hi\"", Status::Pass)]);
        assert!(
            out.contains(r#"groundtruth_check_status{check="say \"hi\""}"#),
            "got:\n{out}"
        );
    }

    #[test]
    fn trailing_newline() {
        let out = render(&[res("x", Status::Pass)]);
        assert!(out.ends_with('\n'));
    }
}
