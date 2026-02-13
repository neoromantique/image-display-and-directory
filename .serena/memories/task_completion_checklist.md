# Task completion checklist
- Run `cargo fmt` after Rust changes.
- Run `cargo check` to catch compile issues quickly.
- Run `cargo test` when behavior or internal logic changed.
- Prefer running `cargo clippy --all-targets --all-features` for non-trivial changes.
- If packaging/build config changed, validate Flatpak build command still works.
- Summarize touched files and any follow-up risks in handoff.
