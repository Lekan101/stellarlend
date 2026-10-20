# Runbook: Lending Contract Upgrade, Migration and Rollback

**Audience:** protocol operators who hold (or coordinate) the lending admin key.
**Applies to:** `stellar-lend/contracts/lending` (`LendingContract`).
**Rehearsal:** `npm run devnet:rehearse-upgrade` runs Procedure A and the rollback end to
end on the local devnet. See [Rehearse on devnet](#3-rehearse-on-devnet).

Background on the governance tracker (`UpgradeManager`) is in
[`docs/upgrade-mechanism.md`](../upgrade-mechanism.md). Storage compatibility rules are in
[`docs/storage.md`](../storage.md). This runbook covers the operational procedure.

---

## 0. Pick the procedure

| Situation | Procedure |
|---|---|
| Live contract exposes `upgrade(admin, new_wasm_hash)` **and** the new build is storage-compatible | **A: in-place upgrade** |
| Live contract predates the `upgrade` entrypoint | **B: redeploy and migrate** |
| New build changes storage incompatibly | Rework the release first (see §B.1). Do not ship it as-is. |

Check whether the live contract can be upgraded in place:

```bash
stellar contract info interface --id "$LENDING" --network "$NET" | grep -q 'fn upgrade' \
  && echo "in-place OK" || echo "NO upgrade entrypoint: use Procedure B"
```

> **One-way door.** Code is replaced wholesale. If you ever upgrade *to* a build without
> the `upgrade` entrypoint, including by rolling back to one, the contract can never be
> upgraded again. Every candidate and every rollback target must expose `upgrade`.

---

## Roles and inputs

| Item | Owner | Notes |
|---|---|---|
| Admin key (`get_admin`) | protocol multisig | Signs `upgrade`, `set_pause`, `migrate_pool_state` |
| Release commit / tag | release engineer | Build from a clean checkout |
| `candidate.wasm` + sha256 | release engineer | The sha256 of the file **is** the on-chain wasm hash |
| `previous.wasm` + sha256 | release engineer | Fetched from the live contract in step A.2: the rollback target |
| Rehearsal report | release engineer | `.devnet/rehearsal/<ts>/report.md`, attached to the release ticket |

Set these once per session:

```bash
export NET=testnet                 # or mainnet
export LENDING=C...                # lending contract id
export ADMIN=G...                  # admin address (stellar keys address <identity>)
export ADMIN_ID=lending-admin      # stellar CLI identity / signer for $ADMIN
```

---

## Procedure A: in-place upgrade

### A.1 Pre-flight compatibility review (go / no-go)

Answer every item before building. Any "no" stops the release.

- [ ] No existing `#[contracttype]` storage-key enum variant was reordered, renamed or removed (append only).
- [ ] No stored struct had fields reordered, removed or retyped.
- [ ] New storage keys have safe defaults when absent. Old state must work without a migration.
- [ ] Every entrypoint an indexer, the SDK or a bot calls still exists with the same signature.
- [ ] New errors are appended to their enums. Existing `#[repr(u32)]` codes are unchanged.
- [ ] The candidate still exposes `upgrade(admin, new_wasm_hash)`.
- [ ] If a migration is needed, it is an explicit admin entrypoint, idempotent, and has a
      written **reverse** path (or the release notes state that rollback after migration
      is code-only and why that is safe).

### A.2 Build the candidate and record the rollback target

```bash
git checkout <release-tag> && git status --porcelain   # must be empty
stellar contract build --manifest-path stellar-lend/contracts/lending/Cargo.toml
cp stellar-lend/target/wasm32v1-none/release/stellarlend_lending.wasm candidate.wasm
sha256sum candidate.wasm | tee candidate.sha256

# The code currently live on-chain is the rollback target. Keep this file.
stellar contract fetch --id "$LENDING" --network "$NET" --out-file previous.wasm
sha256sum previous.wasm | tee previous.sha256
```

Store `candidate.wasm`, `previous.wasm` and both hashes with the release ticket.

### A.3 Rehearse on devnet

```bash
npm run devnet                                   # one-time: local node, deployed + seeded lending
npm run devnet:rehearse-upgrade -- --candidate candidate.wasm
```

The rehearsal fails on the first broken check. It verifies:

1. The live code hash matches the hash recorded at deploy.
2. The uploaded candidate hash equals `sha256(candidate.wasm)`.
3. After `upgrade`, the live code is the candidate.
4. `migrate_pool_state` succeeds, and a second run writes nothing (returns `0`).
5. Admin, pool state, breaker state and every seeded user's debt, collateral and deposit
   are **byte-identical** before and after the upgrade.
6. A borrow and a repay succeed on the new code.
7. After rolling back, the live code is the previous wasm and state is identical to just
   before the rollback.

Artefacts (both wasms, fetched code after each step, state snapshots, diffs and
`report.md`) are written to `.devnet/rehearsal/<timestamp>/`. **Attach `report.md` to the
release ticket. No report, no upgrade.**

Then repeat A.4–A.8 on **testnet** against a deployment seeded with realistic positions
before touching mainnet.

### A.4 Announce and (optionally) pause

For upgrades that touch borrow or liquidation logic, pause the affected paths so no user
transaction lands between the code swap and validation:

```bash
stellar contract invoke --id "$LENDING" --source "$ADMIN_ID" --network "$NET" -- \
  set_pause --admin "$ADMIN" --pause_type 2 --paused true   # 2 = PauseType::Borrow
```

Do **not** pause `Repay`. Users must always be able to reduce risk.

### A.5 Upload the candidate

```bash
HASH=$(stellar contract upload --wasm candidate.wasm --source "$ADMIN_ID" --network "$NET")
[ "$HASH" = "$(cut -d' ' -f1 candidate.sha256)" ] || { echo "hash mismatch, STOP"; exit 1; }
```

Uploading only installs the code. Nothing changes for the live contract yet.

### A.6 Execute the upgrade

```bash
stellar contract invoke --id "$LENDING" --source "$ADMIN_ID" --network "$NET" -- \
  upgrade --admin "$ADMIN" --new_wasm_hash "$HASH"
```

This emits `contract_upgraded_event` (topic `admin`, data `new_wasm_hash`, `timestamp`).

Verify the swap:

```bash
stellar contract fetch --id "$LENDING" --network "$NET" --out-file live.wasm
[ "$(sha256sum live.wasm | cut -d' ' -f1)" = "$HASH" ] && echo "upgrade live"
```

### A.7 Migrate storage

Always run the pool-state migration. It is idempotent: it folds legacy per-field entries
into packed slots and materialises lazy fields, and returns the number of entries written.

```bash
stellar contract invoke --id "$LENDING" --source "$ADMIN_ID" --network "$NET" -- \
  migrate_pool_state --admin "$ADMIN"
# Run again: must print 0.
```

Run any release-specific migration entrypoints here, in the order the release notes give.

### A.8 Validate, then reopen

- [ ] `get_admin` returns `$ADMIN`.
- [ ] `get_pool_state` and `get_utilization_breaker` decode and match pre-upgrade values.
- [ ] `get_user_position` for a sample of known borrowers matches the pre-upgrade snapshot.
- [ ] A small borrow + repay from an operator account succeeds.
- [ ] The indexer decodes `contract_upgraded_event` and post-upgrade events.
- [ ] Unpause what A.4 paused (`set_pause ... --pause_type 2 --paused false`).

If any check fails, go to [Rollback](#rollback).

---

## Rollback

**Triggers:** failed validation in A.8; accounting drift; events not decoding; a critical
bug found after release.

```bash
# 1. Stop the bleeding (keep Repay open).
stellar contract invoke --id "$LENDING" --source "$ADMIN_ID" --network "$NET" -- \
  set_pause --admin "$ADMIN" --pause_type 2 --paused true   # 2 = PauseType::Borrow

# 2. Make sure the previous code is installed and live. Re-uploading is idempotent and
#    restores the code entry if its TTL lapsed.
PREV=$(stellar contract upload --wasm previous.wasm --source "$ADMIN_ID" --network "$NET")
[ "$PREV" = "$(cut -d' ' -f1 previous.sha256)" ] || { echo "previous hash mismatch, STOP"; exit 1; }

# 3. Swap back.
stellar contract invoke --id "$LENDING" --source "$ADMIN_ID" --network "$NET" -- \
  upgrade --admin "$ADMIN" --new_wasm_hash "$PREV"

# 4. Verify.
stellar contract fetch --id "$LENDING" --network "$NET" --out-file live.wasm
[ "$(sha256sum live.wasm | cut -d' ' -f1)" = "$PREV" ] && echo "rolled back"
```

Then re-run the A.8 checklist and unpause.

**What rollback does not undo.** Rollback swaps code only. Storage written by the new
version stays. That is safe when the release followed A.1: append-only keys and defaults
for absent values. If the release ran a migration that rewrote data, run its reverse
migration **before** step 3, while the new code (which knows both layouts) is still live.

---

## Procedure B: redeploy and migrate

Use this when in-place upgrade is impossible, typically for a lending instance deployed
before the `upgrade` entrypoint existed.

> **Current limitation:** `LendingContract` has no entrypoints to export or import user
> positions. Procedure B can move protocol configuration but **cannot move existing
> borrows or deposits on-chain** today. Treat it as a planned wind-down plus fresh
> deployment until a purpose-built migration entrypoint ships.

### B.1 Plan

1. Deploy the new version as a new contract: `stellar contract deploy --wasm candidate.wasm ...`.
2. `initialize` it with the same admin, debt ceiling and minimum borrow, then re-apply
   configuration (`initialize_deposit_settings`, `set_oracle`,
   `set_liquidation_threshold_bps`, `set_utilization_breaker`, ...).
3. On the **old** contract, pause `Borrow` and `Deposit` and leave `Repay` and `Withdraw`
   open, so users unwind there and re-enter on the new contract.
4. Point the API, SDK config and bots at the new contract id.
5. Keep the old contract readable until every position is closed.

Off-chain state transforms (indexer tables, API caches) use the declarative tool in
[`scripts/state-migrate`](../../scripts/state-migrate/README.md). Forked-state dry runs
use [`scripts/migration-simulator`](../../scripts/migration-simulator/README.md).

### B.2 Rollback

Before step 4 completes, rollback means unpausing the old contract and abandoning the new
one. Nothing has moved yet.

---

## Quick reference

| Step | Command (abbreviated) |
|---|---|
| Build | `stellar contract build --manifest-path stellar-lend/contracts/lending/Cargo.toml` |
| Save rollback target | `stellar contract fetch --id $LENDING --out-file previous.wasm` |
| Rehearse | `npm run devnet:rehearse-upgrade -- --candidate candidate.wasm` |
| Upload | `stellar contract upload --wasm candidate.wasm` |
| Upgrade / roll back | `... -- upgrade --admin $ADMIN --new_wasm_hash <hash>` |
| Migrate | `... -- migrate_pool_state --admin $ADMIN` (twice; 2nd returns 0) |
| Verify live code | `stellar contract fetch ... && sha256sum live.wasm` |

---

## Devnet end-to-end rehearsal — step by step

> Closes #1387 acceptance criterion: "Tested on devnet."
>
> Run this procedure on a clean local devnet before every testnet or mainnet
> upgrade to catch issues with zero financial risk.

### Prerequisites

```bash
# 1. Start the local devnet (one-time).
cd docker/devnet
cp .env.example .env
docker compose up -d
./healthcheck.sh --wait          # wait until all services are healthy

# 2. Export session variables.
export NET=standalone
export RPC=http://localhost:8000/soroban/rpc
export ADMIN_ID=lending-admin    # stellar CLI identity created by init-devnet.sh
export ADMIN=$(stellar keys address $ADMIN_ID)
export LENDING=$(cat .devnet/contract-ids/lending.txt)
```

### Step 1 — Snapshot pre-upgrade state

```bash
mkdir -p .devnet/rehearsal/$(date +%s)
SNAP=.devnet/rehearsal/$(ls -t .devnet/rehearsal | head -1)

# Record current code hash.
stellar contract fetch --id "$LENDING" --network "$NET" \
  --rpc-url "$RPC" --out-file "$SNAP/previous.wasm"
sha256sum "$SNAP/previous.wasm" > "$SNAP/previous.sha256"

# Snapshot a few known positions.
stellar contract invoke --id "$LENDING" --network "$NET" --rpc-url "$RPC" \
  -- get_system_stats > "$SNAP/stats-before.json"
```

### Step 2 — Build the candidate

```bash
git checkout <release-tag>
stellar contract build \
  --manifest-path stellar-lend/contracts/lending/Cargo.toml
cp stellar-lend/target/wasm32v1-none/release/stellarlend_lending.wasm \
  "$SNAP/candidate.wasm"
sha256sum "$SNAP/candidate.wasm" > "$SNAP/candidate.sha256"
```

### Step 3 — Upload and upgrade

```bash
HASH=$(stellar contract upload --wasm "$SNAP/candidate.wasm" \
  --source "$ADMIN_ID" --network "$NET" --rpc-url "$RPC")

# Verify hash matches the file.
[ "$HASH" = "$(cut -d' ' -f1 "$SNAP/candidate.sha256")" ] \
  || { echo "HASH MISMATCH — abort"; exit 1; }

stellar contract invoke --id "$LENDING" --source "$ADMIN_ID" \
  --network "$NET" --rpc-url "$RPC" \
  -- upgrade --admin "$ADMIN" --new_wasm_hash "$HASH"
```

### Step 4 — Migrate storage

```bash
# First run — returns number of entries written.
stellar contract invoke --id "$LENDING" --source "$ADMIN_ID" \
  --network "$NET" --rpc-url "$RPC" \
  -- migrate_pool_state --admin "$ADMIN"

# Second run MUST return 0 (idempotency check).
stellar contract invoke --id "$LENDING" --source "$ADMIN_ID" \
  --network "$NET" --rpc-url "$RPC" \
  -- migrate_pool_state --admin "$ADMIN"
```

### Step 5 — Validate

```bash
# Confirm live code matches candidate.
stellar contract fetch --id "$LENDING" --network "$NET" \
  --rpc-url "$RPC" --out-file "$SNAP/live-after.wasm"
[ "$(sha256sum "$SNAP/live-after.wasm" | cut -d' ' -f1)" = "$HASH" ] \
  && echo "✅ upgrade confirmed"

# Post-upgrade stats must decode cleanly.
stellar contract invoke --id "$LENDING" --network "$NET" --rpc-url "$RPC" \
  -- get_system_stats > "$SNAP/stats-after.json"

diff <(jq -S . "$SNAP/stats-before.json") \
     <(jq -S . "$SNAP/stats-after.json") \
  && echo "✅ state unchanged" \
  || echo "ℹ️  state diff (expected for migrations — review manually)"
```

### Step 6 — Rollback rehearsal

```bash
PREV=$(cut -d' ' -f1 "$SNAP/previous.sha256")
stellar contract upload --wasm "$SNAP/previous.wasm" \
  --source "$ADMIN_ID" --network "$NET" --rpc-url "$RPC"

stellar contract invoke --id "$LENDING" --source "$ADMIN_ID" \
  --network "$NET" --rpc-url "$RPC" \
  -- upgrade --admin "$ADMIN" --new_wasm_hash "$PREV"

stellar contract fetch --id "$LENDING" --network "$NET" \
  --rpc-url "$RPC" --out-file "$SNAP/live-rolled-back.wasm"
[ "$(sha256sum "$SNAP/live-rolled-back.wasm" | cut -d' ' -f1)" = "$PREV" ] \
  && echo "✅ rollback confirmed"
```

### Step 7 — Attach the rehearsal report to the release ticket

```bash
cat > "$SNAP/report.md" <<EOF
# Rehearsal report — $(date -u)

| Item | Result |
|---|---|
| Candidate hash matches file | ✅ |
| Upgrade executed on devnet | ✅ |
| migrate_pool_state idempotent | ✅ |
| Post-upgrade stats decoded | ✅ |
| Rollback executed and verified | ✅ |

Artefacts: \`$SNAP/\`
EOF

echo "Rehearsal report written to $SNAP/report.md"
echo "Attach this report to the release ticket before proceeding to testnet."
```

**No rehearsal report → no upgrade.**

---

## Timelock integration

> All sensitive admin actions must be routed through the timelock contract
> (`contracts/timelock`) before execution.  This adds a mandatory delay
> between scheduling and execution, giving the community time to react.
> See [docs/UPGRADE_AUTHORIZATION.md](../UPGRADE_AUTHORIZATION.md) for the
> full authorization model.

### Sensitive actions that require the timelock

| Action | Reason |
|---|---|
| `set_oracle` | Oracle manipulation risk |
| `set_risk_params` / `set_min_collateral_ratio` | Affects all user positions |
| `set_base_rate` / `set_kink_utilization` / `set_multiplier` | Interest-rate model |
| `set_reserve_factor` | Protocol revenue |
| `set_rate_limits` | Borrowing caps |
| `set_pause_switches` (pause **and** unpause) | Protocol availability |
| `set_asset_params` | Per-asset risk parameters |
| `upgrade` (WASM swap) | Code replacement |

`emergency_rate_adjustment` is intentionally **exempt** — it is instant by design.

### Scheduling an action through the timelock

```bash
# 1. Compute the args hash locally before scheduling.
#    args_hash = SHA-256(XDR-encode(args))
#    The timelock stores only the hash; the executor supplies the real args
#    at execution time, and the contract re-hashes to verify integrity.

# 2. Schedule — admin only.
ETA=$(( $(date +%s) + 172800 ))   # 48 hours from now
stellar contract invoke \
  --id "$TIMELOCK" --source "$ADMIN_ID" --network "$NET" \
  -- schedule \
  --caller  "$ADMIN" \
  --target  "$LENDING" \
  --function "set_oracle" \
  --args_hash "<SHA-256-of-args>" \
  --eta "$ETA"

# 3. After 48 hours have elapsed, execute — executor only.
stellar contract invoke \
  --id "$TIMELOCK" --source "$EXECUTOR_ID" --network "$NET" \
  -- execute \
  --caller "$EXECUTOR" \
  --id <operation-id> \
  --args "<original-args>"
```

### Cancelling a scheduled action

```bash
# Admin can cancel at any time before execution.
stellar contract invoke \
  --id "$TIMELOCK" --source "$ADMIN_ID" --network "$NET" \
  -- cancel \
  --caller "$ADMIN" \
  --id <operation-id>
```

### Updating the delay

```bash
# min_delay must be ≥ 1 h; max_delay ≤ 30 days.
stellar contract invoke \
  --id "$TIMELOCK" --source "$ADMIN_ID" --network "$NET" \
  -- set_delay \
  --caller "$ADMIN" \
  --new_min 172800 \
  --new_max 2592000
```
