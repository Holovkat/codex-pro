# Codex Rust Workspace Development Guide

## Commands
```bash
# Build/lint/test
cargo build --workspace
just fmt                    # Format with imports_granularity=Item
just clippy                 # Lint with all features
just fix                    # Auto-fix clippy issues
just test                   # Run tests with cargo-nextest

# Single test
cargo nextest run --no-fail-fast -- test_name
cargo test --package codex-core -- test_name

# CLI
just codex [args]           # Run codex CLI
```

## Code Style

### Imports
External crates → Internal codex-* crates → std library

### Error Handling
```rust
use thiserror::Error;
pub type Result<T> = std::result::Result<T, CodexErr>;
#[derive(Error, Debug)] pub enum CodexErr { /* ... */ }
```

### Naming
- Crates: `codex-{name}`
- Types: PascalCase, functions: snake_case
- Modules: snake_case, constants: SCREAMING_SNAKE_CASE

### Module Structure
```rust
#![deny(clippy::print_stdout, clippy::print_stderr)]
pub mod error; mod internal;
pub use error::Result;
```

### TUI Rules
- Use ANSI colors, avoid RGB/Indexed
- No `white()`, `black()`, `yellow()` methods
- See `tui/styles.md` for palette

### Testing
- Tests in `tests/suite/`, support in `tests/common/`
- Use `cargo nextest`, `wiremock`, `serial_test`

### Lints
- No `.expect()`/`.unwrap()` (except tests)
- Edition 2024, `Item` import granularity
- Strict clippy rules enforced