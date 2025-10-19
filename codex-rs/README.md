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

The Rust implementation is now the maintained Codex CLI and serves as the default experience. It includes a number of features that the legacy TypeScript CLI never supported.

### Config

Codex supports a rich set of configuration options. Note that the Rust CLI uses `config.toml` instead of `config.json`. See [`docs/config.md`](../docs/config.md) for details.

### Model Context Protocol Support

#### MCP client

Codex CLI functions as an MCP client that allows the Codex CLI and IDE extension to connect to MCP servers on startup. See the [`configuration documentation`](../docs/config.md#mcp_servers) for details.

#### MCP server (experimental)

Codex can be launched as an MCP _server_ by running `codex mcp-server`. This allows _other_ MCP clients to use Codex as a tool for another agent.

Use the [`@modelcontextprotocol/inspector`](https://github.com/modelcontextprotocol/inspector) to try it out:

```shell
npx @modelcontextprotocol/inspector codex mcp-server
```

Use `codex mcp` to add/list/get/remove MCP server launchers defined in `config.toml`, and `codex mcp-server` to run the MCP server directly.

### Notifications

You can enable notifications by configuring a script that is run whenever the agent finishes a turn. The [notify documentation](../docs/config.md#notify) includes a detailed example that explains how to get desktop notifications via [terminal-notifier](https://github.com/julienXX/terminal-notifier) on macOS.

### `codex exec` to run Codex programmatically/non-interactively

To run Codex non-interactively, run `codex exec PROMPT` (you can also pass the prompt via `stdin`) and Codex will work on your task until it decides that it is done and exits. Output is printed to the terminal directly. You can set the `RUST_LOG` environment variable to see more about what's going on.

### Use `@` for file search

Typing `@` triggers a fuzzy-filename search over the workspace root. Use up/down to select among the results and Tab or Enter to replace the `@` with the selected path. You can use Esc to cancel the search.

### Esc–Esc to edit a previous message

When the chat composer is empty, press Esc to prime “backtrack” mode. Press Esc again to open a transcript preview highlighting the last user message; press Esc repeatedly to step to older user messages. Press Enter to confirm and Codex will fork the conversation from that point, trim the visible transcript accordingly, and pre‑fill the composer with the selected user message so you can edit and resubmit it.

In the transcript preview, the footer shows an `Esc edit prev` hint while editing is active.

## Release, Upgrades, and Diff References
- Continuous upgrade notes and validation steps live in `/Users/tonyholovka/workspace/codex-pro/designs/codex-pro/functional-design/00-IMPLEMENTATION-CHECKLIST.md`.
- Diff exports for upstream reconciliation accompany the checklist (see the “Diff archives” task group in the same document) and land under `/Users/tonyholovka/workspace/codex-pro/designs/codex-pro/codex-46/diffs/`.
- When preparing releases, follow the checklist gates (lint/build/test records, artifact snapshots) and document smoke results alongside diff references before tagging downstream releases.

### `--cd`/`-C` flag

Sometimes it is not convenient to `cd` to the directory you want Codex to use as the "working root" before running Codex. Fortunately, `codex` supports a `--cd` option so you can specify whatever folder you want. You can confirm that Codex is honoring `--cd` by double-checking the **workdir** it reports in the TUI at the start of a new session.

### `--add-dir` flag

Need to work across multiple projects? Pass `--add-dir` one or more times to expose extra directories as writable roots for the current session while keeping the main working directory unchanged. For example:

```shell
codex --cd apps/frontend --add-dir ../backend --add-dir ../shared
```

Codex can now inspect and edit files in each listed directory without leaving the primary workspace.

### Shell completions

Generate shell completion scripts via:

```shell
codex completion bash
codex completion zsh
codex completion fish
```

### Experimenting with the Codex Sandbox

To test to see what happens when a command is run under the sandbox provided by Codex, we provide the following subcommands in Codex CLI:

```
# macOS
codex sandbox macos [--full-auto] [COMMAND]...

# Linux
codex sandbox linux [--full-auto] [COMMAND]...

# Legacy aliases
codex debug seatbelt [--full-auto] [COMMAND]...
codex debug landlock [--full-auto] [COMMAND]...
```

### Selecting a sandbox policy via `--sandbox`

The Rust CLI exposes a dedicated `--sandbox` (`-s`) flag that lets you pick the sandbox policy **without** having to reach for the generic `-c/--config` option:

```shell
# Run Codex with the default, read-only sandbox
codex --sandbox read-only

# Allow the agent to write within the current workspace while still blocking network access
codex --sandbox workspace-write

# Danger! Disable sandboxing entirely (only do this if you are already running in a container or other isolated env)
codex --sandbox danger-full-access
```

The same setting can be persisted in `~/.codex/config.toml` via the top-level `sandbox_mode = "MODE"` key, e.g. `sandbox_mode = "workspace-write"`.

## Code Organization

This folder is the root of a Cargo workspace. It contains quite a bit of experimental code, but here are the key crates:

- [`core/`](./core) contains the business logic for Codex. Ultimately, we hope this to be a library crate that is generally useful for building other Rust/native applications that use Codex.
- [`exec/`](./exec) "headless" CLI for use in automation.
- [`tui/`](./tui) CLI that launches a fullscreen TUI built with [Ratatui](https://ratatui.rs/).
- [`cli/`](./cli) CLI multitool that provides the aforementioned CLIs via subcommands.
>>>>>>> 4f46360aa (feat: add --add-dir flag for extra writable roots (#5335))
