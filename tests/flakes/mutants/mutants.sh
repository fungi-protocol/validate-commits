#!/usr/bin/env sh
# shellcheck disable=SC2154 # Nix sets src, diff, tools and out.
set -eu
# `tools` puts the host's cargo and cargo-mutants on PATH.
eval "$tools"
export HOME="$TMPDIR"
cp -r "$src" "$TMPDIR/src"
chmod -R u+w "$TMPDIR/src"
# --list picks mutants from the diff without building or testing them.
cargo mutants --list --in-diff "$diff" --dir "$TMPDIR/src" >"$TMPDIR/mutants"
cat "$TMPDIR/mutants"
# There must be mutants, and all of them in code the commit changed or added.
grep -q . "$TMPDIR/mutants"
if grep -v -e ' changed ' -e ' added ' "$TMPDIR/mutants"; then
  echo "mutants outside the commit's changes" >&2
  exit 1
fi
cp "$TMPDIR/mutants" "$out"
