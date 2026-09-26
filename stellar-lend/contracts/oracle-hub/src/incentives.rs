//! Incentive mechanism for price reporters.
//!
//! A push oracle is a volunteer. Governance can register five addresses per
//! asset, but it cannot make any of them stay online, stay honest, or answer a
//! pager at 3 a.m. When a source goes quiet the hub already fails closed (see
//! [`crate::heartbeat`]), which protects the protocol but leaves the operator
//! paying for prices nobody is paid to produce.
//!
//! This module closes that loop with a governance-funded reward pool: an
//! accepted report earns the reporter a fixed reward, and the reporter claims
//! it whenever it likes.
//!
//! # What counts as a report worth paying for
//!
//! Accrual happens on the *accepted* report path — after the price passed
//! validation and was written — so a rejected or unauthorized report earns
//! nothing. Two further rules keep the pool from being drained by a spinning
//! oracle:
//!
//! - **A minimum interval.** A report only earns a reward once
//!   `min_interval_seconds` have passed since that oracle's last rewarded
//!   report for the asset. The hub's own staleness windows are the real budget
//!   for freshness, so paying per second buys nothing.
//! - **A rate ceiling.** `set_reward_per_report` refuses a rate above
//!   [`MAX_REWARD_PER_REPORT`], so a typo cannot drain a funded pool in one
//!   block.
//!
//! # Accounting
//!
//! The hub's token balance is the source of truth for the pool; no balance is
//! mirrored in storage, so a funded pool cannot silently disagree with what the
//! token contract holds. Accrual is a liability, which means a claim can
//! legitimately fail when governance has not funded the pool yet — that reverts
//! with [`crate::OracleHubError::RewardPoolShortfall`] rather than paying out
//! less than the books promise. Reporters therefore get a view of both their
//! earnings and the pool balance, and can claim before the pool runs dry.
//!
//! # Trust
//!
//! - The reporter's own authorization is required to claim *its* rewards, so
//!   nobody can collect on another reporter's behalf.
//! - Only the configured reward token is ever moved. The token is set once;
//!   outstanding rewards are denominated in it, and changing it later would
//!   silently reinterpret them.
//! - Accrual is zeroed before the payout, so a re-entrant token cannot claim the
//!   same rewards twice.
//! - Governance may withdraw from the pool, but never below what reporters have
//!   already earned.

use crate::storage::DataKey;
use crate::types::{
    IncentiveConfig, IncentiveConfigUpdatedEvent, ReporterRewards, RewardTokenSetEvent,
    RewardsClaimedEvent, RewardsFundedEvent, RewardsWithdrawnEvent,
};
use soroban_sdk::{panic_with_error, token, Address, Bytes, Env};

use crate::OracleHubError;

/// Ceiling on a single report's reward, in the token's base units.
pub const MAX_REWARD_PER_REPORT: i128 = 1_000_000_000_000;

/// Default minimum spacing between two rewarded reports by the same oracle for
/// the same asset.
pub const DEFAULT_MIN_REWARD_INTERVAL_SECONDS: u64 = 60;

/// Ceiling on that spacing, so it cannot be set beyond a day.
pub const MAX_MIN_REWARD_INTERVAL_SECONDS: u64 = 86_400;

// ---- configuration ---------------------------------------------------------

/// Whether reporting earns anything at all.
pub fn enabled(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&DataKey::IncentivesEnabled)
        .unwrap_or(false)
}

/// Turn the programme on or off. Turning it off stops accrual; already earned
/// rewards stay claimable.
pub fn set_enabled(env: &Env, is_enabled: bool) {
    env.storage()
        .instance()
        .set(&DataKey::IncentivesEnabled, &is_enabled);
    publish_config(env, None);
}

/// The token rewards are paid in, if governance has chosen one.
pub fn token(env: &Env) -> Option<Address> {
    env.storage().instance().get(&DataKey::RewardToken)
}

/// Choose the reward token. Settable once: outstanding rewards are denominated
/// in it, and swapping it afterwards would reinterpret them.
pub fn set_token(env: &Env, reward_token: &Address) {
    if token(env).is_some() {
        panic_with_error!(env, OracleHubError::InvalidConfig);
    }
    env.storage()
        .instance()
        .set(&DataKey::RewardToken, reward_token);
    RewardTokenSetEvent {
        token: reward_token.clone(),
    }
    .publish(env);
}

/// Hub-wide reward for one accepted report, in token base units.
pub fn default_rate(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::DefaultRewardPerReport)
        .unwrap_or(0)
}

/// Reward for one accepted report of an asset: its own rate, else the default.
pub fn rate(env: &Env, asset: &Bytes) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::RewardPerReport(asset.clone()))
        .unwrap_or_else(|| default_rate(env))
}

/// Set the reward for one asset, or the hub-wide default when `asset` is
/// `None`.
pub fn set_rate(env: &Env, asset: Option<&Bytes>, reward: i128) {
    if !(0..=MAX_REWARD_PER_REPORT).contains(&reward) {
        panic_with_error!(env, OracleHubError::InvalidConfig);
    }
    match asset {
        Some(asset) => env
            .storage()
            .instance()
            .set(&DataKey::RewardPerReport(asset.clone()), &reward),
        None => env
            .storage()
            .instance()
            .set(&DataKey::DefaultRewardPerReport, &reward),
    }
    publish_config(env, asset);
}

/// Minimum spacing between two rewarded reports by one oracle for one asset.
pub fn min_interval(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&DataKey::RewardMinInterval)
        .unwrap_or(DEFAULT_MIN_REWARD_INTERVAL_SECONDS)
}

/// Set that spacing. `0` removes the guard, which governance should only do
/// deliberately.
pub fn set_min_interval(env: &Env, seconds: u64) {
    if seconds > MAX_MIN_REWARD_INTERVAL_SECONDS {
        panic_with_error!(env, OracleHubError::InvalidConfig);
    }
    env.storage()
        .instance()
        .set(&DataKey::RewardMinInterval, &seconds);
    publish_config(env, None);
}

fn publish_config(env: &Env, asset: Option<&Bytes>) {
    IncentiveConfigUpdatedEvent {
        asset: asset.cloned().unwrap_or_else(|| Bytes::new(env)),
        enabled: enabled(env),
        reward_per_report: match asset {
            Some(asset) => rate(env, asset),
            None => default_rate(env),
        },
        min_interval_seconds: min_interval(env),
    }
    .publish(env)
}

/// Configuration in force for an asset, as a view.
pub fn config(env: &Env, asset: Option<&Bytes>) -> IncentiveConfig {
    IncentiveConfig {
        enabled: enabled(env),
        reward_per_report: match asset {
            Some(asset) => rate(env, asset),
            None => default_rate(env),
        },
        min_interval_seconds: min_interval(env),
    }
}

// ---- accrual ---------------------------------------------------------------

/// Credit a reporter for an accepted report.
///
/// A no-op when the programme is off, when no token has been chosen, or when the
/// asset earns nothing.
pub fn accrue(env: &Env, oracle: &Address, asset: &Bytes) {
    if !enabled(env) {
        return;
    }
    if token(env).is_none() {
        return;
    }
    let reward = rate(env, asset);
    if reward <= 0 {
        return;
    }

    let key = DataKey::LastRewarded(oracle.clone(), asset.clone());
    let last_keyed = env.storage().instance().has(&key);
    let last: u64 = env.storage().instance().get(&key).unwrap_or(0);
    let now = env.ledger().timestamp();
    if last_keyed && now.saturating_sub(last) < min_interval(env) {
        return;
    }

    let total = accrued(env, oracle)
        .checked_add(reward)
        .unwrap_or_else(|| panic_with_error!(env, OracleHubError::RewardOverflow));
    env.storage()
        .instance()
        .set(&DataKey::Accrued(oracle.clone()), &total);
    env.storage().instance().set(
        &DataKey::TotalAccrued,
        &total_owed(env).saturating_add(reward),
    );

    // Lifetime earnings per asset, a statistic rather than a balance: it is not
    // decremented on claim, so a reporter's per-asset history survives paying
    // out without the claim having to walk every asset it ever reported.
    let earned = earned_on(env, oracle, asset)
        .checked_add(reward)
        .unwrap_or_else(|| panic_with_error!(env, OracleHubError::RewardOverflow));
    env.storage()
        .instance()
        .set(&DataKey::Earned(oracle.clone(), asset.clone()), &earned);

    env.storage().instance().set(
        &DataKey::RewardedReports(oracle.clone()),
        &(reports_rewarded(env, oracle) + 1),
    );
    env.storage().instance().set(&key, &now);
}

/// Claimable balance of a reporter, in token base units.
pub fn accrued(env: &Env, oracle: &Address) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::Accrued(oracle.clone()))
        .unwrap_or(0)
}

/// Everything reporters have earned and not yet claimed. This is the part of
/// the pool governance may not recover.
pub fn total_owed(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::TotalAccrued)
        .unwrap_or(0)
}

/// Lifetime earnings of a reporter on one asset.
pub fn earned_on(env: &Env, oracle: &Address, asset: &Bytes) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::Earned(oracle.clone(), asset.clone()))
        .unwrap_or(0)
}

/// How many reports of a reporter have earned a reward.
pub fn reports_rewarded(env: &Env, oracle: &Address) -> u32 {
    env.storage()
        .instance()
        .get(&DataKey::RewardedReports(oracle.clone()))
        .unwrap_or(0)
}

// ---- funding and paying ----------------------------------------------------

/// Tokens the hub holds for rewards.
pub fn pool_balance(env: &Env) -> i128 {
    match token(env) {
        Some(token) => {
            token::StellarAssetClient::new(env, &token).balance(&env.current_contract_address())
        }
        None => 0,
    }
}

/// Governance tops the pool up.
pub fn fund(env: &Env, funder: &Address, amount: i128) {
    if amount <= 0 {
        panic_with_error!(env, OracleHubError::InvalidPrice);
    }
    let reward_token = require_token(env);
    token::StellarAssetClient::new(env, &reward_token).transfer(
        funder,
        env.current_contract_address(),
        &amount,
    );
    RewardsFundedEvent {
        funder: funder.clone(),
        amount,
        pool_balance: pool_balance(env),
    }
    .publish(env);
}

/// Pay a reporter everything it has earned.
///
/// The balance is zeroed before the payout, so a re-entrant token cannot claim
/// the same rewards twice, and a claim that the pool cannot cover reverts
/// instead of under-paying.
pub fn claim(env: &Env, oracle: &Address) -> i128 {
    let owed = accrued(env, oracle);
    if owed <= 0 {
        return 0;
    }
    let reward_token = require_token(env);
    if pool_balance(env) < owed {
        panic_with_error!(env, OracleHubError::RewardPoolShortfall);
    }
    env.storage()
        .instance()
        .set(&DataKey::Accrued(oracle.clone()), &0i128);
    env.storage().instance().set(
        &DataKey::TotalAccrued,
        &total_owed(env).saturating_sub(owed),
    );
    token::StellarAssetClient::new(env, &reward_token).transfer(
        &env.current_contract_address(),
        oracle,
        &owed,
    );
    RewardsClaimedEvent {
        oracle: oracle.clone(),
        amount: owed,
        pool_balance: pool_balance(env),
    }
    .publish(env);
    owed
}

/// Governance recovers tokens from the pool, e.g. to wind the programme down.
///
/// Refuses to take the pool below what reporters have already earned, so a
/// reward promised cannot be taken back by the party that promised it.
pub fn withdraw(env: &Env, to: &Address, amount: i128) {
    if amount <= 0 {
        panic_with_error!(env, OracleHubError::InvalidPrice);
    }
    let reward_token = require_token(env);
    let unclaimed = pool_balance(env).saturating_sub(total_owed(env));
    if amount > unclaimed {
        panic_with_error!(env, OracleHubError::RewardPoolShortfall);
    }
    token::StellarAssetClient::new(env, &reward_token).transfer(
        &env.current_contract_address(),
        to,
        &amount,
    );
    RewardsWithdrawnEvent {
        to: to.clone(),
        amount,
        pool_balance: pool_balance(env),
    }
    .publish(env);
}

fn require_token(env: &Env) -> Address {
    token(env).unwrap_or_else(|| panic_with_error!(env, OracleHubError::InvalidConfig))
}

// ---- views -----------------------------------------------------------------

/// A reporter's earnings and the state of the pool funding them.
pub fn rewards_of(env: &Env, oracle: &Address) -> ReporterRewards {
    ReporterRewards {
        oracle: oracle.clone(),
        token: token(env),
        accrued: accrued(env, oracle),
        pool_balance: pool_balance(env),
        total_owed: total_owed(env),
        reports_rewarded: reports_rewarded(env, oracle),
    }
}
