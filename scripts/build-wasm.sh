#!/bin/sh
# Builds the hub reader app's WebAssembly module and its JS glue into assets/hub/pkg/
# (committed and embedded in the binary, so `cargo build` needs neither the wasm32
# target nor the wasm-bindgen CLI).
#
# Needs, once:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version 0.2.129 --locked
#
# The output must be byte for byte what CI builds (CI fails on a difference), so the
# Rust version is pinned here and in .github/workflows/ci.yml, and the paths of the
# checkout, the Cargo home and the sysroot's std sources are remapped out of panic
# messages. Run from anywhere.
set -eu

RUST_VERSION=1.99.0
BINDGEN_VERSION=0.2.129

root=$(cd "$(dirname "$0")/.." && pwd)
cargo_home=${CARGO_HOME:-$HOME/.cargo}

rustc_version=$(rustc --version | cut -d' ' -f2)
if [ "$rustc_version" != "$RUST_VERSION" ]; then
    echo "build-wasm: rustc $rustc_version, but the committed module is built with $RUST_VERSION" >&2
    echo "build-wasm: e.g. rustup toolchain install $RUST_VERSION and RUSTUP_TOOLCHAIN=$RUST_VERSION" >&2
    exit 1
fi
bindgen_version=$(wasm-bindgen --version | cut -d' ' -f2)
if [ "$bindgen_version" != "$BINDGEN_VERSION" ]; then
    echo "build-wasm: wasm-bindgen $bindgen_version, but $BINDGEN_VERSION is needed" >&2
    exit 1
fi

# With the rust-src component installed, std's paths point into the sysroot instead
# of /rustc/<commit>: map them back, so the result does not depend on it.
sysroot=$(rustc --print sysroot)
commit=$(rustc -vV | sed -n 's/^commit-hash: //p')
remap="--remap-path-prefix=$root=/shelfgauge --remap-path-prefix=$cargo_home=/cargo"
remap="$remap --remap-path-prefix=$sysroot/lib/rustlib/src/rust=/rustc/$commit"
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS=$remap
export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS

cd "$root"
cargo build --locked -p shelfgauge-wasm --target wasm32-unknown-unknown --profile wasm
rm -rf assets/hub/pkg
wasm-bindgen --target web --no-typescript --out-dir assets/hub/pkg --out-name shelfgauge \
    target/wasm32-unknown-unknown/wasm/shelfgauge_wasm.wasm
ls -l assets/hub/pkg
