#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
  printf '%s\n' 'usage: build_drp.sh CANDIDATE_COMMIT (from the synced tree on nix, under the retained mise 2026.8.0)' >&2
  exit 1
fi
commit=$1
gates=$HOME/build/dios-gates
git -C "$gates/repo" fetch --quiet "$PWD" "$commit"
candidate=$gates/worktrees/at-candidate
if [ ! -e "$candidate" ]; then
  git -C "$gates/repo" worktree add --detach "$candidate" "$commit"
fi
git -C "$candidate" checkout --quiet --detach "$commit"
test -z "$(git -C "$candidate" status --porcelain=v1)"
test "$(git -C "$candidate" rev-parse HEAD)" = "$(git rev-parse "$commit")"
short=$(git -C "$candidate" rev-parse --short=7 HEAD)
control=$gates/targets/at-control-$short
(cd "$candidate" && CARGO_TARGET_DIR=$control cargo build --quiet --features bench --bin drp_product_harness --bin drp_gate_artifacts)
mkdir -p "$gates/harnesses"
for name in g2-base g4-base candidate; do
  product=$gates/worktrees/$name
  if [ "$name" = candidate ]; then
    product=$candidate
  fi
  test -z "$(git -C "$product" status --porcelain=v1)"
  harness=$gates/harnesses/at-$name-$short
  if [ ! -e "$harness" ]; then
    (cd "$candidate" && "$control/debug/drp_product_harness" "$product" "$harness")
  fi
  (cd "$harness" && CARGO_TARGET_DIR=$gates/targets/at-$name-$short cargo build --release --features bench --bin read_path_product) \
    > "$gates/targets/at-$name-$short.build.log" 2>&1
  sha256sum "$gates/targets/at-$name-$short/release/read_path_product"
done
sha256sum "$control/debug/drp_gate_artifacts" "$candidate/benches/read_path_product.rs" "$candidate/build.rs" \
  "$candidate/src/bin/drp_gate_artifacts.rs" "$gates/artifacts/drp_binding_input.bin"
printf 'short=%s\n' "$short"
