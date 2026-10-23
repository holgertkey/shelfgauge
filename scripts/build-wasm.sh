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

# Every dependency as in the committed lock file (the staged build below cannot check).
cargo metadata --locked --format-version 1 > /dev/null

# The crate version goes into symbol hashes and thus into the module's layout. Build
# from a copy of the two crates whose version has no `+BUILD`, so a build number bump
# (a one-line edit of Cargo.toml) does not change the committed module, and a release
# checkout (which drops it) builds the same bytes.
stage=$root/target/wasm-stage
rm -rf "$stage"
mkdir -p "$stage/crates"
cp -R "$root/crates/shelfgauge-core" "$root/crates/shelfgauge-wasm" "$stage/crates/"
# The lists and search results render from the library's templates.
cp -R "$root/templates" "$stage/"
cp "$root/Cargo.lock" "$stage/"
# The workspace manifest: only the workspace sections and the two profiles the build
# uses (the root package's sections are left out).
awk '
    /^\[/ { keep = ($0 ~ /^\[(workspace|workspace\.[a-z]+|profile\.release|profile\.wasm)\]$/) }
    /^default-members/ { next }
    /^version = "/ && keep { sub(/\+[0-9]+"/, "\"") }
    keep { print }
' "$root/Cargo.toml" > "$stage/Cargo.toml"
remap="$remap --remap-path-prefix=$stage=/shelfgauge"
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS=$remap
export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS

cd "$stage"
# Not --locked: the copied lock file loses the entries of the root package; versions
# of the remaining packages stay as locked.
CARGO_TARGET_DIR=$root/target cargo build -p shelfgauge-wasm --target wasm32-unknown-unknown --profile wasm
cd "$root"
rm -rf assets/hub/pkg
wasm-bindgen --target web --no-typescript --out-dir assets/hub/pkg --out-name shelfgauge \
    target/wasm32-unknown-unknown/wasm/shelfgauge_wasm.wasm
ls -l assets/hub/pkg
