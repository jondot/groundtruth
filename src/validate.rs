//! TFDV-style declarative data validation.
//!
//! [`validate_rows`] evaluates each [`ColumnRule`] over the query rows: any
//! violation → `Status::Fail`, any evaluation error → `Status::Error`. Stats
//! (IQR, z-score, Jarque-Bera) run on `Vec<f64>` — no Polars dependency.

use crate::config::{Check, ColumnRule, Validate};
use crate::runner::{CheckResult, Status};
use hcl::Value as HclValue;
use statrs::distribution::{ChiSquared, ContinuousCDF};

/// Evaluate all column rules against `rows` and return a single [`CheckResult`].
pub fn validate_rows(check: &Check, v: &Validate, rows: &[HclValue]) -> CheckResult {
    if rows.is_empty() {
        return ok(check, "0 row(s), no violations");
    }

    let mut all_violations: Vec<String> = Vec::new();
    let mut all_samples: Vec<HclValue> = Vec::new();
    let mut first_error: Option<String> = None;

    for rule in &v.columns {
        match check_column_rule(check, rule, rows) {
            RuleOutcome::Pass => {}
            RuleOutcome::Fail { detail, samples } => {
                all_violations.push(detail);
                all_samples.extend(samples);
            }
            RuleOutcome::Error(msg) => {
                if first_error.is_none() {
                    first_error = Some(msg);
                }
            }
        }
    }

    if let Some(e) = first_error {
        return CheckResult {
            name: check.name.clone(),
            status: Status::Error,
            detail: e,
            sample: vec![],
        };
    }

    if all_violations.is_empty() {
        ok(check, &format!("{} row(s), all rules passed", rows.len()))
    } else {
        let detail = all_violations.join("; ");
        let sample: Vec<HclValue> = all_samples.into_iter().take(10).collect();
        CheckResult {
            name: check.name.clone(),
            status: Status::Fail,
            detail,
            sample,
        }
    }
}

enum RuleOutcome {
    Pass,
    Fail {
        detail: String,
        samples: Vec<HclValue>,
    },
    Error(String),
}

fn check_column_rule(check: &Check, rule: &ColumnRule, rows: &[HclValue]) -> RuleOutcome {
    let column_exists = rows.iter().any(|row| {
        if let HclValue::Object(obj) = row {
            obj.contains_key(&rule.name)
        } else {
            false
        }
    });

    if !column_exists {
        return RuleOutcome::Error(format!(
            "column {:?} not found in query result (check {:?})",
            rule.name, check.name
        ));
    }

    let col_values: Vec<&HclValue> = rows
        .iter()
        .map(|row| match row {
            HclValue::Object(obj) => obj.get(&rule.name).unwrap_or(&HclValue::Null),
            _ => &HclValue::Null,
        })
        .collect();

    let mut violations: Vec<String> = Vec::new();
    let mut bad_row_indices: Vec<usize> = Vec::new();

    // type
    if let Some(expected_type) = &rule.type_ {
        let mut bad_indices: Vec<usize> = Vec::new();
        for (i, v) in col_values.iter().enumerate() {
            if **v == HclValue::Null {
                continue;
            }
            let conforms = match expected_type.as_str() {
                "int" => is_int(v),
                "float" => is_float(v),
                "string" => matches!(v, HclValue::String(_)),
                "bool" => matches!(v, HclValue::Bool(_)),
                "timestamp" => is_timestamp(v),
                _ => true,
            };
            if !conforms {
                bad_indices.push(i);
            }
        }
        if !bad_indices.is_empty() {
            violations.push(format!(
                "column {:?}: type={}: {} value(s) non-conforming",
                rule.name,
                expected_type,
                bad_indices.len()
            ));
            bad_row_indices.extend_from_slice(&bad_indices);
        }
    }

    // not_null
    if rule.not_null == Some(true) {
        let null_count = col_values.iter().filter(|v| ***v == HclValue::Null).count();
        if null_count > 0 {
            violations.push(format!(
                "column {:?}: not_null: {} null value(s) found",
                rule.name, null_count
            ));
            for (i, v) in col_values.iter().enumerate() {
                if **v == HclValue::Null {
                    bad_row_indices.push(i);
                }
            }
        }
    }

    // null_rate
    if let Some(threshold) = rule.null_rate {
        let null_count = col_values.iter().filter(|v| ***v == HclValue::Null).count();
        let rate = null_count as f64 / col_values.len() as f64;
        if rate > threshold {
            violations.push(format!(
                "column {:?}: null_rate: {:.2}% nulls exceeds threshold {:.2}%",
                rule.name,
                rate * 100.0,
                threshold * 100.0
            ));
        }
    }

    // allowed
    if let Some(allowed_vals) = &rule.allowed {
        let mut bad_indices: Vec<usize> = Vec::new();
        for (i, v) in col_values.iter().enumerate() {
            if **v == HclValue::Null {
                continue;
            }
            let s = hcl_value_to_string(v);
            if !allowed_vals.contains(&s) {
                bad_indices.push(i);
            }
        }
        if !bad_indices.is_empty() {
            violations.push(format!(
                "column {:?}: allowed: {} value(s) not in allowed set {:?}",
                rule.name,
                bad_indices.len(),
                allowed_vals
            ));
            bad_row_indices.extend_from_slice(&bad_indices);
        }
    }

    // matches
    if let Some(re) = &rule.matches {
        let mut bad_indices: Vec<usize> = Vec::new();
        for (i, v) in col_values.iter().enumerate() {
            if **v == HclValue::Null {
                continue;
            }
            let s = hcl_value_to_string(v);
            if !re.is_match(&s) {
                bad_indices.push(i);
            }
        }
        if !bad_indices.is_empty() {
            violations.push(format!(
                "column {:?}: matches: {} value(s) do not match pattern {:?}",
                rule.name,
                bad_indices.len(),
                re.as_str()
            ));
            bad_row_indices.extend_from_slice(&bad_indices);
        }
    }

    // range
    if let Some((min, max)) = rule.range {
        let mut bad_indices: Vec<usize> = Vec::new();
        for (i, v) in col_values.iter().enumerate() {
            if **v == HclValue::Null {
                continue;
            }
            match hcl_value_to_f64(v) {
                Some(f) => {
                    if f < min || f > max {
                        bad_indices.push(i);
                    }
                }
                None => {
                    bad_indices.push(i);
                }
            }
        }
        if !bad_indices.is_empty() {
            violations.push(format!(
                "column {:?}: range=[{min},{max}]: {} value(s) out of range",
                rule.name,
                bad_indices.len()
            ));
            bad_row_indices.extend_from_slice(&bad_indices);
        }
    }

    // unique
    if rule.unique == Some(true) {
        let non_null: Vec<String> = col_values
            .iter()
            .filter(|v| ***v != HclValue::Null)
            .map(|v| hcl_value_to_string(v))
            .collect();
        let total = non_null.len();
        let mut seen = std::collections::HashSet::new();
        let mut dup_count = 0usize;
        for s in &non_null {
            if !seen.insert(s.as_str()) {
                dup_count += 1;
            }
        }
        if dup_count > 0 {
            violations.push(format!(
                "column {:?}: unique: {} duplicate(s) found in {} non-null values",
                rule.name, dup_count, total
            ));
        }
    }

    // outliers
    if let Some(method) = &rule.outliers {
        let outlier_indices: Vec<usize> = match method.as_str() {
            "iqr" => find_iqr_outliers(&col_values),
            "zscore" => find_zscore_outliers(&col_values),
            _ => vec![],
        };

        if !outlier_indices.is_empty() {
            violations.push(format!(
                "column {:?}: outliers={}: {} outlier(s) found",
                rule.name,
                method,
                outlier_indices.len()
            ));
            bad_row_indices.extend_from_slice(&outlier_indices);
        }
    }

    // distribution
    if let Some(dist) = &rule.distribution {
        let nums: Vec<f64> = col_values
            .iter()
            .filter(|v| ***v != HclValue::Null)
            .filter_map(|v| hcl_value_to_f64(v))
            .collect();

        if dist.as_str() == "normal" {
            if nums.len() < 8 {
                return RuleOutcome::Error(format!(
                    "column {:?}: distribution=normal: normality test needs >= 8 non-null values, got {}",
                    rule.name,
                    nums.len()
                ));
            }
            match jarque_bera_test(&nums) {
                Ok(p_value) => {
                    if p_value < 0.05 {
                        violations.push(format!(
                            "column {:?}: distribution=normal: Jarque-Bera p={:.4} < 0.05 (not normal)",
                            rule.name, p_value
                        ));
                    }
                }
                Err(e) => {
                    return RuleOutcome::Error(format!(
                        "column {:?}: distribution=normal: {e}",
                        rule.name
                    ));
                }
            }
        }
    }

    if violations.is_empty() {
        return RuleOutcome::Pass;
    }

    // Dedup bad-row indices, take up to 10 samples.
    let mut seen_indices: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for i in bad_row_indices {
        if i < rows.len() {
            seen_indices.insert(i);
        }
    }
    let samples: Vec<HclValue> = seen_indices
        .into_iter()
        .take(10)
        .map(|i| rows[i].clone())
        .collect();

    RuleOutcome::Fail {
        detail: violations.join(", "),
        samples,
    }
}

fn is_int(v: &HclValue) -> bool {
    match v {
        HclValue::Number(n) => {
            // Exact i64, or a float with no fractional part.
            n.as_i64().is_some() || n.as_f64().map(|f| f.fract() == 0.0).unwrap_or(false)
        }
        _ => false,
    }
}

fn is_float(v: &HclValue) -> bool {
    matches!(v, HclValue::Number(_))
}

fn is_timestamp(v: &HclValue) -> bool {
    match v {
        HclValue::String(s) => {
            chrono::DateTime::parse_from_rfc3339(s).is_ok()
                || chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").is_ok()
                || chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").is_ok()
        }
        _ => false,
    }
}

fn hcl_value_to_f64(v: &HclValue) -> Option<f64> {
    match v {
        HclValue::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn hcl_value_to_string(v: &HclValue) -> String {
    match v {
        HclValue::String(s) => s.clone(),
        HclValue::Number(n) => n.to_string(),
        HclValue::Bool(b) => b.to_string(),
        HclValue::Null => "null".to_string(),
        HclValue::Array(_) => "[array]".to_string(),
        HclValue::Object(_) => "{object}".to_string(),
    }
}

// IQR outliers: outside [Q1 - 1.5*IQR, Q3 + 1.5*IQR].
fn find_iqr_outliers(col_values: &[&HclValue]) -> Vec<usize> {
    let nums: Vec<f64> = col_values
        .iter()
        .filter(|v| ***v != HclValue::Null)
        .filter_map(|v| hcl_value_to_f64(v))
        .collect();

    if nums.len() < 4 {
        return vec![];
    }

    let mut sorted = nums.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let q1 = percentile(&sorted, 0.25);
    let q3 = percentile(&sorted, 0.75);
    let iqr = q3 - q1;
    let lower = q1 - 1.5 * iqr;
    let upper = q3 + 1.5 * iqr;

    let mut bad_indices = Vec::new();
    for (i, v) in col_values.iter().enumerate() {
        if **v == HclValue::Null {
            continue;
        }
        if let Some(f) = hcl_value_to_f64(v)
            && (f < lower || f > upper)
        {
            bad_indices.push(i);
        }
    }
    bad_indices
}

// Z-score outliers: |x - mean| / sd > 3.
fn find_zscore_outliers(col_values: &[&HclValue]) -> Vec<usize> {
    let nums: Vec<f64> = col_values
        .iter()
        .filter(|v| ***v != HclValue::Null)
        .filter_map(|v| hcl_value_to_f64(v))
        .collect();

    if nums.len() < 3 {
        return vec![];
    }

    let n = nums.len() as f64;
    let mean = nums.iter().sum::<f64>() / n;
    let variance = nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    let sd = variance.sqrt();

    if sd == 0.0 {
        return vec![];
    }

    let mut bad_indices = Vec::new();
    for (i, v) in col_values.iter().enumerate() {
        if **v == HclValue::Null {
            continue;
        }
        if let Some(f) = hcl_value_to_f64(v)
            && ((f - mean) / sd).abs() > 3.0
        {
            bad_indices.push(i);
        }
    }
    bad_indices
}

// Percentile via linear interpolation.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if n == 1 {
        return sorted[0];
    }
    let idx = p * (n - 1) as f64;
    let lo = idx.floor() as usize;
    let hi = idx.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let frac = idx - lo as f64;
        sorted[lo] * (1.0 - frac) + sorted[hi] * frac
    }
}

// Jarque-Bera normality test: JB = n/6 * (skew^2 + excess_kurtosis^2/4),
// p = 1 - ChiSquared(2).cdf(JB); violation if p < 0.05.
fn jarque_bera_test(nums: &[f64]) -> Result<f64, String> {
    let n = nums.len() as f64;
    if n < 8.0 {
        return Err(format!(
            "normality test needs >= 8 values, got {}",
            nums.len()
        ));
    }

    let mean = nums.iter().sum::<f64>() / n;
    let m2 = nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    let m3 = nums.iter().map(|x| (x - mean).powi(3)).sum::<f64>() / n;
    let m4 = nums.iter().map(|x| (x - mean).powi(4)).sum::<f64>() / n;

    if m2 == 0.0 {
        return Err("all values are identical; cannot compute normality test".to_string());
    }

    let skewness = m3 / m2.powf(1.5);
    let excess_kurtosis = m4 / m2.powi(2) - 3.0;

    let jb = (n / 6.0) * (skewness.powi(2) + excess_kurtosis.powi(2) / 4.0);

    let chi2 = ChiSquared::new(2.0).map_err(|e| format!("chi-squared distribution error: {e}"))?;
    let p_value = 1.0 - chi2.cdf(jb);

    Ok(p_value)
}

fn ok(check: &Check, detail: &str) -> CheckResult {
    CheckResult {
        name: check.name.clone(),
        status: Status::Pass,
        detail: detail.to_string(),
        sample: vec![],
    }
}
