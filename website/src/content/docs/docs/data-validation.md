---
title: Validating column data
description: Assert on individual columns of a query result with a declarative validate block — types, null rates, allowed sets, ranges, uniqueness, outliers, and distribution.
---

Sometimes a single `warn`/`fail` expression isn't enough — you want to assert facts about the columns a query returns. The `validate` block does that declaratively, one rule at a time.

```hcl
check "users_valid" {
  query = "select id, email, age, status from users"
  validate {
    column "id"     { not_null = true  unique = true }
    column "email"  { matches = ".+@.+" }
    column "age"    { type = "int"  range = { min = 0, max = 130 } }
    column "status" { allowed = ["active", "suspended", "closed"] }
  }
}
```

`validate` is **mutually exclusive** with `warn` and `fail` on the same check. Use one style or the other, not both. For expression-based checks, see [Writing checks](/docs/writing-checks).

## How outcomes are decided

- Any rule violation makes the check **FAIL**.
- A column named in `validate` that is missing from the result, or a rule that can't be evaluated, makes the check **ERROR** — groundtruth never reports a fake PASS.
- Up to 10 offending sample rows are attached to the result (terminal output and the JSON `sample` array).
- An empty result set is a **PASS** — there's nothing to violate.
- Rules skip NULL values, except `not_null` and `null_rate`, which are specifically about NULLs.

## The validators

Each rule below is a single attribute inside a `column "name" { ... }` block.

### type

```hcl
column "age" { type = "int" }
```

Non-null values must be the named type. `timestamp` accepts RFC3339, `YYYY-MM-DDTHH:MM:SS`, or `YYYY-MM-DD HH:MM:SS`.

### not_null

```hcl
column "id" { not_null = true }
```

Fail if any NULL is present.

### null_rate

```hcl
column "middle_name" { null_rate = 0.2 }
```

Fail if the fraction of NULLs exceeds the given number (0–1).

### allowed

```hcl
column "status" { allowed = ["active", "suspended", "closed"] }
```

Non-null values must all be in this set.

### matches

```hcl
column "email" { matches = ".+@.+" }
```

Non-null string values must match the regular expression.

### range

```hcl
column "score" { range = { min = 0, max = 100 } }
```

Non-null numeric values must fall in `[min, max]` inclusive; a non-numeric value fails. Setting `min > max` is a config error.

### unique

```hcl
column "id" { unique = true }
```

Non-null values must be unique.

### outliers

```hcl
column "latency_ms" { outliers = "iqr" }
```

Flag statistical outliers. `iqr` needs at least 4 values; `zscore` flags values with `|z| > 3` and needs at least 3 values.

### distribution

```hcl
column "measurement" { distribution = "normal" }
```

Runs a Jarque-Bera normality test. Needs at least 8 non-null values; it's a violation when p < 0.05.

## Validator reference

| Rule | Value | Meaning |
|---|---|---|
| `type` | `int` \| `float` \| `string` \| `bool` \| `timestamp` | Non-null values must be this type. `timestamp` accepts RFC3339, `YYYY-MM-DDTHH:MM:SS`, or `YYYY-MM-DD HH:MM:SS`. |
| `not_null` | `true` | Fail if any NULL present. |
| `null_rate` | number (0–1) | Fail if the fraction of NULLs exceeds this. |
| `allowed` | list of strings | Non-null values must be in this set. |
| `matches` | regex string | Non-null string values must match. |
| `range` | `{ min = .., max = .. }` | Non-null numeric values in `[min, max]` inclusive; non-numeric fails. `min > max` is a config error. |
| `unique` | `true` | Non-null values must be unique. |
| `outliers` | `"iqr"` \| `"zscore"` | Flag outliers. `iqr` needs ≥4 values; `zscore` flags `|z| > 3`, needs ≥3 values. |
| `distribution` | `"normal"` | Jarque-Bera normality test; needs ≥8 non-null values; violation if p < 0.05. |
