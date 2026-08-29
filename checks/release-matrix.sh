#!/usr/bin/env bash
# Run the complete source release matrix, optionally followed by the real compositor check.
set -euo pipefail

usage() {
    printf '%s\n' \
        "usage: $0 --source-only" \
        "       $0 <wtype-compatible-input-driver> -- <compositor> [arguments...]" >&2
    exit 2
}

[[ $# -ge 1 ]] || usage

source_only=false
input_driver=
compositor=()
if [[ $1 == --source-only ]]; then
    [[ $# -eq 1 ]] || usage
    source_only=true
else
    [[ $# -ge 3 && $2 == -- ]] || usage
    input_driver=$1
    shift 2
    compositor=("$@")
fi

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$repo"

step() {
    printf 'release-gate=%s\n' "$1"
}

step format
cargo fmt --all -- --check

step check-all-targets-features
cargo check --workspace --all-targets --all-features --locked

step clippy-all-targets-features
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

step check-no-default-features
cargo check --no-default-features --locked

step check-matrix-launcher
cargo check --no-default-features --features matrix_launcher --locked

step check-custom-matrix-launcher
cargo check --no-default-features --features custom,matrix_launcher --all-targets --locked

step check-sway-feature-families
cargo check --no-default-features \
    --features 'workspaces+sway,keyboard+sway,bindmode+sway' --locked

step test-default-workspace
cargo test --workspace --all-targets --locked

step test-all-feature-workspace
cargo test --workspace --all-targets --all-features --locked

step doctest-all-features
cargo test --workspace --doc --all-features --locked

step package-launch-service
cargo package --manifest-path launch-service/Cargo.toml --locked

step package-launcher-core
cargo package --manifest-path launcher-core/Cargo.toml --locked

step package-launcher-gtk
cargo package --manifest-path launcher-gtk/Cargo.toml --locked \
    --config "patch.crates-io.cbar-launcher-core.path='$repo/launcher-core'" \
    --config "patch.crates-io.ironbar-launch-service.path='$repo/launch-service'"

step package-root
cargo package --locked \
    --config "patch.crates-io.cbar-launcher.path='$repo/launcher-gtk'" \
    --config "patch.crates-io.cbar-launcher-core.path='$repo/launcher-core'" \
    --config "patch.crates-io.ironbar-launch-service.path='$repo/launch-service'"

if command -v nix >/dev/null 2>&1; then
    step nix-flake-check
    nix flake check --no-write-lock-file
else
    printf 'release-gate=nix-flake-check unavailable\n' >&2
    exit 2
fi

if $source_only; then
    printf 'release-matrix=SOURCE_ONLY_PASS\n'
    exit 0
fi

step build-real-layer-binary
cargo build --all-features --locked --bin ironbar

target_dir=${CARGO_TARGET_DIR:-$repo/target}
if [[ $target_dir != /* ]]; then
    target_dir=$repo/$target_dir
fi

step real-layer-headless
checks/headless-session.sh "$target_dir/debug/ironbar" "$input_driver" -- "${compositor[@]}"

printf 'release-matrix=PASS\n'
