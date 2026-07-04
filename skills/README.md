# groundtruth skills

Agent skills that help write and manage groundtruth configs.

## `authoring-groundtruth-checks`

Teaches an AI agent (Claude Code, etc.) to turn a database schema into correct
`gt` HCL checks in one pass: introspect the schema, sample real data, write
checks, and validate with `gt check` before finishing. Includes the complete,
authoritative HCL grammar so the agent never guesses attribute names.

**Install into your own project** so your agent picks it up automatically:

```sh
cp -r authoring-groundtruth-checks /path/to/your-project/.claude/skills/
```

Then ask your agent to "add groundtruth monitoring for this database" — it will
read the schema and write a validated `checks.hcl`.
