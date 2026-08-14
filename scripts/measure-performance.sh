#!/usr/bin/env bash
set -euo pipefail

output_dir=${1:-target/performance}
if (($# > 0)); then
    shift
fi

cargo_bin=${CARGO:-"$HOME/.cargo/bin/cargo"}
if [[ ! -x "$cargo_bin" ]]; then
    printf 'measure-performance: cargo is not executable: %s\n' "$cargo_bin" >&2
    exit 2
fi

rustc_bin=${RUSTC:-"${cargo_bin%/cargo}/rustc"}
if [[ ! -x "$rustc_bin" ]]; then
    printf 'measure-performance: rustc is not executable: %s\n' "$rustc_bin" >&2
    exit 2
fi

export CARGO_TERM_COLOR=never
export LC_ALL=C
export NO_COLOR=1
export RUST_BACKTRACE=0
export RUSTC="$rustc_bin"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target/hdc15-performance}
export HDC15_CARGO_VERSION
HDC15_CARGO_VERSION=$("$cargo_bin" --version)
export HDC15_RUSTC_VERSION
HDC15_RUSTC_VERSION=$("$rustc_bin" --version)

exec "$cargo_bin" bench \
    --bench performance \
    --features perf-harness \
    --locked \
    -- \
    --output-dir "$output_dir" \
    "$@"
