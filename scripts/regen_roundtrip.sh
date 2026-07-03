#!/usr/bin/env bash
# Extension-contract §5, second clause — PROOF that a consumer's custom rule + the module's
# hand-authored surface survive a regeneration of the module. Snapshots the user-owned files,
# runs `metaphor schema schema generate --force`, and asserts (a) every user-owned file is byte-for-
# byte unchanged and (b) the extension-contract + seam tests are still green afterwards.
#
# Usage: DATABASE_URL=... bash scripts/regen_roundtrip.sh   (run from the module root)
set -euo pipefail
cd "$(dirname "$0")/.."

USER_OWNED=(
  src/application/service/selling_events.rs
  src/application/service/selling_gl.rs
  src/application/service/selling_write_service.rs
  src/application/service/consumer_credit_rule_custom.rs
  src/presentation/http/guarded_routes.rs
  tests/extension_contract.rs
  tests/order_to_cash.rs
  tests/gl_posting_seam.rs
  tests/selling_golden_cases.rs
  tests/integrity_probes.rs
)

echo "→ snapshot user-owned files"
before=$(shasum -a 256 "${USER_OWNED[@]}")

echo "→ regenerate BOTH modules (§5: survive regen of both) — accounting then selling"
( cd ../backbone-accounting && metaphor schema schema generate --force >/dev/null )
metaphor schema schema generate --force >/dev/null

echo "→ verify every user-owned file is byte-identical after regen"
after=$(shasum -a 256 "${USER_OWNED[@]}")
if [ "$before" != "$after" ]; then
  echo "✗ FAIL: a user-owned file changed during regen"
  diff <(echo "$before") <(echo "$after") || true
  exit 1
fi
echo "  ✓ all ${#USER_OWNED[@]} user-owned files unchanged"

echo "→ re-run the extension-contract + seam tests post-regen"
cargo test --test extension_contract --test gl_posting_seam --test order_to_cash -- --test-threads=1 >/dev/null
echo "  ✓ consumer rule + seam still green after regeneration"
echo "✓ §5 round-trip proven: the consumer's custom rule survives a regen of the module."
