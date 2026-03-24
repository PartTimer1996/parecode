# Project: parecode
Language: Rust
Test runner: `cargo test`
Key dependencies: clap, reqwest, tokio, serde, serde_json, toml, anyhow, futures-util

## Rust coding conventions

### Architecture & design
- **Functional style preferred** — favor pure functions with clear inputs/outputs over stateful objects
- **Single Responsibility Principle** — each function/module should do one thing well
- **Use abstractions** — extract reusable patterns; avoid duplication
- **Explicit over implicit** — clear types, explicit error handling, no hidden magic

### Code organization
- Group related functionality into logical sections with `// ── Section ──────` comment headers
- Struct definitions and implementations at the top, followed by helper functions
- Constants declared near the top, using `SCREAMING_SNAKE_CASE`
- Public API first, internal helpers below

### Error handling
- Use `Result<T, E>` for fallible operations; never panic in library code
- Use `anyhow::Result` for application-level errors where context matters
- Provide context with `.context("what failed")` when propagating errors
- Return meaningful error messages that help debugging

### Testing
- Tests live in `#[cfg(test)] mod tests { }` at the bottom of each file
- Test module structure:
  - Group tests by the component/function they test
  - Use descriptive test names: `test_<component>_<scenario>`
  - Comment sections with `// ── Component ────────────────`
- Test coverage expectations:
  - **Core logic**: comprehensive unit tests for all branches
  - **Public API**: test success cases, failure modes, edge cases
  - **Serialization**: roundtrip tests for serde structs
  - **Integration points**: where appropriate, not required for every function
- Test patterns:
  - Prefer `assert_eq!` over `assert!` for better failure messages
  - Use `#[tokio::test]` for async tests
  - Mock filesystem/external calls only when necessary; prefer real calls for unit tests
- Keep tests fast and deterministic

### Async code
- Use `async/await` consistently; avoid blocking calls in async contexts
- Prefer `tokio::fs` and `tokio::process::Command` over blocking equivalents
- Use `tokio::time::timeout` for operations that might hang
- Keep async boundaries clear and minimal

### Naming
- Types: `PascalCase`
- Functions/variables: `snake_case`
- Constants: `SCREAMING_SNAKE_CASE`
- Lifetimes: short and descriptive (`'a`, `'static`, `'input`)
- Use full words over abbreviations (prefer `config` over `cfg`, except in very local scope)

### Documentation
- Doc comments (`///`) for all public items
- Module-level doc comments (`//!`) explaining purpose and key concepts
- Code comments for non-obvious logic; prefer self-documenting code
- Examples in doc comments where helpful

### Dependencies
- Prefer crates from the core Rust ecosystem (tokio, serde, clap, etc.)
- Avoid unnecessary dependencies; consider maintenance burden
- Pin major versions; let Cargo handle minor/patch updates
