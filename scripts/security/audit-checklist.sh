#!/usr/bin/env bash
# scripts/security/audit-checklist.sh
#
# Automated security audit checklist — closes #1388
#
# Runs every pre-audit item, emits a JSON report, and exits non-zero on any
# failure so it can gate CI.
#
# Usage
#   ./scripts/security/audit-checklist.sh [--report <path>]
#
# Environment variables
#   AUDIT_REPORT_PATH     output path for the JSON report (default: audit-report.json)
#   STELLAR_NETWORK       testnet | mainnet (default: testnet)
#   LENDING_CONTRACT_ID   optional; on-chain checks are skipped when unset
#   SKIP_CARGO_AUDIT      set to 1 to skip cargo-audit in offline environments

set -euo pipefail

REPORT_PATH="${AUDIT_REPORT_PATH:-audit-report.json}"
NETWORK="${STELLAR_NETWORK:-testnet}"
TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
PASS=0; FAIL=0; SKIP=0
RESULTS_JSON="["
FIRST=1

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

log_pass() { echo "[PASS]  $*"; }
log_fail() { echo "[FAIL]  $*"; }
log_skip() { echo "[SKIP]  $*"; }
log_info() { echo "[INFO]  $*"; }

append_result() {
  local name="$1" status="$2" detail="$3"
  [[ $FIRST -eq 0 ]] && RESULTS_JSON+=","
  FIRST=0
  # Simple JSON escaping (replace " with \")
  local ename="${name//\"/\\\"}"
  local edetail="${detail//\"/\\\"}"
  RESULTS_JSON+="{\"name\":\"${ename}\",\"status\":\"${status}\",\"detail\":\"${edetail}\"}"
}

record_pass() { ((PASS++)); log_pass "$1 — $2"; append_result "$1" "pass" "$2"; }
record_fail() { ((FAIL++)); log_fail "$1 — $2"; append_result "$1" "fail" "$2"; }
record_skip() { ((SKIP++)); log_skip "$1 — $2"; append_result "$1" "skip" "$2"; }

run_check() {
  local name="$1"; shift
  local output
  if output=$(eval "$*" 2>&1); then
    record_pass "$name" "$(echo "$output" | tail -1)"
  else
    record_fail "$name" "$(echo "$output" | tail -3 | tr '\n' ' ')"
  fi
}

has_tool() { command -v "$1" &>/dev/null; }

# ---------------------------------------------------------------------------
# 1. Toolchain checks
# ---------------------------------------------------------------------------

log_info "=== 1. Toolchain & formatting ==="

if has_tool cargo; then
  run_check "cargo:fmt-check" \
    "cargo fmt --manifest-path stellar-lend/Cargo.toml -- --check"

  run_check "cargo:clippy" \
    "cargo clippy --manifest-path stellar-lend/Cargo.toml -- -D warnings"

  if [[ "${SKIP_CARGO_AUDIT:-0}" != "1" ]]; then
    if has_tool cargo-audit; then
      run_check "cargo:audit" "cargo audit --manifest-path stellar-lend/Cargo.toml"
    else
      record_skip "cargo:audit" "cargo-audit not installed"
    fi
  else
    record_skip "cargo:audit" "SKIP_CARGO_AUDIT=1"
  fi

  if has_tool semgrep; then
    run_check "semgrep:soroban-rules" \
      "semgrep --config .semgrep/soroban-rules.yaml stellar-lend/contracts --error"
  else
    record_skip "semgrep:soroban-rules" "semgrep not installed"
  fi
else
  record_skip "cargo:fmt-check"      "cargo not installed"
  record_skip "cargo:clippy"         "cargo not installed"
  record_skip "cargo:audit"          "cargo not installed"
  record_skip "semgrep:soroban-rules" "cargo not installed"
fi

# ---------------------------------------------------------------------------
# 2. Source-code checks
# ---------------------------------------------------------------------------

log_info "=== 2. Source-code checks ==="

# require_auth() present in the main contract entrypoint
run_check "source:require-auth-present" \
  "grep -q 'require_auth' stellar-lend/contracts/lending/src/lib.rs"

# No bare .unwrap() in production modules
UNWRAP_COUNT=$(grep -rn '\.unwrap()' stellar-lend/contracts/lending/src/ \
  --include='*.rs' --exclude='*test*' --exclude='*_test.rs' 2>/dev/null | wc -l || echo 0)
if [[ "$UNWRAP_COUNT" -eq 0 ]]; then
  record_pass "source:no-unwrap-in-prod" "0 unwrap() calls found in production modules"
else
  record_fail "source:no-unwrap-in-prod" "$UNWRAP_COUNT unwrap() call(s) in production modules"
fi

# Timelock contract must exist and be non-empty
if [[ -s "contracts/timelock/src/lib.rs" ]]; then
  record_pass "source:timelock-present" "contracts/timelock/src/lib.rs exists"
else
  record_fail "source:timelock-present" "contracts/timelock/src/lib.rs missing or empty"
fi

# ---------------------------------------------------------------------------
# 3. Documentation checks
# ---------------------------------------------------------------------------

log_info "=== 3. Documentation checks ==="

if [[ -s "docs/runbooks/upgrade-and-migration.md" ]]; then
  record_pass "docs:upgrade-runbook" "docs/runbooks/upgrade-and-migration.md present"
else
  record_fail "docs:upgrade-runbook" "docs/runbooks/upgrade-and-migration.md missing"
fi

if grep -q "security/advisories" SECURITY.md 2>/dev/null; then
  record_pass "docs:security-md" "SECURITY.md contains disclosure channel"
else
  record_fail "docs:security-md" "SECURITY.md missing or lacks disclosure channel"
fi

if [[ -s "docs/storage.md" ]]; then
  record_pass "docs:storage-layout" "docs/storage.md present"
else
  record_fail "docs:storage-layout" "docs/storage.md missing"
fi

if [[ -s "docs/UPGRADE_AUTHORIZATION.md" ]]; then
  record_pass "docs:upgrade-authorization" "docs/UPGRADE_AUTHORIZATION.md present"
else
  record_fail "docs:upgrade-authorization" "docs/UPGRADE_AUTHORIZATION.md missing"
fi

# ---------------------------------------------------------------------------
# 4. On-chain checks (optional)
# ---------------------------------------------------------------------------

log_info "=== 4. On-chain checks ==="

if [[ -n "${LENDING_CONTRACT_ID:-}" ]] && has_tool stellar; then
  run_check "onchain:admin-set" \
    "stellar contract invoke --id '$LENDING_CONTRACT_ID' --network '$NETWORK' -- get_admin"

  PAUSED=$(stellar contract invoke \
    --id "$LENDING_CONTRACT_ID" --network "$NETWORK" \
    -- is_paused --pause_type 0 2>&1 || echo "error")
  if [[ "$PAUSED" == "false" ]]; then
    record_pass "onchain:not-globally-paused" "Protocol is live on $NETWORK"
  else
    record_fail "onchain:not-globally-paused" "Paused or query failed: $PAUSED"
  fi
else
  record_skip "onchain:admin-set"           "LENDING_CONTRACT_ID not set"
  record_skip "onchain:not-globally-paused" "LENDING_CONTRACT_ID not set"
fi

# ---------------------------------------------------------------------------
# 5. Write report
# ---------------------------------------------------------------------------

RESULTS_JSON+="]"
TOTAL=$((PASS + FAIL + SKIP))

cat > "$REPORT_PATH" <<EOF
{
  "generated_at": "$TIMESTAMP",
  "network": "$NETWORK",
  "summary": { "pass": $PASS, "fail": $FAIL, "skip": $SKIP, "total": $TOTAL },
  "checks": $RESULTS_JSON
}
EOF

echo ""
echo "╔══════════════════════════════════════════════╗"
echo "║      StellarLend Audit Checklist Report       ║"
echo "╠══════════════════════════════════════════════╣"
printf "║  ✅ PASS: %-5d ❌ FAIL: %-5d ⏭ SKIP: %-5d ║\n" "$PASS" "$FAIL" "$SKIP"
echo "╚══════════════════════════════════════════════╝"
echo "Report written to: $REPORT_PATH"

if [[ "$FAIL" -gt 0 ]]; then
  echo "ERROR: $FAIL check(s) failed. Review $REPORT_PATH for details."
  exit 1
fi
exit 0
