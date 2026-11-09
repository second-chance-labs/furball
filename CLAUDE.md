# CLAUDE.md

# Code standards

- Code should be self-explanatory.
- Never use comments, with the following exceptions: 1) docstrings, and 2) reasoning that would not be explainable by code alone.
- Minimize libraries, frameworks, tools, and services used.
- Write tests that would intentionally break your work.
- Money and counts are `rust_decimal::Decimal`, never `f64`.
- Credentials (API key id, private key) never appear in `Debug` output or error messages.
- Public library: every public item needs rustdoc.

# Documentation

- Minimize external documentation. Use inline documentation as much as possible.
- When documenting separately from code, utilize Mermaid diagrams (or simple ASCII diagrams) rather than explaining topics in-text.

# GitHub

- Never squash-merge a PR. Always merge or rebase.
- Always include a mermaid diagram with your PRs describing the high/medium level architecture of what was implemented.
- Always use conventional commits.
