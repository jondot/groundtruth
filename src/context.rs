//! Builds the eval context for a check's `when`.
//!
//! Exposes `row` (first row's columns as fields), `rows` (`{ count, sample }`),
//! plus the condition functions (`duration`, `age`, `env`).

use hcl::eval::Context;
use hcl::{Map, Value};

/// Build a fresh evaluation context from a check's result rows.
pub fn build_context(rows: &[Value]) -> Context<'static> {
    let mut ctx = Context::new();

    let row = rows.first().cloned().unwrap_or(Value::Null);
    ctx.declare_var("row", row);

    let sample: Vec<Value> = rows.iter().take(10).cloned().collect();
    let mut rows_obj = Map::new();
    rows_obj.insert("count".to_string(), Value::from(rows.len() as i64));
    rows_obj.insert("sample".to_string(), Value::Array(sample));
    ctx.declare_var("rows", Value::Object(rows_obj));

    crate::functions::register(&mut ctx);
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcl::eval::Evaluate;

    fn obj(pairs: &[(&str, Value)]) -> Value {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v.clone());
        }
        Value::Object(m)
    }

    fn eval(src: &str, ctx: &Context) -> Value {
        let body = hcl::parse(&format!("x = {src}")).unwrap();
        body.attributes()
            .next()
            .unwrap()
            .expr
            .evaluate(ctx)
            .unwrap()
    }

    #[test]
    fn row_exposes_first_rows_columns() {
        let ctx = build_context(&[obj(&[("recent", 5.into())])]);
        assert_eq!(eval("row.recent == 5", &ctx), Value::Bool(true));
    }

    #[test]
    fn rows_count_reflects_number_of_rows() {
        let ctx = build_context(&[obj(&[("id", 1.into())]), obj(&[("id", 2.into())])]);
        assert_eq!(eval("rows.count == 2", &ctx), Value::Bool(true));
    }

    #[test]
    fn duration_function_converts_to_seconds() {
        let ctx = build_context(&[]);
        assert_eq!(eval("duration(\"30m\") == 1800", &ctx), Value::Bool(true));
    }
}
