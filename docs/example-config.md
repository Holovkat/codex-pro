# Example config.toml

Use this example configuration as a starting point. For an explanation of each field and additional context, see [Configuration](./config.md). Copy the snippet below to `~/.codex/config.toml` and adjust values as needed.

```toml
# Codex example configuration (config.toml)
#
# This file lists all keys Codex reads from config.toml, their default values,
# and concise explanations. Values here mirror the effective defaults compiled
# into the CLI. Adjust as needed.
#
# Notes
# - Root keys must appear before tables in TOML.
# - Optional keys that default to "unset" are shown commented out with notes.
# - MCP servers, profiles, and model providers are examples; remove or edit.

################################################################################
# Core Model Selection
################################################################################

# Primary model used by Codex. Default differs by OS; non-Windows defaults here.
# Linux/macOS default: "gpt-5-codex"; Windows default: "gpt-5".
model = "gpt-5-codex"

# Model used by the /review feature (code reviews). Default: "gpt-5-codex".
review_model = "gpt-5-codex"

# Provider id selected from [model_providers]. Default: "openai".
model_provider = "openai"

# Optional manual model metadata. When unset, Codex auto-detects from model.
# Uncomment to force values.
# model_context_window = 128000       # tokens; default: auto for model
# model_max_output_tokens = 8192      # tokens; default: auto for model
# model_auto_compact_token_limit = 0  # disable/override auto; default: model family specific

################################################################################
# Reasoning & Verbosity (Responses API capable models)
################################################################################

# Reasoning effort: minimal | low | medium | high (default: medium)
model_reasoning_effort = "medium"

# Reasoning summary: auto | concise | detailed | none (default: auto)
model_reasoning_summary = "auto"

# Text verbosity for GPT-5 family (Responses API): low | medium | high (default: medium)
model_verbosity = "medium"

# Force-enable reasoning summaries for current model (default: false)
model_supports_reasoning_summaries = false

# Force reasoning summary format: none | experimental (default: none)
model_reasoning_summary_format = "none"

################################################################################
# Instruction Overrides
################################################################################

# Additional user instructions appended after AGENTS.md. Default: unset.
# developer_instructions = ""

# Optional legacy base instructions override (prefer AGENTS.md). Default: unset.
# instructions = ""

# Inline override for the history compaction prompt. Default: unset.
# compact_prompt = ""

# Override built-in base instructions with a file path. Default: unset.
# experimental_instructions_file = "/absolute/or/relative/path/to/instructions.txt"

# Load the compact prompt override from a file. Default: unset.
# experimental_compact_prompt_file = "/absolute/or/relative/path/to/compact_prompt.txt"

################################################################################
# Approval & Sandbox
################################################################################

# When to ask for command approval:
# - untrusted: only known-safe read-only commands auto-run; others prompt
# - on-failure: auto-run in sandbox; prompt only on failure for escalation
# - on-request: model decides when to ask (default)
# - never: never prompt (risky)
approval_policy = "on-request"

# Filesystem/network sandbox policy for tool calls:
# - read-only (default)
# - workspace-write
# - danger-full-access (no sandbox; extremely risky)
sandbox_mode = "read-only"

# Extra settings used only when sandbox_mode = "workspace-write".
[sandbox_workspace_write]
# Additional writable roots beyond the workspace (cwd). Default: []
writable_roots = []
# Allow outbound network access inside the sandbox. Default: false
network_access = false
# Exclude $TMPDIR from writable roots. Default: false
exclude_tmpdir_env_var = false
# Exclude /tmp from writable roots. Default: false
exclude_slash_tmp = false

################################################################################
# Shell Environment Policy for spawned processes
################################################################################

[shell_environment_policy]
# inherit: all (default) | core | none
inherit = "all"
# Skip default excludes for names containing KEY/TOKEN (case-insensitive). Default: false
ignore_default_excludes = false
# Case-insensitive glob patterns to remove (e.g., "AWS_*", "AZURE_*"). Default: []
exclude = []
# Explicit key/value overrides (always win). Default: {}
set = {}
# Whitelist; if non-empty, keep only matching vars. Default: []
include_only = []
# Experimental: run via user shell profile. Default: false
experimental_use_profile = false

################################################################################
# History & File Opener
################################################################################

# Directory where Codex stores conversation history logs. Default: "~/.codex/history".
history_path = "~/.codex/history"

# If true, use `code` instead of `open` to open files (macOS only). Default: false
prefer_open_in_code = false

################################################################################
# Notifications
################################################################################

# Script run when Codex finishes a turn (non-empty string). Default: unset.
# notify_script = "/path/to/script.sh"

# Pass stdout of notify script through (default: false)
notify_passthrough = false

################################################################################
# MCP Servers (Model Context Protocol) – optional
################################################################################

[mcp_servers]

# --- Example: Stdio transport ---
# [mcp_servers.local_notes]
# type = "stdio"                            # required; "stdio" | "command" | "http"
# command = ["notes-server", "--serve-stdio"]  # required for stdio type
# env = { "API_KEY" = "value" }             # optional
# env_vars = ["API_KEY2"]                   # optional; allow-list for inheriting env
# cwd = "/Users/<user>/code/my-server"      # optional working directory
# startup_timeout_sec = 10.0                # optional; default 10 seconds
# tool_timeout_sec = 60.0                   # optional; default 60 seconds
# enabled_tools = ["search"]                # optional allow-list
# disabled_tools = ["write"]                # optional deny-list

# --- Example: Command transport ---
# [mcp_servers.git_inspector]
# type = "command"
# command = ["python3", "-m", "git_agent"]
# args = ["--stdio"]
# env = { "OPENAI_API_KEY" = "..." }
# env_vars = ["OPENAI_API_KEY", "GH_TOKEN"] # optional
# cwd = "/path/to/server"
# startup_timeout_sec = 10.0
# tool_timeout_sec = 60.0
# enabled_tools = ["search", "summarize"]
# disabled_tools = ["slow-tool"]

# --- Example: Streamable HTTP transport ---
# [mcp_servers.github]
# url = "https://github-mcp.example.com/mcp"  # required
# bearer_token_env_var = "GITHUB_TOKEN"        # optional; Authorization: Bearer <token>
# http_headers = { "X-Example" = "value" }    # optional static headers
# env_http_headers = { "X-Auth" = "AUTH_ENV" } # optional headers populated from env vars
# startup_timeout_sec = 10.0                   # optional
# tool_timeout_sec = 60.0                      # optional
# enabled_tools = ["list_issues"]             # optional allow-list

################################################################################
# Model Providers (extend/override built-ins)
################################################################################

# Built-ins include:
# - openai (Responses API; requires login or OPENAI_API_KEY via auth flow)
# - oss (Chat Completions API; defaults to http://localhost:11434/v1)

[model_providers]

# --- Example: override OpenAI with explicit base URL or headers ---
# [model_providers.openai]
# name = "OpenAI"
# base_url = "https://api.openai.com/v1"         # default if unset
# wire_api = "responses"                         # "responses" | "chat" (default varies)
# # requires_openai_auth = true                    # built-in OpenAI defaults to true
# # request_max_retries = 4                        # default 4; max 100
# # stream_max_retries = 5                         # default 5;  max 100
# # stream_idle_timeout_ms = 300000                # default 300_000 (5m)
# # experimental_bearer_token = "sk-example"      # optional dev-only direct bearer token
# # http_headers = { "X-Example" = "value" }
# # env_http_headers = { "OpenAI-Organization" = "OPENAI_ORGANIZATION", "OpenAI-Project" = "OPENAI_PROJECT" }

# --- Example: Azure (Chat/Responses depending on endpoint) ---
# [model_providers.azure]
# name = "Azure"
# base_url = "https://YOUR_PROJECT_NAME.openai.azure.com/openai"
# wire_api = "responses"                          # or "chat" per endpoint
# query_params = { api-version = "2025-04-01-preview" }
# env_key = "AZURE_OPENAI_API_KEY"
# # env_key_instructions = "Set AZURE_OPENAI_API_KEY in your environment"

# --- Example: Local OSS (e.g., Ollama-compatible) ---
# [model_providers.ollama]
# name = "Ollama"
# base_url = "http://localhost:11434/v1"
# wire_api = "chat"

################################################################################
# Profiles (named presets)
################################################################################

# Active profile name. When unset, no profile is applied.
# profile = "default"

[profiles]

# [profiles.default]
# model = "gpt-5-codex"
# model_provider = "openai"
# approval_policy = "on-request"
# sandbox_mode = "read-only"
# model_reasoning_effort = "medium"
# model_reasoning_summary = "auto"
# model_verbosity = "medium"
# chatgpt_base_url = "https://chatgpt.com/backend-api/"
# experimental_compact_prompt_file = "compact_prompt.txt"
# include_apply_patch_tool = false
# experimental_use_unified_exec_tool = false
# experimental_use_exec_command_tool = false
# experimental_use_rmcp_client = false
# experimental_use_freeform_apply_patch = false
# experimental_sandbox_command_assessment = false
# tools_web_search = false
# tools_view_image = true
# features = { unified_exec = false }

################################################################################
# Projects (trust levels)
################################################################################

# Mark specific worktrees as trusted. Only "trusted" is recognized.
[projects]
# [projects."/absolute/path/to/project"]
# trust_level = "trusted"

################################################################################
# OpenTelemetry (OTEL) – disabled by default
################################################################################

[otel]
# Include user prompt text in logs. Default: false
log_user_prompt = false
# Environment label applied to telemetry. Default: "dev"
environment = "dev"
# Exporter: none (default) | otlp-http | otlp-grpc
exporter = "none"

# Example OTLP/HTTP exporter configuration
# [otel]
# exporter = { otlp-http = {
#   endpoint = "https://otel.example.com/v1/logs",
#   protocol = "binary",                      # "binary" | "json"
#   headers = { "x-otlp-api-key" = "${OTLP_TOKEN}" }
# }}

# Example OTLP/gRPC exporter configuration
# [otel]
# exporter = { otlp-grpc = {
#   endpoint = "https://otel.example.com:4317",
#   headers = { "x-otlp-meta" = "abc123" }
# }}
```
