#!/usr/bin/env bash
# Extension-contract §5 for the selling↔billing invoice seam (order-to-cash): prove the cross-module
# ACL/consumer wiring survives a regeneration of BOTH modules. Snapshots the user-owned seam files,
# regenerates selling AND billing with --force, asserts byte-identical, and re-runs the seam green.
# Usage: DATABASE_URL=... bash scripts/invoice_seam_roundtrip.sh
set -euo pipefail
cd "$(dirname "$0")/.."

SELL_FILES=(
  src/application/service/selling_write_service.rs
  src/application/service/selling_events.rs
  tests/invoice_seam.rs
)
BILL_FILES=(
  ../backbone-billing/src/application/service/billing_write_service.rs
  ../backbone-billing/src/application/service/billing_events.rs
)

echo "→ snapshot seam consumer/ACL files (both modules)"
before=$(shasum -a 256 "${SELL_FILES[@]}" "${BILL_FILES[@]}")

echo "→ regenerate BOTH modules (§5) — billing then selling"
( cd ../backbone-billing && metaphor schema schema generate --force >/dev/null )
metaphor schema schema generate --force >/dev/null

echo "→ verify every seam file is byte-identical after regen"
after=$(shasum -a 256 "${SELL_FILES[@]}" "${BILL_FILES[@]}")
if [ "$before" != "$after" ]; then
  echo "✗ FAIL: a seam file changed during regen"; diff <(echo "$before") <(echo "$after") || true; exit 1
fi
echo "  ✓ all ${#SELL_FILES[@]}+${#BILL_FILES[@]} seam files unchanged"

echo "→ re-run the end-to-end invoice seam post-regen"
cargo test --test invoice_seam -- --test-threads=1 >/dev/null
echo "  ✓ selling→billing→accounting→selling seam still green after regenerating both modules"
echo "✓ §5 round-trip proven for the invoice seam."
