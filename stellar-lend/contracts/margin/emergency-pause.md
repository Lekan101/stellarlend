# Protocol-Wide Emergency Pause

`src/emergency.rs` adds one switch that stops **every** isolated market at once.

## Why this exists

Each market in `src/isolated.rs` already carries its own `is_active` flag, which
isolates one market from the rest. That granularity is exactly what is missing
when an incident is protocol-wide:

- a bug in the shared collateral math,
- a depegged stablecoin used as collateral across several markets,
- an oracle feeding every market at once.

An operator then needs to stop all markets simultaneously, without editing each
market individually, and without losing track of which markets were already halted
for unrelated reasons.

## Components

- `EmergencyPause` — the pause engine. Owns the authority model, the
  per-operation switches, the lifecycle state, and the cascade bookkeeping.
- `MarketRegistry` — the market list plus the pause engine, and the only path
  money moves through once the pause exists. Every mutating entry point runs the
  pause gate *before* it touches a market.
- `PriceContext` — the prices and shared decimal scale the isolated-market math
  needs, bundled to keep the gated wrappers readable.

## State Machine

`Normal -> Halted -> Recovery -> Normal`

- `Normal` — regular operation.
- `Halted` — hard stop. Every gated operation is refused, market administration
  included, so a paused protocol cannot be reconfigured underneath the incident.
- `Recovery` — controlled unwind. Markets are running again, but only the
  operations that let a user get out are permitted.

`Halted -> Normal` is deliberately not a legal transition: it would let an
operator skip the unwind window and restart deposits against a protocol nobody
has inspected yet. `resume_all` therefore only succeeds from `Recovery`.

## Operation Policy by State

| Operation              | `Normal` | `Halted` | `Recovery`          |
| ---------------------- | -------- | -------- | ------------------- |
| `Deposit`              | allow    | block    | block               |
| `Borrow`               | allow    | block    | block               |
| `MarketAdmin`          | allow    | block    | block               |
| `Repay`                | allow    | block    | allow               |
| `Withdraw`             | allow    | block    | allow               |
| `Liquidation`          | allow    | block    | allow               |
| views                  | allow    | allow    | allow               |

`Liquidation` stays open in `Recovery` on purpose. A pause must never be the
reason bad debt keeps accruing: liquidators are how an unhealthy protocol
recovers, and closing that door turns a contained incident into a loss.

## The Cascade Is Reversible

Halting records exactly which markets were running, then flips their own
`is_active` flag. Flipping the market flag rather than only consulting the pause
means the per-market checks in `isolated.rs` stay a second line of defence: a
caller that reaches the isolated functions directly still sees a halted market.

Resuming restores only the markets the cascade suspended, so closing an incident
never silently switches on a market governance had deliberately taken down.

## Roles

- `admin` — governance-controlled address. Can halt, resume, rotate the
  guardian, and toggle any single operation switch.
- `guardian` — optional fast-response address set by the admin. Can halt the
  protocol and open the unwind path, but **cannot** resume, rotate authorities,
  or reconfigure switches.

That asymmetry is the point: a compromised guardian key can stop the protocol,
but it cannot stop it, un-pause it, and drain it.

## Authorized Calls

- `halt_all(env, caller, now, reason)` -> admin or guardian.
- `begin_recovery(env, caller)` -> admin or guardian, only valid from `Halted`.
- `resume_all(env, caller)` -> admin only, only valid from `Recovery`.
- `set_op_pause(env, caller, op, paused, now, cooldown_seconds)` -> admin only.
  `EmergencyOp::All` is rejected: the master switch moves through the lifecycle.
- `set_guardian(caller, guardian)` -> admin only; `None` clears the guardian.

## Per-Operation Switches

Each operation has its own switch, independent of the lifecycle state, so an
operator can stop borrows alone without a governance ceremony and without
pretending the protocol is in an incident.

- `cooldown_seconds == 0` — no cooldown. The switch stays engaged until it is
  lifted explicitly, which is the right default for a deliberate kill switch.
- `cooldown_seconds > 0` — a self-clearing fuse that releases at
  `now + cooldown_seconds`. A forgotten one-shot kill switch cannot stay engaged
  forever.

`resume_all` clears every switch, so the protocol never comes out of an incident
inheriting a switch that was set before it.

## Events

- Topic `emergency`, data `(from, to, caller, markets_suspended, at)`, published on
  every lifecycle transition. `markets_suspended` is what lets an indexer tell a
  protocol-wide halt from a single-market outage.
- Topic `op_pause`, data `(op, paused, expires_at, caller)`, published on every
  switch change.

The incident timestamp and reason recorded by the first `halt_all` are preserved
across re-halts, so the post-mortem keeps the original time the protocol stopped
rather than the last time somebody re-asserted the switch.

## Security Notes

- Rejected operations never half-apply: `MarketRegistry` works on a local copy of
  the market and writes it back only on success, so a blocked borrow cannot be
  the reason a market's debt accounting drifts.
- The pause check runs before the market lookup, so the incident state is never
  masked by a caller mistake.
- Ledger time is supplied by the caller on every gated call, which keeps the
  cooldown and halt-timestamp semantics honest and testable.
- The engine is pure data plus `Result` returns — no panics, no hidden state — so
  it behaves identically from a contract, a bot, or a simulation.

## Test Coverage Added

`src/tests.rs` covers:

- the cascade reaching every market and every operation being refused,
- unauthorized halts, and admin/guardian rotation,
- recovery keeping repay, withdraw, and liquidation open while deposits, borrows,
  and market administration stay shut,
- resume requiring the recovery step, and restoring only cascade-suspended
  markets,
- the original incident time surviving a re-halt,
- per-operation switches, their cooldown fuses, and the master switch being
  unreachable by hand,
- rejected operations leaving market accounting untouched,
- bad debt containment during a recovery liquidation,
- the lifecycle and switch events an indexer depends on.
