#!/bin/sh
set -eu

export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-D warnings"
export RUSTDOCFLAGS="${RUSTDOCFLAGS:+$RUSTDOCFLAGS }-D warnings"

cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo clippy --locked --release --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo test --locked --release --all-targets --all-features
cargo doc --locked --all-features --no-deps --document-private-items
cargo run --locked --release -- --config pivigil.toml --check-config
