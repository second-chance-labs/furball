#!/usr/bin/env sh
set -eu
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
