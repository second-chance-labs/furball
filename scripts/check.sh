#!/usr/bin/env sh
set -eu
msrv=$(sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml)
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
cargo "+$msrv" test
cargo "+$msrv" test --all-features
