//! Parses `.hcl` into the check graph. `warn`/`fail` expressions stay
//! un-evaluated until after the query runs (see [`crate::eval`]).

use anyhow::{Context as _, Result, anyhow, bail};
use hcl::eval::Evaluate;
use hcl::{Expression, TraversalOperator};

/// A declarative validation block on a check.
#[derive(Debug, Clone)]
pub struct Validate {
    pub columns: Vec<ColumnRule>,
}

/// Per-column validation rules. All fields are optional.
#[derive(Debug, Clone)]
pub struct ColumnRule {
    /// The column name as it appears in the query result.
    pub name: String,
    /// Expected type: "int", "float", "string", "bool", "timestamp".
    pub type_: Option<String>,
    /// All non-null values must be present (null_count == 0).
    pub not_null: Option<bool>,
    /// null_count / len <= threshold.
    pub null_rate: Option<f64>,
    /// Non-null values must all appear in this set.
    pub allowed: Option<Vec<String>>,
    /// Non-null string values must match this pre-compiled regex.
    pub matches: Option<regex::Regex>,
    /// Non-null numeric values must be in [min, max] (inclusive).
    pub range: Option<(f64, f64)>,
    /// All non-null values must be unique.
    pub unique: Option<bool>,
    /// Outlier detection method: "iqr" or "zscore".
    pub outliers: Option<String>,
    /// Distribution test: "normal" (Jarque-Bera).
    pub distribution: Option<String>,
}

/// A named database connection, parsed from a `connection "<provider>" "<name>"` block.
#[derive(Debug)]
pub struct Connection {
    pub name: String,
    pub config: ConnConfig,
}

/// Connection configuration for a (read-only) check source. Most engines are
/// connectorx-backed; Athena has its own AWS-SDK source. The only writable
/// database is the state store (see `StateConfig`).
#[derive(Debug)]
pub enum ConnConfig {
    ConnectorX(CxConfig),
    Athena(AthenaConfig),
}

/// A connectorx-backed connection. `provider` is the (cosmetic) block label;
/// the `dsn` scheme is what actually selects the backend — connectorx routes it.
#[derive(Debug)]
pub struct CxConfig {
    pub provider: String,
    pub dsn: String,
}

/// An Amazon Athena connection. Credentials resolve via the standard AWS chain.
#[derive(Debug)]
pub struct AthenaConfig {
    pub region: String,
    pub database: String,
    /// S3 URI for query results. Optional if the workgroup sets a default.
    pub output_location: Option<String>,
    /// Athena workgroup. Optional (defaults to `primary`).
    pub workgroup: Option<String>,
}

/// Parse every `connection "<provider>" "<name>" { ... }` block.
pub fn parse_connections(src: &str) -> Result<Vec<Connection>> {
    let body = hcl::parse(src).context("parsing HCL")?;
    let mut conns = Vec::new();

    for block in body.blocks().filter(|b| b.identifier() == "connection") {
        let provider = label(block, 0, "provider")?;
        let name = label(block, 1, "name")?;

        // Evaluate a string attr with functions registered (so `env("VAR")` works in dsn/path).
        let eval_str_attr = |key: &str| -> Result<Option<String>> {
            match block.body.attributes().find(|a| a.key.as_str() == key) {
                None => Ok(None),
                Some(attr) => {
                    let mut ctx = hcl::eval::Context::new();
                    crate::functions::register(&mut ctx);
                    match attr.expr.evaluate(&ctx) {
                        Ok(hcl::Value::String(s)) => Ok(Some(s)),
                        Ok(other) => anyhow::bail!(
                            "connection {:?} {:?}: `{key}` must evaluate to a string, got {other:?}",
                            provider,
                            name
                        ),
                        Err(e) => anyhow::bail!(
                            "connection {:?} {:?}: evaluating `{key}`: {e}",
                            provider,
                            name
                        ),
                    }
                }
            }
        };

        let config = if provider == "athena" {
            // Athena isn't a dsn/connectorx engine — it takes AWS coordinates.
            let region = eval_str_attr("region")?.ok_or_else(|| {
                anyhow::anyhow!("connection \"athena\" \"{name}\": missing `region`")
            })?;
            let database = eval_str_attr("database")?.ok_or_else(|| {
                anyhow::anyhow!("connection \"athena\" \"{name}\": missing `database`")
            })?;
            let output_location = eval_str_attr("output_location")?;
            let workgroup = eval_str_attr("workgroup")?;
            for attr in block.body.attributes() {
                match attr.key.as_str() {
                    "region" | "database" | "output_location" | "workgroup" => {}
                    other => {
                        anyhow::bail!("connection \"athena\" \"{name}\": unknown field `{other}`")
                    }
                }
            }
            ConnConfig::Athena(AthenaConfig {
                region,
                database,
                output_location,
                workgroup,
            })
        } else {
            let dsn = eval_str_attr("dsn")?.ok_or_else(|| {
                anyhow::anyhow!("connection \"{provider}\" \"{name}\": missing `dsn`")
            })?;
            for attr in block.body.attributes() {
                match attr.key.as_str() {
                    "dsn" => {}
                    other => anyhow::bail!(
                        "connection \"{provider}\" \"{name}\": unknown field `{other}`"
                    ),
                }
            }
            // No hardcoded provider list: connectorx routes by the dsn scheme and
            // is the source of truth. Validate here so `gt check` catches a
            // bad/unsupported scheme up front.
            crate::cx::validate_connection_url(&dsn)
                .with_context(|| format!("connection \"{provider}\" \"{name}\""))?;
            ConnConfig::ConnectorX(CxConfig {
                provider: provider.clone(),
                dsn,
            })
        };
        conns.push(Connection { name, config });
    }

    Ok(conns)
}

fn label(block: &hcl::Block, i: usize, what: &str) -> Result<String> {
    block
        .labels()
        .get(i)
        .map(|l| l.as_str().to_string())
        .ok_or_else(|| anyhow!("connection block is missing its {what} label"))
}

/// One monitored check.
#[derive(Debug)]
pub struct Check {
    pub name: String,
    /// Connection name to query against. `None` falls back to the first connection.
    pub on: Option<String>,
    /// Schedule interval, e.g. "5m". Parsed later.
    pub every: Option<String>,
    /// Per-query timeout, e.g. "10s". Parsed at run time.
    pub timeout: Option<String>,
    /// SQL, un-evaluated so `${each.value}` templates resolve against a context later.
    pub query: Expression,
    /// Warning tier — fires when `when` is true.
    pub warn: Option<Tier>,
    /// Failure tier — fires when `when` is true.
    pub fail: Option<Tier>,
    /// Current item when expanded from a `for_each` list.
    pub each_value: Option<String>,
    /// Notifier to alert on failure (last segment of a traversal, or a bare string).
    pub on_fail: Option<String>,
    /// Declarative validation rules. Mutually exclusive with `warn`/`fail`.
    pub validate: Option<Validate>,
}

/// A threshold tier (warn or fail).
#[derive(Debug)]
pub struct Tier {
    pub when: Expression,
    /// How long the condition must hold before firing, e.g. "15m".
    pub sustained: Option<String>,
    /// Number of failing-row samples to capture.
    pub sample: Option<usize>,
}

/// Allowed attributes on a `check` block (besides the name label itself).
const CHECK_ATTRS: &[&str] = &[
    "on", "every", "timeout", "query", "warn", "fail", "for_each", "on_fail",
];
/// Allowed sub-blocks inside a `check` block.
const CHECK_BLOCKS: &[&str] = &["warn", "fail", "validate"];
/// Allowed attributes inside a `warn`/`fail` block.
const TIER_ATTRS: &[&str] = &["when", "sustained", "sample"];
/// Allowed attributes inside a `column "<name>" { ... }` block within `validate`.
const COLUMN_RULE_ATTRS: &[&str] = &[
    "type",
    "not_null",
    "null_rate",
    "allowed",
    "matches",
    "range",
    "unique",
    "outliers",
    "distribution",
];

/// Top-level defaults applied to checks lacking their own `on`/`every`/`on_fail`/`timeout`.
struct Defaults {
    on: Option<String>,
    every: Option<String>,
    timeout: Option<String>,
    on_fail: Option<String>,
}

/// Parse every `check "<name>" { ... }` block in the document, applying
/// `defaults { ... }` and expanding `for_each`.
pub fn parse_checks(src: &str) -> Result<Vec<Check>> {
    let body = hcl::parse(src).context("parsing HCL")?;

    let defaults = parse_defaults(&body)?;

    let mut checks = Vec::new();

    for block in body.blocks().filter(|b| b.identifier() == "check") {
        let name = block
            .labels()
            .first()
            .ok_or_else(|| anyhow!("check block is missing a name label"))?
            .as_str()
            .to_string();

        // Reject unknown attributes (a typo'd key must fail loudly, not be ignored).
        for attr in block.body.attributes() {
            let key = attr.key.as_str();
            if !CHECK_ATTRS.contains(&key) {
                bail!("check {:?}: unknown attribute `{key}`", name);
            }
        }
        for sub in block.body.blocks() {
            let id = sub.identifier();
            if !CHECK_BLOCKS.contains(&id) {
                bail!("check {:?}: unknown block `{id}`", name);
            }
        }

        let attr_expr = |key: &str| -> Option<Expression> {
            block
                .body
                .attributes()
                .find(|a| a.key.as_str() == key)
                .map(|a| a.expr.clone())
        };

        let attr_str = |key: &str| -> Result<Option<String>> {
            match attr_expr(key) {
                None => Ok(None),
                Some(expr) => {
                    let mut func_ctx = hcl::eval::Context::new();
                    crate::functions::register(&mut func_ctx);
                    match expr.evaluate(&func_ctx) {
                        Ok(hcl::Value::String(s)) => Ok(Some(s)),
                        Ok(other) => {
                            bail!("check {:?}: `{key}` must be a string, got {other:?}", name)
                        }
                        Err(e) => bail!("check {:?}: evaluating `{key}`: {e}", name),
                    }
                }
            }
        };

        let query = attr_expr("query").ok_or_else(|| anyhow!("check {name:?} has no `query`"))?;

        let on_raw = attr_expr("on");
        let on = on_raw.map(extract_on).transpose()?;

        let every = attr_str("every")?;
        let timeout = attr_str("timeout")?;

        let warn = parse_tier(&block.body, "warn", &name)?;
        let fail = parse_tier(&block.body, "fail", &name)?;

        let validate = parse_validate(&block.body, &name)?;

        if validate.is_some() && (warn.is_some() || fail.is_some()) {
            bail!(
                "check {:?}: `validate` is mutually exclusive with `warn`/`fail`; \
                 remove one or the other",
                name
            );
        }

        let on_fail_raw = attr_expr("on_fail");
        let on_fail = on_fail_raw.map(extract_on_fail).transpose()?;

        let for_each_items = if let Some(fe_expr) = attr_expr("for_each") {
            let mut fe_ctx = hcl::eval::Context::new();
            crate::functions::register(&mut fe_ctx);
            match fe_expr.evaluate(&fe_ctx) {
                Ok(hcl::Value::Array(items)) => items
                    .into_iter()
                    .map(|v| match v {
                        hcl::Value::String(s) => Ok(s),
                        other => bail!(
                            "check {:?}: for_each items must be strings, got {other:?}",
                            name
                        ),
                    })
                    .collect::<Result<Vec<_>>>()?,
                Ok(other) => bail!("check {:?}: for_each must be a list, got {other:?}", name),
                Err(e) => bail!("check {:?}: evaluating for_each: {e}", name),
            }
        } else {
            vec![]
        };

        let on = on.or_else(|| defaults.on.clone());
        let every = every.or_else(|| defaults.every.clone());
        let timeout = timeout.or_else(|| defaults.timeout.clone());
        let on_fail = on_fail.or_else(|| defaults.on_fail.clone());

        if for_each_items.is_empty() {
            checks.push(Check {
                name,
                on,
                every,
                timeout,
                query,
                warn,
                fail,
                each_value: None,
                on_fail,
                validate,
            });
        } else {
            for item in for_each_items {
                checks.push(Check {
                    name: format!("{name}[{item}]"),
                    on: on.clone(),
                    every: every.clone(),
                    timeout: timeout.clone(),
                    query: query.clone(),
                    warn: clone_tier(&warn),
                    fail: clone_tier(&fail),
                    each_value: Some(item),
                    on_fail: on_fail.clone(),
                    validate: validate.clone(),
                });
            }
        }
    }

    Ok(checks)
}

/// Clone a `Tier` for for_each expansion.
pub fn clone_tier(t: &Option<Tier>) -> Option<Tier> {
    t.as_ref().map(|t| Tier {
        when: t.when.clone(),
        sustained: t.sustained.clone(),
        sample: t.sample,
    })
}

/// Parse an optional `validate { column "<name>" { ... } }` block.
fn parse_validate(body: &hcl::Body, check_name: &str) -> Result<Option<Validate>> {
    let vblock = body.blocks().find(|b| b.identifier() == "validate");
    let Some(vb) = vblock else {
        return Ok(None);
    };

    if let Some(attr) = vb.body.attributes().next() {
        bail!(
            "check {:?}: validate block: unknown attribute `{}`",
            check_name,
            attr.key.as_str()
        );
    }

    let mut columns = Vec::new();

    for col_block in vb.body.blocks() {
        let id = col_block.identifier();
        if id != "column" {
            bail!(
                "check {:?}: validate block: unknown block `{id}` (expected `column`)",
                check_name
            );
        }

        let col_name = col_block
            .labels()
            .first()
            .ok_or_else(|| {
                anyhow!(
                    "check {:?}: validate column block missing name label",
                    check_name
                )
            })?
            .as_str()
            .to_string();

        for attr in col_block.body.attributes() {
            let k = attr.key.as_str();
            if !COLUMN_RULE_ATTRS.contains(&k) {
                bail!(
                    "check {:?}: validate column {:?}: unknown attribute `{k}`",
                    check_name,
                    col_name
                );
            }
        }

        let eval_attr = |key: &str| -> Result<Option<hcl::Value>> {
            match col_block.body.attributes().find(|a| a.key.as_str() == key) {
                None => Ok(None),
                Some(attr) => {
                    let mut ctx = hcl::eval::Context::new();
                    crate::functions::register(&mut ctx);
                    match attr.expr.evaluate(&ctx) {
                        Ok(v) => Ok(Some(v)),
                        Err(e) => bail!(
                            "check {:?}: validate column {:?}: evaluating `{key}`: {e}",
                            check_name,
                            col_name
                        ),
                    }
                }
            }
        };

        let type_ = match eval_attr("type")? {
            None => None,
            Some(hcl::Value::String(s)) => {
                match s.as_str() {
                    "int" | "float" | "string" | "bool" | "timestamp" => {}
                    other => bail!(
                        "check {:?}: validate column {:?}: unknown type {:?}; \
                         expected int, float, string, bool, or timestamp",
                        check_name,
                        col_name,
                        other
                    ),
                }
                Some(s)
            }
            Some(other) => bail!(
                "check {:?}: validate column {:?}: `type` must be a string, got {other:?}",
                check_name,
                col_name
            ),
        };

        let not_null = match eval_attr("not_null")? {
            None => None,
            Some(hcl::Value::Bool(b)) => Some(b),
            Some(other) => bail!(
                "check {:?}: validate column {:?}: `not_null` must be bool, got {other:?}",
                check_name,
                col_name
            ),
        };

        let null_rate = match eval_attr("null_rate")? {
            None => None,
            Some(hcl::Value::Number(n)) => Some(n.as_f64().ok_or_else(|| {
                anyhow!(
                    "check {:?}: validate column {:?}: `null_rate` not representable as f64",
                    check_name,
                    col_name
                )
            })?),
            Some(other) => bail!(
                "check {:?}: validate column {:?}: `null_rate` must be a number, got {other:?}",
                check_name,
                col_name
            ),
        };

        let allowed = match eval_attr("allowed")? {
            None => None,
            Some(hcl::Value::Array(arr)) => {
                let mut strs = Vec::new();
                for item in arr {
                    match item {
                        hcl::Value::String(s) => strs.push(s),
                        other => bail!(
                            "check {:?}: validate column {:?}: `allowed` items must be strings, got {other:?}",
                            check_name,
                            col_name
                        ),
                    }
                }
                Some(strs)
            }
            Some(other) => bail!(
                "check {:?}: validate column {:?}: `allowed` must be a list, got {other:?}",
                check_name,
                col_name
            ),
        };

        // Compile the regex at parse time so bad patterns fail fast.
        let matches = match eval_attr("matches")? {
            None => None,
            Some(hcl::Value::String(s)) => {
                let re = regex::Regex::new(&s).map_err(|e| {
                    anyhow!(
                        "check {:?}: validate column {:?}: `matches` regex {:?} is invalid: {e}",
                        check_name,
                        col_name,
                        s
                    )
                })?;
                Some(re)
            }
            Some(other) => bail!(
                "check {:?}: validate column {:?}: `matches` must be a string, got {other:?}",
                check_name,
                col_name
            ),
        };

        let range = match eval_attr("range")? {
            None => None,
            Some(hcl::Value::Object(obj)) => {
                let get_f64 = |key: &str| -> Result<f64> {
                    match obj.get(key) {
                        None => bail!(
                            "check {:?}: validate column {:?}: `range` missing `{key}`",
                            check_name, col_name
                        ),
                        Some(hcl::Value::Number(n)) => n.as_f64().ok_or_else(|| {
                            anyhow!(
                                "check {:?}: validate column {:?}: `range.{key}` not representable as f64",
                                check_name, col_name
                            )
                        }),
                        Some(other) => bail!(
                            "check {:?}: validate column {:?}: `range.{key}` must be a number, got {other:?}",
                            check_name, col_name
                        ),
                    }
                };
                let min = get_f64("min")?;
                let max = get_f64("max")?;
                if min > max {
                    bail!(
                        "check {:?}: validate column {:?}: `range.min` ({min}) must be <= `range.max` ({max})",
                        check_name,
                        col_name
                    );
                }
                Some((min, max))
            }
            Some(other) => bail!(
                "check {:?}: validate column {:?}: `range` must be an object, got {other:?}",
                check_name,
                col_name
            ),
        };

        let unique = match eval_attr("unique")? {
            None => None,
            Some(hcl::Value::Bool(b)) => Some(b),
            Some(other) => bail!(
                "check {:?}: validate column {:?}: `unique` must be bool, got {other:?}",
                check_name,
                col_name
            ),
        };

        let outliers = match eval_attr("outliers")? {
            None => None,
            Some(hcl::Value::String(s)) => {
                match s.as_str() {
                    "iqr" | "zscore" => {}
                    other => bail!(
                        "check {:?}: validate column {:?}: unknown outliers method {:?}; \
                         expected 'iqr' or 'zscore'",
                        check_name,
                        col_name,
                        other
                    ),
                }
                Some(s)
            }
            Some(other) => bail!(
                "check {:?}: validate column {:?}: `outliers` must be a string, got {other:?}",
                check_name,
                col_name
            ),
        };

        let distribution = match eval_attr("distribution")? {
            None => None,
            Some(hcl::Value::String(s)) => {
                match s.as_str() {
                    "normal" => {}
                    other => bail!(
                        "check {:?}: validate column {:?}: unknown distribution {:?}; \
                         expected 'normal'",
                        check_name,
                        col_name,
                        other
                    ),
                }
                Some(s)
            }
            Some(other) => bail!(
                "check {:?}: validate column {:?}: `distribution` must be a string, got {other:?}",
                check_name,
                col_name
            ),
        };

        columns.push(ColumnRule {
            name: col_name,
            type_,
            not_null,
            null_rate,
            allowed,
            matches,
            range,
            unique,
            outliers,
            distribution,
        });
    }

    Ok(Some(Validate { columns }))
}

/// Parse a tier (warn or fail) from a check body: either bare attr or block form.
fn parse_tier(body: &hcl::Body, key: &str, check_name: &str) -> Result<Option<Tier>> {
    // Bare form `warn = <expr>` vs block form `warn { when = <expr> ... }`.
    let bare = body
        .attributes()
        .find(|a| a.key.as_str() == key)
        .map(|a| a.expr.clone());
    let block = body.blocks().find(|b| b.identifier() == key);

    match (bare, block) {
        (Some(_), Some(_)) => bail!(
            "check {:?}: `{key}` defined as both an attribute and a block",
            check_name
        ),
        (Some(expr), None) => Ok(Some(Tier {
            when: expr,
            sustained: None,
            sample: None,
        })),
        (None, Some(b)) => {
            for attr in b.body.attributes() {
                let k = attr.key.as_str();
                if !TIER_ATTRS.contains(&k) {
                    bail!(
                        "check {:?}: {key} block: unknown attribute `{k}`",
                        check_name
                    );
                }
            }
            let when = b
                .body
                .attributes()
                .find(|a| a.key.as_str() == "when")
                .map(|a| a.expr.clone())
                .ok_or_else(|| anyhow!("check {:?}: {key} block has no `when`", check_name))?;
            let sustained = b
                .body
                .attributes()
                .find(|a| a.key.as_str() == "sustained")
                .map(|a| a.expr.clone())
                .map(|e| {
                    let mut ctx = hcl::eval::Context::new();
                    crate::functions::register(&mut ctx);
                    match e.evaluate(&ctx) {
                        Ok(hcl::Value::String(s)) => Ok(s),
                        Ok(other) => bail!(
                            "check {:?}: {key}.sustained must be a string, got {other:?}",
                            check_name
                        ),
                        Err(e) => bail!("check {:?}: evaluating {key}.sustained: {e}", check_name),
                    }
                })
                .transpose()?;
            let sample = b
                .body
                .attributes()
                .find(|a| a.key.as_str() == "sample")
                .map(|a| a.expr.clone())
                .map(|e| {
                    let mut ctx = hcl::eval::Context::new();
                    crate::functions::register(&mut ctx);
                    match e.evaluate(&ctx) {
                        Ok(hcl::Value::Number(n)) => {
                            n.as_u64().map(|v| v as usize).ok_or_else(|| {
                                anyhow!(
                                    "check {:?}: {key}.sample must be a non-negative integer",
                                    check_name
                                )
                            })
                        }
                        Ok(other) => bail!(
                            "check {:?}: {key}.sample must be a number, got {other:?}",
                            check_name
                        ),
                        Err(e) => bail!("check {:?}: evaluating {key}.sample: {e}", check_name),
                    }
                })
                .transpose()?;
            Ok(Some(Tier {
                when,
                sustained,
                sample,
            }))
        }
        (None, None) => Ok(None),
    }
}

/// Parse top-level `defaults { on = ...  every = ...  timeout = ...  on_fail = ... }` block.
fn parse_defaults(body: &hcl::Body) -> Result<Defaults> {
    let mut on = None;
    let mut every = None;
    let mut timeout = None;
    let mut on_fail = None;

    for block in body.blocks().filter(|b| b.identifier() == "defaults") {
        for attr in block.body.attributes() {
            match attr.key.as_str() {
                "on" => {
                    on = Some(extract_on(attr.expr.clone())?);
                }
                "every" => {
                    let mut ctx = hcl::eval::Context::new();
                    crate::functions::register(&mut ctx);
                    match attr.expr.evaluate(&ctx) {
                        Ok(hcl::Value::String(s)) => every = Some(s),
                        Ok(other) => bail!("defaults.every must be a string, got {other:?}"),
                        Err(e) => bail!("evaluating defaults.every: {e}"),
                    }
                }
                "timeout" => {
                    let mut ctx = hcl::eval::Context::new();
                    crate::functions::register(&mut ctx);
                    match attr.expr.evaluate(&ctx) {
                        Ok(hcl::Value::String(s)) => timeout = Some(s),
                        Ok(other) => bail!("defaults.timeout must be a string, got {other:?}"),
                        Err(e) => bail!("evaluating defaults.timeout: {e}"),
                    }
                }
                "on_fail" => {
                    on_fail = Some(extract_on_fail(attr.expr.clone())?);
                }
                other => bail!("defaults block: unknown attribute `{other}`"),
            }
        }
    }

    Ok(Defaults {
        on,
        every,
        timeout,
        on_fail,
    })
}

/// Connection name from an `on` expr: a string literal, or a traversal's last segment.
pub fn extract_on(expr: Expression) -> Result<String> {
    match expr {
        Expression::String(s) => Ok(s),
        Expression::Traversal(t) => t
            .operators
            .iter()
            .rev()
            .find_map(|op| {
                if let TraversalOperator::GetAttr(ident) = op {
                    Some(ident.as_str().to_string())
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow!("`on` traversal has no attribute segment")),
        other => bail!("`on` must be a string or traversal, got: {other:?}"),
    }
}

/// Notifier name from an `on_fail` expr: a string literal, or a traversal's last segment.
fn extract_on_fail(expr: Expression) -> Result<String> {
    match expr {
        Expression::String(s) => Ok(s),
        Expression::Traversal(t) => t
            .operators
            .iter()
            .rev()
            .find_map(|op| {
                if let TraversalOperator::GetAttr(ident) = op {
                    Some(ident.as_str().to_string())
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow!("`on_fail` traversal has no attribute segment")),
        other => bail!("`on_fail` must be a string or traversal, got: {other:?}"),
    }
}

/// Parse every `notify "<type>" "<name>" { ... }` block (`env(...)` allowed in values).
pub fn parse_notifiers(src: &str) -> Result<Vec<(String, crate::notify::Notifier)>> {
    let body = hcl::parse(src).context("parsing HCL")?;
    let mut notifiers = Vec::new();

    for block in body.blocks().filter(|b| b.identifier() == "notify") {
        let kind = label(block, 0, "type")?;
        let name = label(block, 1, "name")?;

        let eval_str = |key: &str| -> Result<Option<String>> {
            match block.body.attributes().find(|a| a.key.as_str() == key) {
                None => Ok(None),
                Some(attr) => {
                    let mut ctx = hcl::eval::Context::new();
                    crate::functions::register(&mut ctx);
                    match attr.expr.evaluate(&ctx) {
                        Ok(hcl::Value::String(s)) => Ok(Some(s)),
                        Ok(other) => anyhow::bail!(
                            "notify {:?} {:?}: `{key}` must be a string, got {other:?}",
                            kind,
                            name
                        ),
                        Err(e) => {
                            anyhow::bail!("notify {:?} {:?}: evaluating `{key}`: {e}", kind, name)
                        }
                    }
                }
            }
        };

        let notifier = match kind.as_str() {
            "webhook" => {
                for attr in block.body.attributes() {
                    match attr.key.as_str() {
                        "url" => {}
                        other => anyhow::bail!(
                            "notify \"webhook\" {:?}: unknown attribute `{other}`",
                            name
                        ),
                    }
                }
                let url = eval_str("url")?.ok_or_else(|| {
                    anyhow::anyhow!("notify \"webhook\" {:?}: missing `url`", name)
                })?;
                crate::notify::Notifier::Webhook { url }
            }
            other => bail!(
                "unknown notifier type {:?} (notify {:?} {:?})",
                other,
                other,
                name
            ),
        };

        notifiers.push((name, notifier));
    }

    Ok(notifiers)
}

/// Parse the optional `heartbeat { ... }` block. At most one is allowed.
/// `url`/`fail_url` accept `env(...)`; `fail_body` is read as a raw string so
/// its `{{...}}` placeholders survive to the run-time resolver.
pub fn parse_heartbeat(src: &str) -> Result<Option<crate::heartbeat::HeartbeatConfig>> {
    use crate::heartbeat::{ContentType, HeartbeatConfig};
    let body = hcl::parse(src).context("parsing HCL")?;
    let mut blocks = body.blocks().filter(|b| b.identifier() == "heartbeat");

    let block = match blocks.next() {
        None => return Ok(None),
        Some(b) => b,
    };
    if blocks.next().is_some() {
        bail!("at most one `heartbeat` block is allowed");
    }

    let eval_str = |key: &str| -> Result<Option<String>> {
        match block.body.attributes().find(|a| a.key.as_str() == key) {
            None => Ok(None),
            Some(attr) => {
                let mut ctx = hcl::eval::Context::new();
                crate::functions::register(&mut ctx);
                match attr.expr.evaluate(&ctx) {
                    Ok(hcl::Value::String(s)) => Ok(Some(s)),
                    Ok(other) => bail!("heartbeat: `{key}` must be a string, got {other:?}"),
                    Err(e) => bail!("heartbeat: evaluating `{key}`: {e}"),
                }
            }
        }
    };

    for attr in block.body.attributes() {
        match attr.key.as_str() {
            "url" | "fail_url" | "fail_body" | "content_type" => {}
            other => bail!("heartbeat: unknown attribute `{other}`"),
        }
    }

    let url = eval_str("url")?.ok_or_else(|| anyhow!("heartbeat: missing `url`"))?;
    let fail_url = eval_str("fail_url")?;
    let fail_body = eval_str("fail_body")?;
    let content_type = match eval_str("content_type")?.as_deref() {
        None | Some("text/plain") => ContentType::Text,
        Some("application/json") => ContentType::Json,
        Some(other) => bail!(
            "heartbeat: `content_type` must be \"text/plain\" or \"application/json\", got {other:?}"
        ),
    };

    if let Some(tmpl) = &fail_body {
        crate::heartbeat::validate_template(tmpl)?;
    }

    Ok(Some(HeartbeatConfig {
        url,
        fail_url,
        fail_body,
        content_type,
    }))
}

/// Top-level `state { dsn = ... }` block — groundtruth's own writable bookkeeping
/// database (Postgres or SQLite), separate from the connectorx check
/// connections. `dsn` set → Sql backend; absent → in-memory.
#[derive(Debug)]
pub struct StateConfig {
    /// Writable DSN for the Sql backend (`postgres://...` or `sqlite:...`).
    /// `None` → Memory.
    pub dsn: Option<String>,
}

/// Parse the optional `state { ... }` block; absent → `dsn: None`.
pub fn parse_state_config(src: &str) -> Result<StateConfig> {
    let body = hcl::parse(src).context("parsing HCL")?;
    let mut dsn = None;

    for block in body.blocks().filter(|b| b.identifier() == "state") {
        for attr in block.body.attributes() {
            match attr.key.as_str() {
                "dsn" => {
                    let mut ctx = hcl::eval::Context::new();
                    crate::functions::register(&mut ctx);
                    match attr.expr.evaluate(&ctx) {
                        Ok(hcl::Value::String(s)) => dsn = Some(s),
                        Ok(other) => {
                            bail!("state block: `dsn` must be a string, got {other:?}")
                        }
                        Err(e) => bail!("state block: evaluating `dsn`: {e}"),
                    }
                }
                other => bail!("state block: unknown attribute `{other}`"),
            }
        }
        if let Some(sub) = block.body.blocks().next() {
            bail!("state block: unknown block `{}`", sub.identifier());
        }
    }

    Ok(StateConfig { dsn })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcl::eval::{Context, Evaluate};

    fn sql_of(c: &Check) -> String {
        match c.query.evaluate(&Context::new()).unwrap() {
            hcl::Value::String(s) => s,
            other => panic!("query not a string: {other:?}"),
        }
    }

    #[test]
    fn parses_name_query_and_fail() {
        let checks = parse_checks(
            r#"
            check "orders_present" {
              query = "select count(*) as recent from orders"
              fail  = row.recent == 0
            }
            "#,
        )
        .unwrap();

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "orders_present");
        assert_eq!(sql_of(&checks[0]), "select count(*) as recent from orders");
        assert!(checks[0].fail.is_some());
        assert!(checks[0].warn.is_none());
    }

    #[test]
    fn fail_block_form_is_equivalent_to_bare_attr() {
        let checks = parse_checks(
            r#"
            check "orphans" {
              query = "select 1"
              fail { when = rows.count > 0 }
            }
            "#,
        )
        .unwrap();
        assert!(checks[0].fail.is_some(), "block-form fail should be parsed");
    }

    #[test]
    fn parses_postgres_connection() {
        let conns =
            parse_connections("connection \"postgres\" \"main\" {\n  dsn = \"postgres://x/y\"\n}")
                .unwrap();
        assert_eq!(conns.len(), 1);
        assert_eq!(conns[0].name, "main");
        // Postgres checks now route through connectorx like every other engine.
        let ConnConfig::ConnectorX(cx) = &conns[0].config else {
            panic!("expected ConnectorX config");
        };
        assert_eq!(cx.provider, "postgres");
        assert_eq!(cx.dsn, "postgres://x/y");
    }

    #[test]
    fn parses_connectorx_provider_connection() {
        let conns =
            parse_connections("connection \"mysql\" \"m\" {\n  dsn = \"mysql://u:p@h:3306/db\"\n}")
                .unwrap();
        assert_eq!(conns.len(), 1);
        let ConnConfig::ConnectorX(cx) = &conns[0].config else {
            panic!("expected ConnectorX config");
        };
        assert_eq!(cx.provider, "mysql");
        assert_eq!(cx.dsn, "mysql://u:p@h:3306/db");
    }

    #[test]
    fn parses_athena_connection() {
        let conns = parse_connections(
            "connection \"athena\" \"lake\" {\n  region = \"us-east-1\"\n  database = \"default\"\n  output_location = \"s3://b/r/\"\n}",
        )
        .unwrap();
        let ConnConfig::Athena(a) = &conns[0].config else {
            panic!("expected Athena config");
        };
        assert_eq!(a.region, "us-east-1");
        assert_eq!(a.database, "default");
        assert_eq!(a.output_location.as_deref(), Some("s3://b/r/"));
        assert_eq!(a.workgroup, None);
    }

    #[test]
    fn athena_requires_region_and_database() {
        let err = parse_connections("connection \"athena\" \"l\" {\n  region = \"us-east-1\"\n}")
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("missing `database`"),
            "got: {err:#}"
        );
    }

    #[test]
    fn connectorx_provider_requires_dsn() {
        let err = parse_connections("connection \"trino\" \"t\" {\n}").unwrap_err();
        assert!(
            format!("{err:#}").contains("missing `dsn`"),
            "expected missing dsn error, got: {err:#}"
        );
    }

    #[test]
    fn connectorx_provider_rejects_unknown_field() {
        let err = parse_connections(
            "connection \"bigquery\" \"b\" {\n  dsn = \"bigquery://x\"\n  path = \"y\"\n}",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("path"),
            "expected unknown-field error, got: {err:#}"
        );
    }

    #[test]
    fn unsupported_dsn_scheme_is_rejected() {
        // No hardcoded provider list — validation is by dsn scheme via connectorx.
        let err = parse_connections("connection \"mongo\" \"m\" {\n  dsn = \"mongodb://h/db\"\n}")
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unrecognized connection URL scheme") || msg.contains("not supported"),
            "expected scheme-rejection error, got: {msg}"
        );
    }

    #[test]
    fn unknown_field_in_connection_is_rejected() {
        let err = parse_connections(
            "connection \"postgres\" \"main\" {\n  dsn = \"d\"\n  poolsize = 9\n}",
        )
        .unwrap_err();
        let full = format!("{err:#}");
        assert!(
            full.contains("poolsize"),
            "expected typo to be reported, got: {full}"
        );
    }

    // Regression tests: typos must fail loudly, never be silently ignored.

    #[test]
    fn typo_in_fail_attribute_is_rejected() {
        let err = parse_checks(
            r#"
            check "x" {
              query = "select 1"
              fial  = row.n == 0
            }
            "#,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("fial"),
            "error must name the offending key 'fial', got: {msg}"
        );
    }

    #[test]
    fn unknown_block_in_check_is_rejected() {
        let err = parse_checks(
            r#"
            check "x" {
              query = "select 1"
              bogus { }
            }
            "#,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("bogus"),
            "error must name the offending block 'bogus', got: {msg}"
        );
    }

    #[test]
    fn unknown_tier_key_is_rejected() {
        let err = parse_checks(
            r#"
            check "x" {
              query = "select 1"
              fail { whne = row.n == 0 }
            }
            "#,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("whne"),
            "error must name the offending key 'whne', got: {msg}"
        );
    }

    // Model coverage

    #[test]
    fn bare_warn_parses() {
        let checks = parse_checks(
            r#"
            check "x" {
              query = "select 1"
              warn  = row.n > 5
            }
            "#,
        )
        .unwrap();
        let warn = checks[0].warn.as_ref().expect("warn should be Some");
        assert!(warn.sustained.is_none(), "bare warn has no sustained");
        assert!(warn.sample.is_none(), "bare warn has no sample");
    }

    #[test]
    fn block_warn_with_sustained_and_sample_parses() {
        let checks = parse_checks(
            r#"
            check "x" {
              query = "select 1"
              warn {
                when      = row.n > 5
                sustained = "15m"
                sample    = 10
              }
            }
            "#,
        )
        .unwrap();
        let warn = checks[0].warn.as_ref().expect("warn should be Some");
        assert_eq!(warn.sustained, Some("15m".to_string()));
        assert_eq!(warn.sample, Some(10));
    }

    /// `baseline {}` is no longer in `CHECK_BLOCKS`, so it's a hard error naming the block.
    #[test]
    fn baseline_block_is_now_hard_error() {
        let err = parse_checks(
            r#"
            check "x" {
              query = "select 1"
              baseline {
                metric = row.val
                window = "14d"
              }
            }
            "#,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("baseline"),
            "error must name the 'baseline' block, got: {msg}"
        );
    }

    #[test]
    fn defaults_applied_to_check_missing_every() {
        let checks = parse_checks(
            r#"
            defaults {
              every = "5m"
            }
            check "x" {
              query = "select 1"
            }
            "#,
        )
        .unwrap();
        assert_eq!(
            checks[0].every.as_deref(),
            Some("5m"),
            "check should inherit defaults.every"
        );
    }

    #[test]
    fn defaults_applied_to_check_missing_on() {
        let checks = parse_checks(
            r#"
            defaults {
              on = "primary"
            }
            check "x" {
              query = "select 1"
            }
            "#,
        )
        .unwrap();
        assert_eq!(
            checks[0].on.as_deref(),
            Some("primary"),
            "check should inherit defaults.on"
        );
    }

    #[test]
    fn for_each_expands_checks() {
        let checks = parse_checks(
            r#"
            check "orders" {
              query      = "select 1"
              for_each   = ["a", "b"]
            }
            "#,
        )
        .unwrap();
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].name, "orders[a]");
        assert_eq!(checks[0].each_value, Some("a".to_string()));
        assert_eq!(checks[1].name, "orders[b]");
        assert_eq!(checks[1].each_value, Some("b".to_string()));
    }

    #[test]
    fn on_traversal_form_yields_last_segment() {
        let checks = parse_checks(
            r#"
            check "x" {
              query = "select 1"
              on    = connection.postgres.main
            }
            "#,
        )
        .unwrap();
        assert_eq!(
            checks[0].on.as_deref(),
            Some("main"),
            "traversal on = connection.postgres.main should yield 'main'"
        );
    }

    #[test]
    fn on_string_form_yields_name() {
        let checks = parse_checks(
            r#"
            check "x" {
              query = "select 1"
              on    = "replica"
            }
            "#,
        )
        .unwrap();
        assert_eq!(
            checks[0].on.as_deref(),
            Some("replica"),
            "string on = \"replica\" should yield 'replica'"
        );
    }

    // on_fail regression tests

    #[test]
    fn on_fail_string_parses() {
        let checks = parse_checks(
            r#"
            check "x" {
              query   = "select 1"
              on_fail = "oncall"
            }
            "#,
        )
        .unwrap();
        assert_eq!(
            checks[0].on_fail.as_deref(),
            Some("oncall"),
            "on_fail should parse to 'oncall'"
        );
    }

    #[test]
    fn on_fail_traversal_yields_last_segment() {
        let checks = parse_checks(
            r#"
            check "x" {
              query   = "select 1"
              on_fail = notify.slack.oncall
            }
            "#,
        )
        .unwrap();
        assert_eq!(
            checks[0].on_fail.as_deref(),
            Some("oncall"),
            "traversal on_fail should yield last segment 'oncall'"
        );
    }

    #[test]
    fn check_without_on_fail_has_none() {
        let checks = parse_checks(
            r#"
            check "x" {
              query = "select 1"
            }
            "#,
        )
        .unwrap();
        assert!(
            checks[0].on_fail.is_none(),
            "on_fail should default to None"
        );
    }

    #[test]
    fn bogus_attr_still_rejected_alongside_on_fail() {
        let err = parse_checks(
            r#"
            check "x" {
              query   = "select 1"
              on_fail = "oncall"
              bogus   = "nope"
            }
            "#,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("bogus"),
            "bogus attr should still be rejected, got: {msg}"
        );
    }

    // parse_notifiers tests

    #[test]
    fn parse_notifiers_webhook() {
        let notifiers = parse_notifiers(
            r#"
            notify "webhook" "alerts" {
              url = "https://example.com/hook"
            }
            "#,
        )
        .unwrap();
        assert_eq!(notifiers.len(), 1);
        assert_eq!(notifiers[0].0, "alerts");
        let crate::notify::Notifier::Webhook { url } = &notifiers[0].1;
        assert_eq!(url, "https://example.com/hook");
    }

    #[test]
    fn parse_notifiers_unknown_type_is_rejected() {
        let err = parse_notifiers(
            r#"
            notify "email" "me" {
              to = "test@example.com"
            }
            "#,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("email"), "expected type name in error: {msg}");
    }

    #[test]
    fn parse_notifiers_unknown_attr_is_rejected() {
        let err = parse_notifiers(
            r#"
            notify "webhook" "alerts" {
              url     = "https://example.com"
              timeout = 30
            }
            "#,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("timeout"),
            "expected bogus attr in error: {msg}"
        );
    }

    #[test]
    fn parse_notifiers_empty_returns_empty_vec() {
        let notifiers = parse_notifiers(
            r#"
            check "x" {
              query = "select 1"
            }
            "#,
        )
        .unwrap();
        assert!(notifiers.is_empty());
    }

    // parse_heartbeat tests

    #[test]
    fn heartbeat_parses_url_and_defaults() {
        unsafe { std::env::set_var("GT_TEST_HB", "https://hc-ping.com/abc") };
        let cfg = parse_heartbeat("heartbeat {\n  url = env(\"GT_TEST_HB\")\n}")
            .unwrap()
            .expect("expected a heartbeat block");
        assert_eq!(cfg.url, "https://hc-ping.com/abc");
        assert_eq!(cfg.fail_url, None);
        assert_eq!(cfg.fail_body, None);
        assert_eq!(cfg.content_type, crate::heartbeat::ContentType::Text);
        assert_eq!(cfg.fail_target(), "https://hc-ping.com/abc/fail");
    }

    #[test]
    fn heartbeat_absent_is_none() {
        assert!(
            parse_heartbeat("check \"x\" { query = \"select 1\" }")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn heartbeat_rejects_unknown_attr() {
        let err = parse_heartbeat("heartbeat {\n  url = \"x\"\n  bogus = \"y\"\n}").unwrap_err();
        assert!(err.to_string().contains("bogus"), "got: {err}");
    }

    #[test]
    fn heartbeat_rejects_second_block() {
        let err =
            parse_heartbeat("heartbeat { url = \"a\" }\nheartbeat { url = \"b\" }").unwrap_err();
        assert!(err.to_string().contains("at most one"), "got: {err}");
    }

    #[test]
    fn heartbeat_json_content_type_and_body() {
        let cfg = parse_heartbeat(
            "heartbeat {\n  url = \"u\"\n  content_type = \"application/json\"\n  fail_body = \"{{summary_json}}\"\n}",
        )
        .unwrap()
        .unwrap();
        assert_eq!(cfg.content_type, crate::heartbeat::ContentType::Json);
        assert_eq!(cfg.fail_body.as_deref(), Some("{{summary_json}}"));
    }

    #[test]
    fn heartbeat_rejects_unknown_template_var_at_parse_time() {
        // A typo'd `{{var}}` in fail_body must fail closed here (i.e. `gt check`),
        // not silently at run time.
        let err = parse_heartbeat("heartbeat {\n  url = \"u\"\n  fail_body = \"{{nope}}\"\n}")
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "got: {err}");
    }

    // parse_state_config tests

    #[test]
    fn state_config_absent_yields_none() {
        let cfg = parse_state_config(
            r#"
            check "x" {
              query = "select 1"
            }
            "#,
        )
        .unwrap();
        assert!(
            cfg.dsn.is_none(),
            "absent state block should yield dsn=None"
        );
    }

    #[test]
    fn state_config_parses_postgres_dsn() {
        let cfg = parse_state_config(
            r#"
            state {
              dsn = "postgres://u:p@host/groundtruth_state"
            }
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.dsn.as_deref(),
            Some("postgres://u:p@host/groundtruth_state")
        );
    }

    #[test]
    fn state_config_dsn_supports_env() {
        unsafe { std::env::set_var("GT_TEST_STATE_DSN", "sqlite:state.db?mode=rwc") };
        let cfg = parse_state_config(
            r#"
            state {
              dsn = env("GT_TEST_STATE_DSN")
            }
            "#,
        )
        .unwrap();
        assert_eq!(cfg.dsn.as_deref(), Some("sqlite:state.db?mode=rwc"));
    }

    #[test]
    fn state_config_unknown_attr_is_hard_error() {
        let err = parse_state_config(
            r#"
            state {
              bogus = "x"
            }
            "#,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("bogus"),
            "unknown attr should be reported, got: {msg}"
        );
    }

    #[test]
    fn state_config_unknown_block_is_hard_error() {
        let err = parse_state_config(
            r#"
            state {
              nested { }
            }
            "#,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("nested"),
            "unknown sub-block should be reported, got: {msg}"
        );
    }
}
