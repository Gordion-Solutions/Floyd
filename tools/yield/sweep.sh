#!/usr/bin/env bash
# Run the yield sweep: every crate in tools/yield/crates.tsv at its pinned commit,
# `cargo floyd test --format=json --all-candidates <flags> .`, laid out the way
# tools/yield/render_yield.py reads it.
#
# Usage: tools/yield/sweep.sh OUT_DIR [crate ...]
#   OUT_DIR   new or existing sweep directory (outputs, clones, binaries)
#   crate     optional subset of names from crates.tsv (default: all)
# Environment:
#   LIMIT_SECS   per-crate wall-clock limit if `timeout`/`gtimeout` is on PATH (default 3600)
#   CARGO_BUILD_JOBS  passed through to cargo (set it to keep the machine usable)
#
# Takes hours (base64 and time dominate). Needs git, a nightly toolchain with
# llvm-tools (as for Floyd itself) and network access for the clones.
set -u
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
[ $# -ge 1 ] || { echo "usage: $0 OUT_DIR [crate ...]" >&2; exit 2; }
S=$(mkdir -p "$1" && cd "$1" && pwd); shift
LOG=$S/sweep.log
LIMIT_SECS=${LIMIT_SECS:-3600}
TO=$(command -v timeout || command -v gtimeout || true)

# 1. Build Floyd from this checkout into OUT_DIR/bin (cargo-floyd finds the shim next to itself).
(cd "$repo" && cargo build --release -p cargo-floyd) || exit 1
mkdir -p "$S/bin"
cp "$repo/target/release/cargo-floyd" "$repo/target/release/floyd-rustc-shim" "$S/bin/"
git -C "$repo" rev-parse HEAD > "$S/bin/git_rev"
git -C "$repo" diff --quiet HEAD -- floyd cargo-floyd || echo "WARNING: floyd/ or cargo-floyd/ has uncommitted changes; bin/git_rev understates them" >&2
rustc +nightly --version > "$S/TOOLCHAIN" 2>/dev/null || rustc --version > "$S/TOOLCHAIN"
echo "$(uname -s) $(uname -r) $(uname -m), CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-unset}" > "$S/MACHINE"
B=$S/bin/cargo-floyd

# 2. One run per crate.
grep -v '^#' "$here/crates.tsv" | while IFS=$'\t' read -r name url rev rel flags; do
  [ -n "$name" ] || continue
  if [ $# -gt 0 ] && ! printf '%s\n' "$@" | grep -qx "$name"; then continue; fi
  d=$S/$name; src=$d/src
  rm -rf "$d/out"; mkdir -p "$d/out"
  if [ ! -d "$src/.git" ]; then git clone --quiet "$url" "$src" < /dev/null || { echo "clone failed: $name" >&2; continue; }; fi
  git -C "$src" fetch --quiet origin "$rev" < /dev/null 2>/dev/null
  git -C "$src" checkout --quiet --force "$rev" || { echo "checkout failed: $name $rev" >&2; continue; }
  git -C "$src" clean -fdxq
  git -C "$src" rev-parse --short HEAD > "$d/out/git_rev"
  echo "$flags" > "$d/out/flags"
  echo "$(date +%FT%T) START $name flags='$flags'" >> "$LOG"
  # $flags is intentionally unquoted: it holds several arguments.
  (cd "$src/$rel" && ${TO:+$TO $LIMIT_SECS} "$B" floyd test --format=json --all-candidates $flags . \
     < /dev/null > "$d/out/span.json" 2> "$d/out/stderr.log")
  echo $? > "$d/out/exit_code"
  echo "$(date +%FT%T) END $name exit=$(cat "$d/out/exit_code")" >> "$LOG"
done
echo "$(date +%FT%T) SWEEP_DONE" >> "$LOG"
echo "Render with: python3 $here/render_yield.py $S"
