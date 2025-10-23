# Codex Agentic Workspace (Rust 0.46.0)

Codex Agentic builds on the upstream OpenAI Codex CLI to deliver a customised Rust workspace targeting version `0.46.0`. It layers semantic indexing, provider management, and agentic command flows on top of the maintained Codex core while remaining compatible with upstream tooling.

## Architecture
- Workspace root hosts all crates; `Cargo.toml` tracks shared metadata and workspace lints.
- Core platform crates: `core/` (conversation engine), `common/` (shared utilities), `protocol/` (transport types), `exec/` (non-interactive CLI), `tui/` (Ratatui interface), and `cli/` (multitool entrypoints including `codex` and `codex-agentic`).
- Custom layer: `codex-agentic-core/` centralises settings overlays, BYOK provider resolution, semantic index services, ACP adapters, and the shared command registry used by CLI/TUI.
- Supporting services: `app-server` and `responses-api-proxy` expose HTTP surfaces; `cloud-tasks` and `backend-client` integrate optional queue-based workflows.
- Semantic assets, prompts, and snapshots sit beside their owners (e.g. `tui/tests/`, `example-system-prompts/`). The `future-functional-design/` directory outside the repo records upgrade checklists and diff archives.

## Infrastructure Requirements
- Toolchain pinned via `rust-toolchain.toml` to Rust `1.90.0` with `clippy`, `rustfmt`, and `rust-src`.
- macOS builds require `/usr/bin/sandbox-exec`; Linux builds rely on the `codex sandbox linux` launcher (Landlock). CLI entrypoints must honour `CODEX_SANDBOX`/`CODEX_SANDBOX_NETWORK_DISABLED`.
- Install helper tooling before development: `just`, `cargo-insta`, `fastembed` model caches (downloaded on demand), and optional `cargo-nextest` (`just test` ensures installation).
- Semantic indexers persist to `.codex/index/`; ensure disks permit large vector files and allow exclusive `fslock` access during builds.

## Core Codex Functionality
- **Interactive TUI** (`codex`, `codex-agentic`) provides full-screen chat, model picker, sandbox selectors (`--sandbox read-only|workspace-write|danger-full-access`), fuzzy file search (`@`), and transcript backtracking (`Esc` twice).
- **Non-interactive exec** (`codex exec "prompt"`) streams turn output to stdout and respects `RUST_LOG` for diagnostics.
- **Sandbox tooling** (`codex sandbox macos|linux`) mirrors CLI seatbelt/landlock behaviour for manual experiments.
- **MCP support**: acts as both client (`codex mcp ...`) and server (`codex mcp-server`), exposing JSON-RPC endpoints defined in `protocol/src/mcp_protocol.rs`.
- **Apply & resume helpers**: `codex apply` converts agent diffs into `git apply`, while `codex resume` continues saved sessions and integrates with CLI recipe hints.

## Agentic Extensions
- **Command registry** (`codex-agentic-core::CommandRegistry`) powers `/help-recipes`, semantic index commands, and `search-code` across CLI, TUI, ACP, and exec.
- **Semantic index** (`codex-agentic-core::index`) chunks repositories, reuses embeddings (`fastembed` + `hnsw_rs`), tracks analytics, and surfaces results through the TUI search pane and CLI commands (`codex-agentic index.query`, `search-code`).
- **Semantic search manager** (`/search-code`) opens a TUI modal to rebuild the index, adjust the persisted minimum confidence (`.codex/settings.json` or `~/.codex/settings.json`), and launch semantic queries. Searches are embedding-based (no regex support); use descriptive phrases, function names, or doc snippets—quote multi-word queries like `"load_config error handling"` for best results. CLI and ACP invocations respect the stored threshold, and you can override it per call with `codex search-code --min-confidence <percent>`.
- **BYOK provider management** integrates custom endpoints, cached model manifests, and automatic reasoning flag gating. Provider kinds (OpenAI Responses, Ollama, Anthropic Claude) surface dedicated controls—Ollama `think` toggles, Claude thinking budgets—and the `/BYOK` TUI modal plus CLI `models` subcommand flow through `codex-agentic-core::provider`.
- **Prompt overlays**: global/system prompts live in `example-system-prompts/` and load via `codex-agentic-core::prompt`, enabling fork-specific instructions without patching upstream.
- **ACP surface** (`cli acp`, `codex-agentic-core::acp`) implements the Agentic Control Plane JSON-RPC with shared slash commands, approval streaming, and BYOK-aware model selection.

## Build, Test, and Development Commands
- `just fmt` – run rustfmt across touched crates (required after Rust edits).
- `just fix -p codex-<crate>` – apply scoped Clippy fixes; only run plain `just fix` when shared crates change.
- `cargo test -p codex-<crate>` – execute crate-scoped tests; use `cargo test --all-features` after changes in `common`, `core`, or `protocol`.
- `cargo insta pending-snapshots -p codex-tui` / `cargo insta accept -p codex-tui` – review and accept TUI snapshot updates.
- `just install` – provision CLI conveniences (completions, dev dependencies) for local development.

## Release, Upgrades, and Diff References
- Continuous upgrade notes and validation steps live in `/Users/tonyholovka/workspace/codex-pro/designs/codex-pro/functional-design/00-IMPLEMENTATION-CHECKLIST.md`.
- Diff exports for upstream reconciliation accompany the checklist (see the “Diff archives” task group in the same document) and land under `/Users/tonyholovka/workspace/codex-pro/designs/codex-pro/codex-46/diffs/`.
- When preparing releases, follow the checklist gates (lint/build/test records, artifact snapshots) and document smoke results alongside diff references before tagging `codex-agentic-v0.46.0`.
