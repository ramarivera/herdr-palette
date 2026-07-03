# AGENTS.md

## Project

`herdr-palette` is a Rust/Ratatui plugin for Herdr. It opens a fuzzy command
palette over live Herdr state and uses `herdr-pretty-which` for binding models,
theme data, and config parsing.

## Rules

- Keep the crate publishable with `cargo package --locked`.
- Prefer small, explicit Rust changes over broad rewrites.
- Do not weaken tests, snapshots, or dispatch safety to make CI pass.
- Treat shell execution paths in `src/tui.rs` and Herdr process dispatch in
  `src/dispatch.rs` as security-sensitive.
- Keep README install instructions centered on crates.io:
  `cargo install herdr-palette --locked`.
- Local checkout install instructions are allowed only as a development path,
  not the primary user path.
- If Herdr plugin manifest behavior changes, update `herdr-plugin.toml` and
  the README in the same change.

## Verification

Run these before claiming the repo is ready:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo package --locked
cargo audit
```

If `cargo audit` is unavailable locally, install `cargo-audit` or report that
the audit could not be run.
