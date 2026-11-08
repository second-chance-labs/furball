# CLAUDE.md

## Workflow

Issues and epics are driven by the tokenmaxer scripts (`tokenmaxer/scripts/github/`) and skills:

- `/do-issue {n}` — start issue, create branch, implement, open PR
- `/do-epic {n}` — complete an epic's subissues in order, then open a PR back to main
- `/do-create-issue`, `/do-create-epic` — create issues / epics with linked subissues
- `/do-close-pr` — merge a PR once reviews and checks pass

Conventions the scripts enforce:

- Branch: `{issue-label}/{summary}` off `main`, or off `epic/{epic-slug}` when the issue has a parent
- PR title: `{issue-label}: {summary} - closes #{issue-number}`
- Plain merge (no squash), delete branch after merge
- Run scripts from the repository root; `gh` and `jq` are required

## Crate conventions

- MIT licensed.
- Read-only first: no trading (order/position) endpoints; add them only with explicit intent.
- Money and counts are `rust_decimal::Decimal`, never `f64`.
- Credentials (API key id, private key) never appear in `Debug` output or error messages.
- Public library: every public item needs rustdoc.
