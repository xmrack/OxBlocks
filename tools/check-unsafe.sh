#!/usr/bin/env bash
# The memory-safety claim is compiler-enforced -- but only while every crate
# actually inherits the workspace lint. This fails if one stops.
#
# `#![forbid(unsafe_code)]` cannot be checked by grepping for the word "unsafe"
# in sources: the point is that the compiler rejects it, so a source grep would
# pass on a crate that had quietly dropped the lint and contains no unsafe
# *yet*. What matters is that the lint is still in force.
set -euo pipefail

fail=0

# The workspace must still declare it.
if ! grep -qE '^unsafe_code *= *"forbid"' Cargo.toml; then
  echo "::error file=Cargo.toml::workspace lints no longer forbid unsafe_code"
  fail=1
fi

# And every member crate must still inherit it. A [lints] table containing
# `workspace = true`, not merely the words appearing somewhere in the file.
for manifest in crates/*/Cargo.toml; do
  if ! awk '
      /^\[lints\]/       { inside = 1; next }
      /^\[/              { inside = 0 }
      inside && /^workspace[[:space:]]*=[[:space:]]*true/ { found = 1 }
      END                { exit !found }
    ' "$manifest"; then
    echo "::error file=$manifest::does not inherit workspace lints, so unsafe_code is not forbidden here"
    fail=1
  fi
done

if [ "$fail" -eq 0 ]; then
  echo "all $(ls -d crates/*/ | wc -l) crates inherit forbid(unsafe_code)"
fi
exit "$fail"
