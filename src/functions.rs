//! HCL functions for check `when` expressions; install them via [`register`].

use chrono::Utc;
use hcl::Value;
use hcl::eval::{Context, FuncArgs, FuncDef, ParamType};

/// Declare all groundtruth functions into an eval context.
pub fn register(ctx: &mut Context<'static>) {
    ctx.declare_func(
        "duration",
        FuncDef::builder()
            .param(ParamType::String)
            .build(f_duration),
    );
    ctx.declare_func("age", FuncDef::builder().param(ParamType::Any).build(f_age));
    ctx.declare_func(
        "env",
        FuncDef::builder().param(ParamType::String).build(f_env),
    );
}

// duration("30m") -> seconds (f64)
fn f_duration(args: FuncArgs) -> Result<Value, String> {
    parse_duration(args[0].as_str().unwrap_or("")).map(Value::from)
}

fn parse_duration(s: &str) -> Result<f64, String> {
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: f64 = num.parse().map_err(|_| format!("bad duration: {s:?}"))?;
    match unit {
        "s" => Ok(n),
        "m" => Ok(n * 60.0),
        "h" => Ok(n * 3600.0),
        "d" => Ok(n * 86400.0),
        _ => Err(format!("unknown duration unit in {s:?} (use s/m/h/d)")),
    }
}

// age(ts) -> seconds since now; ts is an RFC3339 string or unix epoch number.
fn f_age(args: FuncArgs) -> Result<Value, String> {
    let now = Utc::now().timestamp_millis() as f64 / 1000.0;
    let then: f64 = match &args[0] {
        Value::String(s) => {
            let dt = chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|e| format!("bad timestamp {s:?}: {e}"))?;
            dt.timestamp_millis() as f64 / 1000.0
        }
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| "numeric timestamp out of range".to_string())?,
        other => return Err(format!("age() expects a string or number, got {other:?}")),
    };
    Ok(Value::from(now - then))
}

// env("NAME") -> string; missing var => Err
fn f_env(args: FuncArgs) -> Result<Value, String> {
    let name = args[0].as_str().unwrap_or("");
    std::env::var(name)
        .map(Value::String)
        .map_err(|_| format!("env var {name:?} not set"))
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use hcl::eval::Evaluate;

    fn eval_ctx(src: &str, ctx: &Context) -> Result<Value, hcl::eval::Error> {
        let body = hcl::parse(&format!("x = {src}")).unwrap();
        body.attributes().next().unwrap().expr.evaluate(ctx)
    }

    fn ctx() -> Context<'static> {
        let mut c = Context::new();
        register(&mut c);
        c
    }

    #[test]
    fn duration_30m_is_1800() {
        let c = ctx();
        assert_eq!(
            eval_ctx("duration(\"30m\")", &c).unwrap(),
            Value::from(1800.0)
        );
    }

    #[test]
    fn duration_bad_unit_errors() {
        let c = ctx();
        assert!(eval_ctx("duration(\"30x\")", &c).is_err());
    }

    #[test]
    fn age_rfc3339_string_approx() {
        let ts = (Utc::now() - chrono::Duration::seconds(5))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let c = ctx();
        let val = eval_ctx(&format!("age(\"{ts}\")"), &c).unwrap();
        let secs = val.as_f64().unwrap();
        assert!((4.0..=10.0).contains(&secs), "age was {secs}");
    }

    #[test]
    fn age_epoch_number_works() {
        let c = ctx();
        let val = eval_ctx("age(0)", &c).unwrap();
        let secs = val.as_f64().unwrap();
        assert!(secs > 1_000_000_000.0, "age of epoch 0 was {secs}");
    }

    #[test]
    fn env_reads_set_var() {
        // SAFETY: single-threaded test; no other thread reads this var.
        unsafe { std::env::set_var("GROUNDTRUTH_TEST_VAR", "hello") };
        let c = ctx();
        let val = eval_ctx("env(\"GROUNDTRUTH_TEST_VAR\")", &c).unwrap();
        assert_eq!(val, Value::String("hello".to_string()));
    }

    #[test]
    fn env_missing_var_errors() {
        // SAFETY: single-threaded test; no other thread reads this var.
        unsafe { std::env::remove_var("GROUNDTRUTH_MISSING_XYZ_VAR") };
        let c = ctx();
        assert!(eval_ctx("env(\"GROUNDTRUTH_MISSING_XYZ_VAR\")", &c).is_err());
    }
}
