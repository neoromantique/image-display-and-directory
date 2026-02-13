# Code style and conventions
- Rust idioms: module-oriented layout (`src/<domain>/...`), `snake_case` for functions/modules/variables, `CamelCase` for structs/types.
- Keep UI responsive: async/background work for scanning/thumbnailing; avoid blocking GTK main thread.
- Error handling crates present: `anyhow` and `thiserror`; logging via `tracing`/`tracing-subscriber`.
- Comments are sparse and purposeful; favor clear code over verbose comments.
- Formatting/lints are not explicitly configured in repo; use standard Rust tooling defaults (`cargo fmt`, `cargo clippy`).
