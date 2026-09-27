#!/usr/bin/env sh
# Builds a static (musl) release binary for a Linux target with clang + rust-lld,
# no per-target GCC toolchain needed (ring's C/asm is compiled by clang).
#
#   rustup target add aarch64-unknown-linux-musl
#   ./scripts/build-static.sh aarch64-unknown-linux-musl
#
# `test` as the second argument runs the test suite for that target instead
# (CI runs foreign targets under qemu-user via CARGO_TARGET_<TRIPLE>_RUNNER).
set -eu

target="${1:?usage: $0 <rust-target-triple> [test] [cargo args…]}"
shift
mode=build
if [ "${1:-}" = test ]; then
  mode=test
  shift
fi

case "$target" in
  x86_64-unknown-linux-musl) clang_target=x86_64-linux-musl ;;
  aarch64-unknown-linux-musl) clang_target=aarch64-linux-musl ;;
  armv7-unknown-linux-musleabihf) clang_target=armv7-linux-musleabihf ;;
  *) echo "unsupported target: $target" >&2; exit 1 ;;
esac

env_target=$(echo "$target" | tr 'a-z-' 'A-Z_')
lower_target=$(echo "$target" | tr '-' '_')

export "CC_${lower_target}=clang"
export "CFLAGS_${lower_target}=--target=${clang_target}"
# x86_64 musl links with the host toolchain; other targets with rust-lld.
if [ "$target" != "x86_64-unknown-linux-musl" ]; then
  export "CARGO_TARGET_${env_target}_LINKER=rust-lld"
fi

if [ "$mode" = test ]; then
  exec cargo test --locked --target "$target" "$@"
fi
cargo build --release --locked --target "$target" "$@"
file "${CARGO_TARGET_DIR:-target}/$target/release/mihomyak" 2>/dev/null || true
