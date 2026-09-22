#!/usr/bin/env bash
# Fails when a crate enters or leaves the dependency tree without the baseline
# being updated in the same commit.
#
# Dependency creep arrives one innocuous pull request at a time, and a tree
# nobody is counting is a tree that only grows. This compares crate *names*,
# not versions, so routine upgrades stay quiet and only new attack surface
# trips it.
#
#   tools/check-deps.sh            compare against the baseline
#   tools/check-deps.sh --update   rewrite the baseline
set -euo pipefail

BASELINE=deps-baseline.txt

# Pinned to the target the Dockerfile and the systemd unit deploy on, so the
# list does not depend on who ran it. Dev-dependencies are excluded: they are
# not linked into the shipped binary. Build-dependencies are included, because
# a build script runs arbitrary code on the machine doing the build.
current() {
  cargo tree --workspace --locked \
      --edges normal,build \
      --target x86_64-unknown-linux-gnu \
      --prefix none \
    | sed 's/ (\*)$//' \
    | awk 'NF {print $1}' \
    | sort -u
}

if [ "${1:-}" = "--update" ]; then
  current > "$BASELINE"
  echo "wrote $BASELINE ($(wc -l < "$BASELINE") crates)"
  exit 0
fi

if [ ! -f "$BASELINE" ]; then
  echo "::error::$BASELINE is missing; run tools/check-deps.sh --update"
  exit 1
fi

added=$(comm -13 "$BASELINE" <(current))
removed=$(comm -23 "$BASELINE" <(current))

if [ -z "$added" ] && [ -z "$removed" ]; then
  echo "dependency tree unchanged ($(wc -l < "$BASELINE") crates)"
  exit 0
fi

# Removals are good news, but they still mean the baseline is stale, so both
# directions fail and both are fixed the same way.
for crate in $added; do
  echo "::error file=$BASELINE::$crate entered the dependency tree"
done
for crate in $removed; do
  echo "::error file=$BASELINE::$crate left the dependency tree"
done
echo
echo "If this is intended, run tools/check-deps.sh --update and commit $BASELINE."
exit 1
