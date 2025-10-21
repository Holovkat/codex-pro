# Repository Guidelines

## Project Structure & Module Organization
- Rust crates live at the repo root; each crate folder maps to a `codex-*` crate (for example `core` → `codex-core`, `tui` → `codex-tui`).
- Shared utilities reside in `common`, protocol types in `protocol`, and backend contracts under `app-server-protocol` and `codex-backend-openapi-models`.
- CLI and terminal UI live in `cli` and `tui`; UI assets and snapshots are co-located with their modules.
- Reusable tooling and scripts sit in `scripts/` and `docs/` holds architectural notes and design RFCs.

## Build, Test, and Development Commands
- `just fmt` — run rustfmt across the workspace after making Rust changes.
- `just fix -p <crate>` — apply Clippy fixes for the touched crate; run `just fix` without `-p` only when shared crates change.
- `cargo test -p codex-<crate>` — scoped test run; add `--all-features` when exercising feature-gated APIs.
- `cargo test --all-features` — full suite, required after altering `common`, `core`, or `protocol`.
- `cargo insta pending-snapshots -p codex-tui` followed by `cargo insta accept -p codex-tui` — review then accept TUI snapshot updates.

## Coding Style & Naming Conventions
- Keep crate names prefixed with `codex-`; new modules follow snake_case, types are UpperCamelCase, and functions/methods are lower_snake_case.
- Collapse nested `if` statements, inline `format!` arguments, and prefer method references over redundant closures.
- Run rustfmt before committing; do not introduce manual formatting that conflicts with the configured `rustfmt.toml`.
- In the TUI, use ratatui’s `Stylize` helpers (`"text".red().bold()`) rather than manual `Style` construction.
- Avoid touching `CODEX_SANDBOX_ENV_VAR` or `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` logic; these guard sandbox-specific behavior.

## Testing Guidelines
- Use `cargo test -p codex-<crate>` for unit tests and `cargo test --all-features` when shared crates change.
- Snapshot tests rely on `insta`; inspect `.snap.new` files before accepting updates.
- Prefer `pretty_assertions::assert_eq` for clearer diffs and compare whole objects instead of individual fields.
- Integration tests in `core` leverage helpers from `core_test_support::responses`; capture the returned `ResponseMock` to assert outbound traffic.

## Commit & Pull Request Guidelines
- Write imperative, present-tense commit subjects (e.g., “Add TUI snapshot helpers”) and group related changes together.
- Reference relevant issues in commit bodies or PR descriptions and summarize validation (`cargo test -p codex-core`).
- PRs should include a brief change synopsis, screenshots or snapshot references when UI output shifts, and note any follow-up work.

## Environment & Agent Notes
- Commands may run inside sandboxed shells; assume `CODEX_SANDBOX_NETWORK_DISABLED=1` during automated runs.
- When scripting toolchains, guard optional dependencies and avoid downloading assets at build time without user confirmation.
