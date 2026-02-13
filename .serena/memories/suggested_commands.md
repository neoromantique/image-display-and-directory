# Suggested commands
## Development
- `cargo run --release -- /path/to/media` : run app directly with a target directory.
- `cargo run -- /path/to/media` : faster iteration build.

## Quality checks
- `cargo fmt` : format code.
- `cargo check` : typecheck/compile validation.
- `cargo test` : run tests.
- `cargo clippy --all-targets --all-features` : lint pass.

## Packaging / CI-related
- `cargo vendor vendor` : vendor crates for Flatpak/CI flow.
- `flatpak-builder --user --install --force-clean build-dir flatpak/lt.gtw.idxd.json` : build/install local Flatpak.
- `flatpak run lt.gtw.idxd /path/to/media` : run Flatpak build.

## Linux utility commands
- `rg <pattern>` / `rg --files` for fast search.
- `ls`, `find`, `cd`, `git status`, `git diff` for standard local workflow.
