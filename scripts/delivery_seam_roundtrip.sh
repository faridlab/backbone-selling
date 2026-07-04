#!/usr/bin/env bash
# Extension-contract §5 for the selling↔inventory delivery seam: prove the cross-module ACL/consumer
# wiring survives a regeneration of BOTH modules. Snapshots the user-owned seam files, regenerates
# selling AND inventory with --force, asserts every file is byte-identical, and re-runs the
# end-to-end seam test green.  Usage: DATABASE_URL=... bash scripts/delivery_seam_roundtrip.sh
set -euo pipefail
cd "$(dirname "$0")/.."

SELL_FILES=(
  src/application/service/selling_write_service.rs
  src/application/service/selling_events.rs
  tests/delivery_seam.rs
)
INV_FILES=(
  ../backbone-inventory/src/application/service/inventory_intake.rs
  ../backbone-inventory/src/application/service/inventory_read.rs
  ../backbone-inventory/src/application/service/inventory_write_service.rs
)

echo "→ snapshot seam consumer/ACL files (both modules)"
before=$(shasum -a 256 "${SELL_FILES[@]}" "${INV_FILES[@]}")

echo "→ regenerate BOTH modules (§5: survive regen of both) — inventory then selling"
( cd ../backbone-inventory && metaphor schema schema generate --force >/dev/null )
metaphor schema schema generate --force >/dev/null

echo "→ verify every seam file is byte-identical after regen"
after=$(shasum -a 256 "${SELL_FILES[@]}" "${INV_FILES[@]}")
if [ "$before" != "$after" ]; then
  echo "✗ FAIL: a seam file changed during regen"; diff <(echo "$before") <(echo "$after") || true; exit 1
fi
echo "  ✓ all ${#SELL_FILES[@]}+${#INV_FILES[@]} seam files unchanged"

echo "→ re-run the end-to-end delivery seam post-regen"
cargo test --test delivery_seam -- --test-threads=1 >/dev/null
echo "  ✓ selling→inventory→accounting→selling seam still green after regenerating both modules"
echo "✓ §5 round-trip proven for the delivery seam."
