//! Evaluates a check's `when` into pass / fail / error.
//! A non-boolean, typo, or panic is ERROR — never silently a pass.

use hcl::eval::{Context, Evaluate};
use hcl::expr::{BinaryOperator, Operation};
use hcl::{Expression, Value};

/// The result of evaluating one `warn`/`fail` condition.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail,
    Error(String),
}

/// Evaluate a `when` expression against an injected context.
///
/// Panic-safe: a poison expression degrades to [`Outcome::Error`] rather than
/// killing the daemon. Division/modulo by zero is also surfaced as an error:
/// hcl-rs saturates `x / 0` to `f64::MAX` instead of producing infinity or
/// panicking, which would otherwise let a broken expression slip through as a
/// silent pass (`1.0 / 0.0 < 1` → false → PASS). Fail loud, never silently.
pub fn evaluate_when(expr: &Expression, ctx: &Context) -> Outcome {
    if let Some(msg) = find_division_by_zero(expr, ctx) {
        return Outcome::Error(msg);
    }
    let evaluated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| expr.evaluate(ctx)));
    match evaluated {
        Ok(Ok(Value::Bool(true))) => Outcome::Fail,
        Ok(Ok(Value::Bool(false))) => Outcome::Pass,
        Ok(Ok(other)) => Outcome::Error(format!("`when` returned {other:?}, expected a boolean")),
        Ok(Err(e)) => Outcome::Error(e.to_string()),
        Err(_) => Outcome::Error("expression panicked while evaluating".into()),
    }
}

/// Walk the expression tree; if any `/` or `%` has a divisor that evaluates to
/// zero, return an error message. hcl-rs would otherwise saturate the result to
/// `f64::MAX` and silently continue.
fn find_division_by_zero(expr: &Expression, ctx: &Context) -> Option<String> {
    match expr {
        Expression::Operation(op) => match op.as_ref() {
            Operation::Binary(b) => {
                if matches!(b.operator, BinaryOperator::Div | BinaryOperator::Mod)
                    && evaluates_to_zero(&b.rhs_expr, ctx)
                {
                    let sym = if b.operator == BinaryOperator::Div {
                        "/"
                    } else {
                        "%"
                    };
                    return Some(format!("division by zero (`{sym}` with a zero divisor)"));
                }
                find_division_by_zero(&b.lhs_expr, ctx)
                    .or_else(|| find_division_by_zero(&b.rhs_expr, ctx))
            }
            Operation::Unary(u) => find_division_by_zero(&u.expr, ctx),
        },
        Expression::Parenthesis(inner) => find_division_by_zero(inner, ctx),
        Expression::Conditional(c) => find_division_by_zero(&c.cond_expr, ctx)
            .or_else(|| find_division_by_zero(&c.true_expr, ctx))
            .or_else(|| find_division_by_zero(&c.false_expr, ctx)),
        _ => None,
    }
}

/// True if `expr` evaluates to a numeric zero. Panic-safe; a failing or
/// non-numeric sub-expression is simply not "zero" and is left for the main
/// evaluation pass to report.
fn evaluates_to_zero(expr: &Expression, ctx: &Context) -> bool {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| expr.evaluate(ctx)));
    matches!(r, Ok(Ok(Value::Number(n))) if n.as_f64() == Some(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcl::eval::Context;

    /// Parse `x = <src>` and return the bare expression, as the engine holds it.
    fn expr(src: &str) -> Expression {
        let body = hcl::parse(&format!("x = {src}")).expect("parse expr");
        body.attributes().next().unwrap().expr.clone()
    }

    #[test]
    fn true_condition_fails() {
        let ctx = Context::new();
        assert_eq!(evaluate_when(&expr("true"), &ctx), Outcome::Fail);
    }

    #[test]
    fn false_condition_passes() {
        let ctx = Context::new();
        assert_eq!(evaluate_when(&expr("false"), &ctx), Outcome::Pass);
    }

    #[test]
    fn non_boolean_is_an_error_not_a_pass() {
        // A numeric `when` is a misconfiguration, not a Pass.
        let ctx = Context::new();
        assert!(matches!(
            evaluate_when(&expr("1 + 1"), &ctx),
            Outcome::Error(_)
        ));
    }

    #[test]
    fn undefined_reference_is_an_error_not_a_pass() {
        // A typo'd column reference must surface, never silently pass.
        let ctx = Context::new();
        assert!(matches!(
            evaluate_when(&expr("row.missing == 0"), &ctx),
            Outcome::Error(_)
        ));
    }

    #[test]
    fn division_by_zero_is_an_error_not_a_pass() {
        // hcl-rs saturates `x / 0` to f64::MAX (no panic, no infinity), so the
        // comparison would otherwise evaluate normally. Both the FAIL-leaning and
        // the PASS-leaning forms must surface as ERROR — never a fake pass.
        let ctx = Context::new();
        assert!(matches!(
            evaluate_when(&expr("1.0 / 0.0 > 1"), &ctx),
            Outcome::Error(_)
        ));
        assert!(matches!(
            evaluate_when(&expr("1.0 / 0.0 < 1"), &ctx),
            Outcome::Error(_)
        ));
        // Integer division by zero too.
        assert!(matches!(
            evaluate_when(&expr("1 / 0 > 1"), &ctx),
            Outcome::Error(_)
        ));
    }

    #[test]
    fn modulo_by_zero_is_an_error() {
        let ctx = Context::new();
        assert!(matches!(
            evaluate_when(&expr("10 % 0 == 0"), &ctx),
            Outcome::Error(_)
        ));
    }

    #[test]
    fn nonzero_division_still_evaluates_normally() {
        let ctx = Context::new();
        assert_eq!(evaluate_when(&expr("10 / 2 > 1"), &ctx), Outcome::Fail);
        assert_eq!(evaluate_when(&expr("10 / 2 < 1"), &ctx), Outcome::Pass);
    }
}
