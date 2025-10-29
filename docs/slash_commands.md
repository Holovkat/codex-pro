## Slash commands

### What are slash commands?

Slash commands are shortcuts you can type in the chat composer that start with `/`. They let you switch models, adjust approval policies, surface memory, and run workspace utilities without leaving the TUI.

---

### Built-in slash commands

Use these commands to control Codex during an active session.

| Command | Purpose |
| --- | --- |
| `/model` | choose what model and reasoning effort to use |
| `/approvals` | choose what Codex can do without approval |
| `/review` | review current changes and find issues |
| `/new` | start a new chat during a conversation |
| `/init` | create an AGENTS.md file with instructions for Codex |
| `/compact` | summarize conversation to prevent hitting the context limit |
| `/undo` | ask Codex to undo a turn<sup>†</sup> |
| `/diff` | show git diff (including untracked files) |
| `/mention` | mention a file |
| `/status` | show current session configuration and token usage |
| `/mcp` | list configured MCP tools |
| `/index` | rebuild the semantic index |
| `/search-code` | run semantic code search and adjust the confidence threshold |
| `/memory-suggest` | list stored memories related to the current question |
| `/memory` | inspect and manage global context memory |
| `/byok` | manage custom model providers |
| `/rollout` | print the rollout file path |
| `/logout` | log out of Codex |
| `/feedback` | send logs to maintainers |
| `/quit` | exit Codex |
| `/exit` | exit Codex |

<sup>†</sup> `/undo` appears when the `BETA_FEATURE` environment variable is set.

---

### Commands that accept arguments

- `/search-code <query> [--min-confidence=<value>]` searches the semantic index.
- `/memory-suggest <query>` surfaces stored memories relevant to the current question.
