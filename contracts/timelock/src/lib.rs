// SPDX-License-Identifier: MIT
//
// Timelock contract for sensitive admin actions — closes #1389
//
// All sensitive admin operations (oracle updates, risk-param changes, pause
// toggles, fee changes) MUST be scheduled through this contract and can only
// be executed after the configured delay has elapsed.
//
// Roles
//   admin    — schedules, cancels, configures delay, rotates executor.
//   executor — executes queued operations once their ETA has passed.
//
// Configurable parameters
//   min_delay — minimum scheduling lead-time (default 48 h, floor 1 h).
//   max_delay — maximum scheduling lead-time (default 30 days).
//
// Operation lifecycle
//   schedule → (wait ≥ min_delay) → execute
//                                 ↘ cancel  (admin only, before execution)
//
// Sensitive actions that MUST be routed through this contract
//   set_oracle, set_risk_params, set_min_collateral_ratio,
//   set_base_rate, set_kink_utilization, set_multiplier,
//   set_reserve_factor, set_rate_limits, set_pause_switches, set_asset_params.
//
//   emergency_rate_adjustment is intentionally exempt (instant by design).

#![no_std]

use soroban_sdk::{
    contract, contractimpl, contracttype, contracterror, contractevent,
    panic_with_error,
    Address, BytesN, Env, Symbol, Vec,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default minimum scheduling delay: 48 hours in seconds.
pub const DEFAULT_MIN_DELAY_SECS: u64 = 172_800;
/// Hard ceiling on any configured max delay: 30 days.
pub const DEFAULT_MAX_DELAY_SECS: u64 = 2_592_000;
/// Hard floor on min_delay: 1 hour — prevents accidental zero-delay config.
pub const HARD_MIN_DELAY_SECS: u64 = 3_600;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TimelockError {
    AlreadyInitialized = 1,
    NotInitialized     = 2,
    NotAuthorized      = 3,
    OperationNotFound  = 4,
    EtaTooSoon         = 5,
    EtaTooLate         = 6,
    TimelockNotElapsed = 7,
    AlreadyExecuted    = 8,
    AlreadyCancelled   = 9,
    InvalidDelay       = 10,
    ArgsMismatch       = 11,
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Lifecycle state of a queued operation.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum OperationState {
    Pending   = 0,
    Executed  = 1,
    Cancelled = 2,
}

/// A queued admin operation.
#[contracttype]
#[derive(Clone, Debug)]
pub struct QueuedOperation {
    pub id:        u64,
    pub target:    Address,
    pub function:  Symbol,
    /// SHA-256 of the XDR-encoded args supplied at schedule time.
    /// The executor must present matching args; the contract re-hashes them
    /// before dispatching, preventing argument-substitution attacks.
    pub args_hash: BytesN<32>,
    /// Earliest ledger timestamp at which execute() may be called.
    pub eta:       u64,
    pub proposer:  Address,
    pub state:     OperationState,
}

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug)]
enum StorageKey {
    Admin,
    Executor,
    MinDelay,
    MaxDelay,
    NextId,
    Operation(u64),
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[contractevent]
#[derive(Clone, Debug)]
pub struct OperationScheduledEvent {
    pub id:        u64,
    pub proposer:  Address,
    pub target:    Address,
    pub function:  Symbol,
    pub args_hash: BytesN<32>,
    pub eta:       u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct OperationExecutedEvent {
    pub id:       u64,
    pub executor: Address,
    pub target:   Address,
    pub function: Symbol,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct OperationCancelledEvent {
    pub id:    u64,
    pub admin: Address,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct DelayUpdatedEvent {
    pub admin:     Address,
    pub min_delay: u64,
    pub max_delay: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct ExecutorRotatedEvent {
    pub admin:        Address,
    pub new_executor: Address,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct TimelockContract;

#[contractimpl]
impl TimelockContract {
    // -----------------------------------------------------------------------
    // Initialisation
    // -----------------------------------------------------------------------

    /// Initialise the timelock. Can only be called once.
    ///
    /// * `admin`     — may schedule / cancel / configure.
    /// * `executor`  — may execute queued operations.
    /// * `min_delay` — minimum scheduling lead-time in seconds (≥ 1 h).
    pub fn initialize(env: Env, admin: Address, executor: Address, min_delay: u64) {
        admin.require_auth();
        if env.storage().instance().has(&StorageKey::Admin) {
            panic_with_error!(&env, TimelockError::AlreadyInitialized);
        }
        if min_delay < HARD_MIN_DELAY_SECS || min_delay > DEFAULT_MAX_DELAY_SECS {
            panic_with_error!(&env, TimelockError::InvalidDelay);
        }
        env.storage().instance().set(&StorageKey::Admin,    &admin);
        env.storage().instance().set(&StorageKey::Executor, &executor);
        env.storage().instance().set(&StorageKey::MinDelay, &min_delay);
        env.storage().instance().set(&StorageKey::MaxDelay, &DEFAULT_MAX_DELAY_SECS);
        env.storage().instance().set(&StorageKey::NextId,   &1u64);
    }

    // -----------------------------------------------------------------------
    // Core operations
    // -----------------------------------------------------------------------

    /// Queue a sensitive admin operation.
    ///
    /// `eta` must satisfy: `now + min_delay ≤ eta ≤ now + max_delay`.
    /// `args_hash` = SHA-256(xdr(args)); the executor must present the
    /// original args to pass the integrity check before dispatch.
    ///
    /// Returns the numeric id assigned to the new operation.
    pub fn schedule(
        env:       Env,
        caller:    Address,
        target:    Address,
        function:  Symbol,
        args_hash: BytesN<32>,
        eta:       u64,
    ) -> u64 {
        caller.require_auth();
        Self::assert_initialized(&env);
        Self::assert_admin(&env, &caller);

        let now       = env.ledger().timestamp();
        let min_delay = Self::load_min_delay(&env);
        let max_delay = Self::load_max_delay(&env);

        if eta < now.saturating_add(min_delay) {
            panic_with_error!(&env, TimelockError::EtaTooSoon);
        }
        if eta > now.saturating_add(max_delay) {
            panic_with_error!(&env, TimelockError::EtaTooLate);
        }

        let id = Self::next_id(&env);
        env.storage().instance().set(&StorageKey::NextId, &(id + 1));

        let op = QueuedOperation {
            id,
            target:    target.clone(),
            function:  function.clone(),
            args_hash: args_hash.clone(),
            eta,
            proposer:  caller.clone(),
            state:     OperationState::Pending,
        };
        env.storage().persistent().set(&StorageKey::Operation(id), &op);

        OperationScheduledEvent { id, proposer: caller, target, function, args_hash, eta }
            .publish(&env);
        id
    }

    /// Execute a queued operation whose ETA has passed.
    ///
    /// The contract recomputes SHA-256(xdr(args)) and rejects any mismatch
    /// with the stored hash before dispatching the cross-contract call.
    pub fn execute(env: Env, caller: Address, id: u64, args: Vec<soroban_sdk::Val>) {
        caller.require_auth();
        Self::assert_initialized(&env);
        Self::assert_executor(&env, &caller);

        let mut op: QueuedOperation = env
            .storage()
            .persistent()
            .get(&StorageKey::Operation(id))
            .unwrap_or_else(|| panic_with_error!(&env, TimelockError::OperationNotFound));

        match op.state {
            OperationState::Executed  => panic_with_error!(&env, TimelockError::AlreadyExecuted),
            OperationState::Cancelled => panic_with_error!(&env, TimelockError::AlreadyCancelled),
            OperationState::Pending   => {}
        }

        if env.ledger().timestamp() < op.eta {
            panic_with_error!(&env, TimelockError::TimelockNotElapsed);
        }

        // Argument integrity check.
        let computed: BytesN<32> = env.crypto().sha256(&env.to_xdr(&args)).into();
        if computed != op.args_hash {
            panic_with_error!(&env, TimelockError::ArgsMismatch);
        }

        op.state = OperationState::Executed;
        env.storage().persistent().set(&StorageKey::Operation(id), &op);

        env.invoke_contract::<()>(&op.target, &op.function, args);

        OperationExecutedEvent {
            id,
            executor: caller,
            target:   op.target,
            function: op.function,
        }
        .publish(&env);
    }

    /// Cancel a pending operation. Admin only.
    pub fn cancel(env: Env, caller: Address, id: u64) {
        caller.require_auth();
        Self::assert_initialized(&env);
        Self::assert_admin(&env, &caller);

        let mut op: QueuedOperation = env
            .storage()
            .persistent()
            .get(&StorageKey::Operation(id))
            .unwrap_or_else(|| panic_with_error!(&env, TimelockError::OperationNotFound));

        match op.state {
            OperationState::Executed  => panic_with_error!(&env, TimelockError::AlreadyExecuted),
            OperationState::Cancelled => panic_with_error!(&env, TimelockError::AlreadyCancelled),
            OperationState::Pending   => {}
        }

        op.state = OperationState::Cancelled;
        env.storage().persistent().set(&StorageKey::Operation(id), &op);

        OperationCancelledEvent { id, admin: caller }.publish(&env);
    }

    // -----------------------------------------------------------------------
    // Configuration
    // -----------------------------------------------------------------------

    /// Update the min/max delay window. Admin only.
    ///
    /// Invariants: new_min ≥ 1 h, new_max ≤ 30 days, new_min ≤ new_max.
    pub fn set_delay(env: Env, caller: Address, new_min: u64, new_max: u64) {
        caller.require_auth();
        Self::assert_initialized(&env);
        Self::assert_admin(&env, &caller);

        if new_min < HARD_MIN_DELAY_SECS
            || new_max > DEFAULT_MAX_DELAY_SECS
            || new_min > new_max
        {
            panic_with_error!(&env, TimelockError::InvalidDelay);
        }
        env.storage().instance().set(&StorageKey::MinDelay, &new_min);
        env.storage().instance().set(&StorageKey::MaxDelay, &new_max);

        DelayUpdatedEvent { admin: caller, min_delay: new_min, max_delay: new_max }
            .publish(&env);
    }

    /// Rotate the executor role. Admin only.
    pub fn set_executor(env: Env, caller: Address, new_executor: Address) {
        caller.require_auth();
        Self::assert_initialized(&env);
        Self::assert_admin(&env, &caller);

        env.storage().instance().set(&StorageKey::Executor, &new_executor);
        ExecutorRotatedEvent { admin: caller, new_executor }.publish(&env);
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    pub fn get_operation(env: Env, id: u64) -> QueuedOperation {
        Self::assert_initialized(&env);
        env.storage()
            .persistent()
            .get(&StorageKey::Operation(id))
            .unwrap_or_else(|| panic_with_error!(&env, TimelockError::OperationNotFound))
    }

    pub fn min_delay(env: Env) -> u64 { Self::load_min_delay(&env) }

    pub fn max_delay(env: Env) -> u64 { Self::load_max_delay(&env) }

    pub fn is_initialized(env: Env) -> bool {
        env.storage().instance().has(&StorageKey::Admin)
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn assert_initialized(env: &Env) {
        if !env.storage().instance().has(&StorageKey::Admin) {
            panic_with_error!(env, TimelockError::NotInitialized);
        }
    }

    fn assert_admin(env: &Env, caller: &Address) {
        let admin: Address = env.storage().instance()
            .get(&StorageKey::Admin)
            .unwrap_or_else(|| panic_with_error!(env, TimelockError::NotInitialized));
        if *caller != admin { panic_with_error!(env, TimelockError::NotAuthorized); }
    }

    fn assert_executor(env: &Env, caller: &Address) {
        let executor: Address = env.storage().instance()
            .get(&StorageKey::Executor)
            .unwrap_or_else(|| panic_with_error!(env, TimelockError::NotInitialized));
        if *caller != executor { panic_with_error!(env, TimelockError::NotAuthorized); }
    }

    fn load_min_delay(env: &Env) -> u64 {
        env.storage().instance().get(&StorageKey::MinDelay).unwrap_or(DEFAULT_MIN_DELAY_SECS)
    }

    fn load_max_delay(env: &Env) -> u64 {
        env.storage().instance().get(&StorageKey::MaxDelay).unwrap_or(DEFAULT_MAX_DELAY_SECS)
    }

    fn next_id(env: &Env) -> u64 {
        env.storage().instance().get(&StorageKey::NextId).unwrap_or(1u64)
    }
}
