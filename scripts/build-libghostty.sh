#!/usr/bin/env bash
# Build libghostty-vt with Zig 0.15.2.
#
# On macOS, Zig 0.15.2 (required by the Ghostty commit the crate pins) bundles LLVM 20
# headers. The Xcode 27 SDK's math.h defers INFINITY/NAN to a newer clang
# protocol, so Zig's own libc++ fails to compile against it. The Command Line
# Tools ship SDK 26.5, which still defines INFINITY directly. gpui needs
# Xcode's Metal compiler, so the override is scoped to this one crate.
#
# Cargo does not fingerprint DEVELOPER_DIR, so after this succeeds a plain
# `cargo build` / `cargo run` reuses the result until `cargo clean` or a bump
# of libghostty-vt.
set -euo pipefail
cd "$(dirname "$0")/.."

zig_version=0.15.2
platform="$(uname -s)"
if [[ -x "$HOME/.zvm/$zig_version/zig" ]]; then
  export PATH="$HOME/.zvm/$zig_version:$PATH"
elif [[ "$platform" == Darwin ]]; then
  export PATH="/opt/homebrew/opt/zig@0.15/bin:$PATH"
fi

if ! command -v zig >/dev/null 2>&1 || [[ "$(zig version)" != "$zig_version" ]]; then
  printf 'Zig %s is required to build libghostty-vt; install it with zvm or Homebrew.\n' "$zig_version" >&2
  exit 1
fi

# LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseFast comes from .cargo/config.toml.
if [[ "$platform" == Darwin ]]; then
  export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
  DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo build -p libghostty-vt-sys "$@"
else
  cargo build -p libghostty-vt-sys "$@"
fi
