# Contributing to Wren

1. Read [docs/design.md](docs/design.md), especially **§7 Invariants**. Changes
   that break an invariant will not be merged, however convenient.
2. Every query Wren answers must have a differential test against git
   (`crates/wren-core/tests/differential.rs`). git is the oracle.
3. New history queries must be built on derived data or an index. Never scan
   the commit log linearly.
4. `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
   must pass.

Reviews focus on intent and invariants first, then line-level details.
