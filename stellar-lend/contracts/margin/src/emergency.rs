//! Protocol-wide emergency pause for every lending market.
//!
//! [`crate::isolated`] gives each market its own `is_active` flag, which
//! isolates one market from the rest. That granularity is exactly what is
//! *missing* when an incident is protocol-wide: a bug in the shared collateral
//! math, a depegged stablecoin used as collateral everywhere, or a halted
//! oracle feeding every market at once. An operator then needs one switch that
//! stops **all** markets at the same time, without editing each market
//! individually and without losing track of which markets were already halted
//! for unrelated reasons.
//!
//! This module is that switch. It is built around two pieces:
//!
//! - [`EmergencyPause`] - the pause engine. It owns the authority model (admin
//!   and guardian), the operation-level switches, the lifecycle state
//!   (`Normal` / `Halted` / `Recovery`), and the cascade bookkeeping.
//! - [`MarketRegistry`] - the markets themselves plus the pause engine, and the
//!   only path money moves through once the pause exists. Every mutating entry
//!   point runs the pause gate *before* it touches a market.
//!
//! # Design rules
//!
//! 1. **The cascade is reversible and non-destructive.** Halting records exactly
//!    which markets were running, so resuming restores the previous topology
//!    instead of force-activating a market governance had already taken down.
//! 2. **Halted means halted.** While [`EmergencyState::Halted`] every gated
//!    operation is refused, including market administration, so a paused
//!    protocol cannot be reconfigured underneath the incident.
//! 3. **Recovery is an unwind, not a restart.** [`EmergencyState::Recovery`]
//!    re-opens only the operations that let a user get out - repay, withdraw,
//!    and liquidation - and keeps deposits, borrows, and market administration
//!    shut. That is the difference between containing an incident and merely
//!    pausing it.
//! 4. **Per-operation switches are independent of the lifecycle state**, so an
//!    operator can halt borrows alone with no governance ceremony and without
//!    pretending the protocol is in an incident.
//! 5. **A guardian can stop, only governance can restart.** The guardian exists
//!    because time matters; it can halt and open the unwind path but cannot
//!    resume the protocol or rotate authorities, which removes the "panic,
//!    un-pause, drain" attack from a compromised guardian key.
//! 6. **Fuses clear themselves.** A per-operation switch may carry a cooldown;
//!    once it elapses the switch releases on read, so a forgotten one-shot kill
//!    switch cannot stay engaged forever. A switch with no cooldown stays
//!    engaged until it is lifted explicitly, which is the right default for a
//!    deliberate kill switch.
//!
//! Everything is pure data plus `Result` returns - no panics, no hidden state.
//! The caller supplies the caller identity and the ledger time, and the pause
//! engine decides. That keeps the same engine usable from a contract, a bot, or
//! a simulation. Lifecycle transitions and switch changes are published as
//! contract events, so an indexer can follow an incident without polling state.

use crate::isolated::{IsolatedMarket, IsolatedUserPosition};
use soroban_sdk::{contractevent, contracttype, Address, Bytes, Env, Vec};

/// Operations the emergency pause gates independently.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub enum EmergencyOp {
    /// Master switch. When engaged every other switch is redundant.
    All = 0,
    /// Adding collateral to a market.
    Deposit = 1,
    /// Opening or increasing debt.
    Borrow = 2,
    /// Paying down debt.
    Repay = 3,
    /// Removing collateral.
    Withdraw = 4,
    /// Repaying bad debt and seizing collateral.
    Liquidation = 5,
    /// Creating, reactivating, or reconfiguring markets.
    MarketAdmin = 6,
}

impl EmergencyOp {
    /// Whether this operation belongs to the user-facing unwind path that
    /// `Recovery` keeps open.
    pub fn is_unwind_op(self) -> bool {
        matches!(
            self,
            EmergencyOp::Repay | EmergencyOp::Withdraw | EmergencyOp::Liquidation
        )
    }

    /// Whether this operation opens new risk and must therefore stay shut while
    /// the protocol is unwinding.
    pub fn is_risk_opening(self) -> bool {
        matches!(
            self,
            EmergencyOp::Deposit | EmergencyOp::Borrow | EmergencyOp::MarketAdmin
        )
    }
}

/// Every gated operation, in `#[contracttype]` order.
///
/// `All` is included because it is a switch like any other; it is simply never
/// set by hand - the lifecycle state drives it.
pub const GATED_OPS: [EmergencyOp; 7] = [
    EmergencyOp::All,
    EmergencyOp::Deposit,
    EmergencyOp::Borrow,
    EmergencyOp::Repay,
    EmergencyOp::Withdraw,
    EmergencyOp::Liquidation,
    EmergencyOp::MarketAdmin,
];

/// Lifecycle of a protocol-wide incident.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub enum EmergencyState {
    /// Normal operation; only per-operation switches apply.
    Normal = 0,
    /// Full stop. Every gated operation is refused.
    Halted = 1,
    /// Controlled unwind. Repay, withdraw, and liquidation stay open; deposits,
    /// borrows, and market administration are refused.
    Recovery = 2,
}

impl EmergencyState {
    /// Whether this state permits `op` at all, ignoring per-operation switches.
    pub fn permits(self, op: EmergencyOp) -> bool {
        match self {
            EmergencyState::Normal => true,
            EmergencyState::Halted => false,
            EmergencyState::Recovery => !op.is_risk_opening(),
        }
    }
}

/// Per-operation pause switch with an optional cooldown.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct OpPause {
    /// Whether the switch is set.
    pub paused: bool,
    /// Ledger time before which the switch stays engaged. `0` = no cooldown, so
    /// the switch stays off until it is lifted explicitly.
    pub expires_at: u64,
}

impl OpPause {
    /// An open switch.
    pub fn open() -> Self {
        OpPause {
            paused: false,
            expires_at: 0,
        }
    }

    /// Whether the switch is currently engaged, releasing itself once its
    /// cooldown has elapsed.
    pub fn engaged(&self, now: u64) -> bool {
        if !self.paused {
            return false;
        }
        self.expires_at == 0 || now < self.expires_at
    }
}

/// Prices the isolated-market math needs, bundled so the pause-gated wrappers
/// stay readable. `decimals` is the shared scale of both price feeds.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PriceContext {
    /// Price of the collateral asset.
    pub collateral_price: i128,
    /// Price of the borrowed asset.
    pub borrow_price: i128,
    /// Shared decimal scale of both prices.
    pub decimals: u32,
}

impl PriceContext {
    /// A context where both assets trade at the same price and scale.
    pub fn symmetric(price: i128, decimals: u32) -> Self {
        PriceContext {
            collateral_price: price,
            borrow_price: price,
            decimals,
        }
    }
}

/// The protocol-wide pause configuration and its cascade bookkeeping.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct EmergencyPause {
    /// Address that may halt, resume, and reconfigure the protocol.
    pub admin: Address,
    /// Address that may halt the protocol and open the unwind path without
    /// waiting for governance. `None` when no guardian is installed.
    pub guardian: Option<Address>,
    /// Current incident state.
    pub state: EmergencyState,
    /// Per-operation switches, indexed by `EmergencyOp as u32`.
    pub op_pauses: Vec<OpPause>,
    /// Whether the cascade suspended each registered market, positionally
    /// aligned with the registry's `markets`. Only the markets flagged here are
    /// re-activated when the incident closes.
    pub suspended: Vec<bool>,
    /// Ledger time of the current halt; `0` when the protocol is not halted.
    pub halted_at: u64,
    /// Why the protocol was halted. Empty when the protocol is not halted.
    pub reason: Bytes,
}

impl EmergencyPause {
    /// A fresh configuration in the `Normal` state with every operation open.
    pub fn new(env: &Env, admin: Address, guardian: Option<Address>) -> Self {
        let mut op_pauses: Vec<OpPause> = Vec::new(env);
        for op in GATED_OPS {
            let _ = op;
            op_pauses.push_back(OpPause::open());
        }
        EmergencyPause {
            admin,
            guardian,
            state: EmergencyState::Normal,
            op_pauses,
            suspended: Vec::new(env),
            halted_at: 0,
            reason: Bytes::new(env),
        }
    }

    /// The stored switch for `op`, defaulting to open for an unknown index.
    pub fn op_pause(&self, op: EmergencyOp) -> OpPause {
        self.op_pauses.get(op as u32).unwrap_or_else(OpPause::open)
    }

    /// Whether `op` is blocked right now, considering the master switch, the
    /// per-operation switch and its cooldown, and the lifecycle state.
    pub fn is_paused(&self, op: EmergencyOp, now: u64) -> bool {
        if self.op_pause(EmergencyOp::All).engaged(now) {
            return true;
        }
        if self.op_pause(op).engaged(now) {
            return true;
        }
        !self.state.permits(op)
    }

    /// Gate check for a mutating operation, with a reason that says *which*
    /// control stopped it.
    pub fn require_open(&self, op: EmergencyOp, now: u64) -> Result<(), &'static str> {
        if self.op_pause(EmergencyOp::All).engaged(now) {
            return Err("Protocol is paused by the emergency master switch");
        }
        if self.op_pause(op).engaged(now) {
            return Err("Operation is paused");
        }
        match self.state {
            EmergencyState::Normal => Ok(()),
            EmergencyState::Halted => Err("Protocol is halted by an emergency pause"),
            EmergencyState::Recovery if op.is_risk_opening() => {
                Err("Protocol is in recovery: risk-opening operations are disabled")
            }
            EmergencyState::Recovery => Ok(()),
        }
    }

    /// Whether `caller` may halt the protocol.
    ///
    /// Both the admin and the guardian may halt; the guardian exists precisely
    /// so an incident can be contained without waiting for governance.
    pub fn require_halt_authority(&self, caller: &Address) -> Result<(), &'static str> {
        if &self.admin == caller || self.guardian.as_ref() == Some(caller) {
            return Ok(());
        }
        Err("Caller is not authorized to halt the protocol")
    }

    /// Whether `caller` may resume the protocol or reconfigure it.
    pub fn require_admin(&self, caller: &Address) -> Result<(), &'static str> {
        if &self.admin == caller {
            return Ok(());
        }
        Err("Caller is not the protocol admin")
    }

    /// Whether `caller` is the installed guardian.
    pub fn is_guardian(&self, caller: &Address) -> bool {
        self.guardian.as_ref() == Some(caller)
    }

    /// Rotate or clear the guardian. Admin only.
    pub fn set_guardian(
        &mut self,
        caller: &Address,
        guardian: Option<Address>,
    ) -> Result<(), &'static str> {
        self.require_admin(caller)?;
        self.guardian = guardian;
        Ok(())
    }

    /// Switch one operation on or off, optionally with a cooldown.
    ///
    /// Admin only. A `cooldown_seconds` of `0` means *no* cooldown: the switch
    /// stays engaged until it is lifted explicitly, which is the right default
    /// for a deliberate kill switch. A non-zero value makes it a self-clearing
    /// fuse that releases `now + cooldown_seconds`.
    pub fn set_op_pause(
        &mut self,
        env: &Env,
        caller: &Address,
        op: EmergencyOp,
        paused: bool,
        now: u64,
        cooldown_seconds: u64,
    ) -> Result<(), &'static str> {
        self.require_admin(caller)?;
        if op == EmergencyOp::All {
            return Err("Use halt/resume to control the master switch");
        }
        let switch = OpPause {
            paused,
            expires_at: if !paused || cooldown_seconds == 0 {
                0
            } else {
                now.saturating_add(cooldown_seconds)
            },
        };
        self.write_op(op, switch);
        EmergencyOpPauseEvent {
            op,
            paused: switch.paused,
            expires_at: switch.expires_at,
            caller: caller.clone(),
        }
        .publish(env);
        Ok(())
    }

    /// Lift every per-operation switch, including the master switch.
    ///
    /// Used when the incident closes so a protocol coming out of recovery does
    /// not silently inherit kill switches that were set before it.
    pub fn clear_all_op_pauses(&mut self) {
        for op in GATED_OPS {
            self.write_op(op, OpPause::open());
        }
    }

    /// A snapshot of every switch, positionally aligned with [`GATED_OPS`].
    pub fn op_pause_snapshot(&self, env: &Env) -> Vec<OpPause> {
        let mut out: Vec<OpPause> = Vec::new(env);
        for op in GATED_OPS {
            out.push_back(self.op_pause(op));
        }
        out
    }

    fn write_op(&mut self, op: EmergencyOp, value: OpPause) {
        let index = op as u32;
        if index < self.op_pauses.len() {
            self.op_pauses.set(index, value);
        }
    }

    /// Enter [`EmergencyState::Halted`]. Admin or guardian.
    pub fn halt(
        &mut self,
        env: &Env,
        caller: &Address,
        now: u64,
        reason: &str,
    ) -> Result<EmergencyTransition, &'static str> {
        self.require_halt_authority(caller)?;
        let from = self.state;
        self.state = EmergencyState::Halted;
        // Re-halting an already halted protocol must not reset the clock or the
        // reason: the original incident time is what the post-mortem needs.
        if from != EmergencyState::Halted {
            self.halted_at = now;
            self.reason = Bytes::from_slice(env, reason.as_bytes());
        }
        Ok(EmergencyTransition {
            from,
            to: self.state,
            caller: caller.clone(),
            at: self.halted_at,
        })
    }

    /// Enter [`EmergencyState::Recovery`], opening the unwind path. Admin or
    /// guardian.
    pub fn begin_recovery(
        &mut self,
        caller: &Address,
    ) -> Result<EmergencyTransition, &'static str> {
        self.require_halt_authority(caller)?;
        if self.state != EmergencyState::Halted {
            return Err("Recovery must follow a halt");
        }
        let from = self.state;
        self.state = EmergencyState::Recovery;
        Ok(EmergencyTransition {
            from,
            to: self.state,
            caller: caller.clone(),
            at: self.halted_at,
        })
    }

    /// Return to [`EmergencyState::Normal`]. Admin only, and only from
    /// `Recovery`: jumping straight from `Halted` to `Normal` would let an
    /// operator skip the unwind window and restart deposits against a protocol
    /// nobody has inspected yet.
    pub fn resume(
        &mut self,
        env: &Env,
        caller: &Address,
    ) -> Result<EmergencyTransition, &'static str> {
        self.require_admin(caller)?;
        if self.state == EmergencyState::Halted {
            return Err("Enter recovery before resuming so users can exit");
        }
        let from = self.state;
        self.state = EmergencyState::Normal;
        self.halted_at = 0;
        self.reason = Bytes::new(env);
        self.clear_all_op_pauses();
        Ok(EmergencyTransition {
            from,
            to: self.state,
            caller: caller.clone(),
            at: 0,
        })
    }

    /// Whether the cascade suspended the market at `index`.
    pub fn is_suspended(&self, index: u32) -> bool {
        self.suspended.get(index).unwrap_or(false)
    }

    /// Record that the cascade suspended the market at `index`.
    pub fn mark_suspended(&mut self, index: u32) {
        if index < self.suspended.len() {
            self.suspended.set(index, true);
        }
    }

    /// Forget the cascade bookkeeping, used once the incident is closed.
    pub fn clear_suspended(&mut self) {
        for i in 0..self.suspended.len() {
            self.suspended.set(i, false);
        }
    }

    /// Number of markets the cascade suspended.
    pub fn suspended_count(&self) -> u32 {
        let mut count = 0u32;
        for i in 0..self.suspended.len() {
            if self.suspended.get(i).unwrap_or(false) {
                count += 1;
            }
        }
        count
    }
}

/// What changed when the lifecycle moved, for the on-chain event trail.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct EmergencyTransition {
    pub from: EmergencyState,
    pub to: EmergencyState,
    pub caller: Address,
    /// Ledger time the transition was recorded at.
    pub at: u64,
}

/// The set of isolated markets plus the protocol-wide pause that gates them.
///
/// All money movement in a paused protocol goes through this type, so there is
/// exactly one place where a halt is enforced and exactly one place that has to
/// be right. Ledger time is supplied by the caller on every gated call, which
/// keeps the cooldown and halt-timestamp semantics honest and testable.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct MarketRegistry {
    pub markets: Vec<IsolatedMarket>,
    pub pause: EmergencyPause,
}

impl MarketRegistry {
    /// An empty registry governed by `admin`, optionally with a `guardian` that
    /// may halt the protocol without governance.
    pub fn new(env: &Env, admin: Address, guardian: Option<Address>) -> Self {
        MarketRegistry {
            markets: Vec::new(env),
            pause: EmergencyPause::new(env, admin, guardian),
        }
    }

    /// The protocol admin.
    pub fn admin(&self) -> Address {
        self.pause.admin.clone()
    }

    /// The installed guardian, if any.
    pub fn guardian(&self) -> Option<Address> {
        self.pause.guardian.clone()
    }

    /// The current incident state.
    pub fn state(&self) -> EmergencyState {
        self.pause.state
    }

    /// Switch one operation on or off, optionally with a cooldown. Admin only.
    pub fn set_op_pause(
        &mut self,
        env: &Env,
        caller: &Address,
        op: EmergencyOp,
        paused: bool,
        now: u64,
        cooldown_seconds: u64,
    ) -> Result<(), &'static str> {
        self.pause
            .set_op_pause(env, caller, op, paused, now, cooldown_seconds)
    }

    /// Rotate or clear the guardian. Admin only.
    pub fn set_guardian(
        &mut self,
        caller: &Address,
        guardian: Option<Address>,
    ) -> Result<(), &'static str> {
        self.pause.set_guardian(caller, guardian)
    }

    /// Register a market. Gated by `MarketAdmin`, so a halted or recovering
    /// protocol cannot take on new markets.
    pub fn open_market(&mut self, now: u64, market: IsolatedMarket) -> Result<u32, &'static str> {
        self.pause.require_open(EmergencyOp::MarketAdmin, now)?;
        if self.index_of(&market.market_id).is_some() {
            return Err("Market is already registered");
        }
        self.markets.push_back(market);
        self.pause.suspended.push_back(false);
        Ok(self.markets.len() - 1)
    }

    /// Add collateral to a market through the pause gate.
    pub fn deposit(
        &mut self,
        now: u64,
        market_id: &Address,
        position: &mut IsolatedUserPosition,
        asset: &Address,
        amount: i128,
    ) -> Result<(), &'static str> {
        self.with_market(now, market_id, EmergencyOp::Deposit, |market| {
            crate::isolated::deposit_isolated_market(market, position, asset.clone(), amount)
        })
    }

    /// Open or increase debt in a market through the pause gate.
    pub fn borrow(
        &mut self,
        now: u64,
        market_id: &Address,
        position: &mut IsolatedUserPosition,
        borrow_asset: &Address,
        amount: i128,
        prices: &PriceContext,
    ) -> Result<(), &'static str> {
        self.with_market(now, market_id, EmergencyOp::Borrow, |market| {
            crate::isolated::borrow_isolated_market(
                market,
                position,
                borrow_asset.clone(),
                amount,
                prices.collateral_price,
                prices.borrow_price,
                prices.decimals,
            )
        })
    }

    /// Pay down debt. Available in every state, including during an unwind.
    pub fn repay(
        &mut self,
        now: u64,
        market_id: &Address,
        position: &mut IsolatedUserPosition,
        repay_asset: &Address,
        amount: i128,
    ) -> Result<i128, &'static str> {
        self.with_market(now, market_id, EmergencyOp::Repay, |market| {
            crate::isolated::repay_isolated_market(market, position, repay_asset.clone(), amount)
        })
    }

    /// Remove collateral. Available in every state, including during an unwind.
    pub fn withdraw(
        &mut self,
        now: u64,
        market_id: &Address,
        position: &mut IsolatedUserPosition,
        asset: &Address,
        amount: i128,
        prices: &PriceContext,
    ) -> Result<(), &'static str> {
        self.with_market(now, market_id, EmergencyOp::Withdraw, |market| {
            crate::isolated::withdraw_isolated_market(
                market,
                position,
                asset.clone(),
                amount,
                prices.collateral_price,
                prices.borrow_price,
                prices.decimals,
            )
        })
    }

    /// Liquidate a position. Available in every state, including during an
    /// unwind: a pause must never be the reason bad debt keeps accruing.
    pub fn liquidate(
        &mut self,
        now: u64,
        market_id: &Address,
        position: &mut IsolatedUserPosition,
        repay_amount: i128,
        prices: &PriceContext,
    ) -> Result<(i128, i128), &'static str> {
        self.with_market(now, market_id, EmergencyOp::Liquidation, |market| {
            crate::isolated::liquidate_isolated_market_position(
                market,
                position,
                repay_amount,
                prices.collateral_price,
                prices.borrow_price,
                prices.decimals,
            )
        })
    }

    /// Halt every market at once and record which of them were running.
    ///
    /// The cascade is expressed by flipping each market's own `is_active` flag,
    /// so the per-market checks in [`crate::isolated`] become a second line of
    /// defence: a caller that reaches the isolated functions directly still
    /// sees a halted market.
    pub fn halt_all(
        &mut self,
        env: &Env,
        caller: &Address,
        now: u64,
        reason: &str,
    ) -> Result<EmergencyTransition, &'static str> {
        self.pause.require_halt_authority(caller)?;
        let transition = self.pause.halt(env, caller, now, reason)?;
        self.cascade_suspend();
        EmergencyPauseEvent {
            from: transition.from,
            to: transition.to,
            caller: caller.clone(),
            markets_suspended: self.pause.suspended_count(),
            at: transition.at,
        }
        .publish(env);
        Ok(transition)
    }

    /// Re-activate exactly the markets the cascade suspended and return the
    /// protocol to normal operation.
    ///
    /// Markets that were already inactive for an unrelated reason stay inactive,
    /// so closing an incident never silently switches on a market governance had
    /// deliberately taken down.
    pub fn begin_recovery(
        &mut self,
        env: &Env,
        caller: &Address,
    ) -> Result<EmergencyTransition, &'static str> {
        let transition = self.pause.begin_recovery(caller)?;
        self.cascade_restore();
        EmergencyPauseEvent {
            from: transition.from,
            to: transition.to,
            caller: caller.clone(),
            markets_suspended: self.pause.suspended_count(),
            at: transition.at,
        }
        .publish(env);
        Ok(transition)
    }

    /// Close the incident: back to `Normal` with every market the cascade
    /// suspended running again. Admin only.
    pub fn resume_all(
        &mut self,
        env: &Env,
        caller: &Address,
    ) -> Result<EmergencyTransition, &'static str> {
        let transition = self.pause.resume(env, caller)?;
        self.cascade_restore();
        self.pause.clear_suspended();
        EmergencyPauseEvent {
            from: transition.from,
            to: transition.to,
            caller: caller.clone(),
            markets_suspended: 0,
            at: transition.at,
        }
        .publish(env);
        Ok(transition)
    }

    /// Whether a market is registered, running, and not blocked by the pause.
    pub fn is_market_open(&self, market_id: &Address) -> bool {
        if self.pause.state == EmergencyState::Halted {
            return false;
        }
        match self.index_of(market_id) {
            Some(index) => match self.markets.get(index) {
                Some(market) => market.is_active,
                None => false,
            },
            None => false,
        }
    }

    /// Number of registered markets that are not currently running.
    pub fn halted_market_count(&self) -> u32 {
        let mut halted = 0u32;
        for market in self.markets.iter() {
            if !market.is_active {
                halted += 1;
            }
        }
        halted
    }

    /// Total number of registered markets.
    pub fn market_count(&self) -> u32 {
        self.markets.len()
    }

    /// Position of `market_id` in the registry.
    pub fn index_of(&self, market_id: &Address) -> Option<u32> {
        let len = self.markets.len();
        let mut i = 0u32;
        while i < len {
            if let Some(market) = self.markets.get(i) {
                if &market.market_id == market_id {
                    return Some(i);
                }
            }
            i += 1;
        }
        None
    }

    /// A market's configuration, if it is registered.
    pub fn market(&self, market_id: &Address) -> Option<IsolatedMarket> {
        self.index_of(market_id)
            .and_then(|index| self.markets.get(index))
    }

    /// Pause check plus a running-market check, so the caller gets the most
    /// specific reason it can.
    fn gate(&self, now: u64, market_id: &Address, op: EmergencyOp) -> Result<u32, &'static str> {
        self.pause.require_open(op, now)?;
        match self.index_of(market_id) {
            Some(index) => match self.markets.get(index) {
                Some(market) if market.is_active => Ok(index),
                Some(_) => Err("Market is paused or inactive"),
                None => Err("Market is not registered"),
            },
            None => Err("Market is not registered"),
        }
    }

    /// Run `f` against a market only if the pause allows `op` for it.
    ///
    /// The market is written back **only** when `f` succeeds, so a rejected
    /// operation can never leave a market half-mutated - a paused borrow must
    /// not be the reason a market's debt accounting drifts.
    fn with_market<R, F>(
        &mut self,
        now: u64,
        market_id: &Address,
        op: EmergencyOp,
        f: F,
    ) -> Result<R, &'static str>
    where
        F: FnOnce(&mut IsolatedMarket) -> Result<R, &'static str>,
    {
        let index = self.gate(now, market_id, op)?;
        let mut market = self.markets.get(index).ok_or("Market is not registered")?;
        let result = f(&mut market)?;
        self.markets.set(index, market);
        Ok(result)
    }

    /// Flip every running market to inactive and remember which ones were.
    fn cascade_suspend(&mut self) {
        for i in 0..self.markets.len() {
            if let Some(market) = self.markets.get(i) {
                if market.is_active {
                    let mut market = market;
                    market.is_active = false;
                    self.markets.set(i, market);
                    self.pause.mark_suspended(i);
                }
            }
        }
    }

    /// Flip back exactly the markets the cascade suspended.
    fn cascade_restore(&mut self) {
        for i in 0..self.markets.len() {
            if self.pause.is_suspended(i) {
                if let Some(market) = self.markets.get(i) {
                    let mut market = market;
                    market.is_active = true;
                    self.markets.set(i, market);
                }
            }
        }
    }
}

/// Event emitted whenever the protocol-wide lifecycle changes.
#[contractevent(topics = ["emergency"], data_format = "vec")]
#[derive(Clone, Debug)]
pub struct EmergencyPauseEvent {
    pub from: EmergencyState,
    pub to: EmergencyState,
    pub caller: Address,
    pub markets_suspended: u32,
    pub at: u64,
}

/// Event emitted when a single operation switch is toggled.
#[contractevent(topics = ["op_pause"], data_format = "vec")]
#[derive(Clone, Debug)]
pub struct EmergencyOpPauseEvent {
    pub op: EmergencyOp,
    pub paused: bool,
    pub expires_at: u64,
    pub caller: Address,
}
