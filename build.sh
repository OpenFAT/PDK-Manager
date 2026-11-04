#!/usr/bin/env bash
# Build the plugin for wasm32-unknown-unknown into ./pdk_manager.wasm (committed; the package
# manifest, schemify.project, names it: `plugin pdk-manager path pdk_manager.wasm`). Fails at 200 KB.
#
# Reproducible: CI rebuilds the module and fails when the committed one differs. The bytes
# depend only on the sources, Cargo.lock and the toolchain: build in `nix develop` (rustc and
# rust-lld pinned by flake.lock) or commit the `pdk-manager-wasm` artifact of the CI run. Host
# paths are remapped out of the module (panic locations would otherwise name this checkout).
# A toolchain without rust-lld (nixpkgs' rustc) links with wasm-ld instead, re-running the
# script under `nix shell nixpkgs#lld` when none is on PATH; the dev shell has rust-lld.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
host="$(rustc -vV | sed -n 's/^host: //p')"
pinned="$(sed -n 's/^rust-version = "\(.*\)"/\1/p' "$here/Cargo.toml")"
if ! rustc -V | grep -q "^rustc $pinned\(\.0\)\? "; then
  echo "warning: $(rustc -V) is not the pinned rustc $pinned (nix develop): the module will" \
    "not match the committed one byte for byte" >&2
fi
# Unit separator between flags: paths may contain spaces.
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$here=/Schemify-PDK-Manager"$'\x1f'"--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"

if [[ ! -x "$(rustc --print sysroot)/lib/rustlib/$host/bin/rust-lld" ]] \
   && [[ -z "${CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_LINKER:-}" ]]; then
  if command -v wasm-ld >/dev/null; then
    export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_LINKER=wasm-ld
  else
    exec nix shell nixpkgs#lld -c env CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_LINKER=wasm-ld "$0" "$@"
  fi
fi

cargo build --manifest-path "$here/Cargo.toml" --release --target wasm32-unknown-unknown --locked

wasm="${CARGO_TARGET_DIR:-$here/target}/wasm32-unknown-unknown/release/pdk_manager.wasm"
size=$(wc -c <"$wasm")
if (( size >= 200 * 1024 )); then
  echo "error: pdk_manager.wasm is $size bytes (limit 200 KB)" >&2
  exit 1
fi
cp "$wasm" "$here/pdk_manager.wasm"
echo "pdk_manager.wasm $size bytes"
