//! Oracle health monitoring.
//!
//! Tracks consecutive failures per asset, records the last success timestamp,
//! and automatically opens a per-asset circuit breaker once a failure
//! threshold is breached. Breakers opened automatically self-heal when a
//! subsequent `get_price` succeeds (see `recover_breaker_if_healthy`);
//! governance can always freeze/unfreeze an asset manually.

use crate::storage::DataKey;
use crate::types::{BreakerState, FeedStatus, FeedStatusCode, OracleHealthStatus};
use soroban_sdk::{Bytes, Env, Vec};

/// Consecutive failures required before the per-asset breaker auto-opens.
pub const AUTO_BREAKER_FAILURE_THRESHOLD: u32 = 3;

/// Cooldown (seconds) applied to an auto-opened breaker.
pub const DEFAULT_BREAKER_COOLDOWN_SECONDS: u64 = 600;

pub fn get_breaker(env: &Env, asset: &Bytes) -> BreakerState {
    env.storage()
        .instance()
        .get::<_, BreakerState>(&DataKey::AssetBreaker(asset.clone()))
        .unwrap_or(BreakerState {
            open_until: 0,
            auto: false,
        })
}

fn set_breaker(env: &Env, asset: &Bytes, state: &BreakerState) {
    env.storage()
        .instance()
        .set(&DataKey::AssetBreaker(asset.clone()), state);
}

/// True when global freeze is active or the per-asset breaker is open.
pub fn is_frozen(env: &Env, asset: &Bytes) -> bool {
    let globally_frozen: bool = env
        .storage()
        .instance()
        .get(&DataKey::Frozen)
        .unwrap_or(false);
    if globally_frozen {
        return true;
    }
    let breaker = get_breaker(env, asset);
    if breaker.open_until == 0 {
        return false;
    }
    env.ledger().timestamp() < breaker.open_until
}

fn get_consecutive_failures(env: &Env, asset: &Bytes) -> u32 {
    env.storage()
        .instance()
        .get::<_, u32>(&DataKey::ConsecutiveFailures(asset.clone()))
        .unwrap_or(0)
}

fn get_last_success(env: &Env, asset: &Bytes) -> u64 {
    env.storage()
        .instance()
        .get::<_, u64>(&DataKey::LastSuccess(asset.clone()))
        .unwrap_or(0)
}

/// Count of enabled feeds with a fresh quote for the asset.
pub fn active_feed_count(env: &Env, asset: &Bytes) -> u32 {
    let now = env.ledger().timestamp();
    let mut active = 0u32;
    for priority in 0u32..=2 {
        let feed_key = DataKey::Feed(asset.clone(), priority);
        if let Some(feed) = env
            .storage()
            .instance()
            .get::<_, crate::types::PriceFeed>(&feed_key)
        {
            if !feed.enabled {
                continue;
            }
            let latest_key = DataKey::LatestPrice(asset.clone(), priority);
            if let Some(point) = env
                .storage()
                .instance()
                .get::<_, crate::types::PricePoint>(&latest_key)
            {
                if now.saturating_sub(point.timestamp) <= feed.stale_threshold_seconds {
                    active += 1;
                }
            }
        }
    }
    active
}

/// Open the automatic per-asset breaker unless it is already open, and report
/// whether this call is what opened it.
///
/// Shared by failure-threshold monitoring and heartbeat expiry so both produce
/// the same state, the same cooldown, and the same event.
pub fn open_auto_breaker(env: &Env, asset: &Bytes, now: &u64) -> bool {
    if is_frozen(env, asset) {
        return false;
    }
    let state = BreakerState {
        open_until: now.saturating_add(DEFAULT_BREAKER_COOLDOWN_SECONDS),
        auto: true,
    };
    set_breaker(env, asset, &state);
    crate::types::BreakerOpenedEvent {
        asset: asset.clone(),
        open_until: state.open_until,
        auto: true,
    }
    .publish(env);
    true
}

/// Record a failed price fetch for an asset and auto-open the breaker once the
/// consecutive failure threshold is reached.
pub fn monitor_oracle_health(env: &Env, asset: &Bytes) -> OracleHealthStatus {
    let failures = get_consecutive_failures(env, asset).saturating_add(1);
    env.storage()
        .instance()
        .set(&DataKey::ConsecutiveFailures(asset.clone()), &failures);

    let last_success = get_last_success(env, asset);
    let now = env.ledger().timestamp();
    let auto_triggered =
        failures >= AUTO_BREAKER_FAILURE_THRESHOLD && open_auto_breaker(env, asset, &now);

    crate::types::HealthFailureEvent {
        asset: asset.clone(),
        consecutive_failures: failures,
    }
    .publish(env);

    OracleHealthStatus {
        asset: asset.clone(),
        consecutive_failures: failures,
        last_success_timestamp: last_success,
        circuit_breaker_open: is_frozen(env, asset),
        auto_triggered,
        active_feeds: active_feed_count(env, asset),
    }
}

/// Record a successful price fetch, resetting the failure counter.
pub fn record_oracle_success(env: &Env, asset: &Bytes) {
    env.storage()
        .instance()
        .set(&DataKey::ConsecutiveFailures(asset.clone()), &0u32);
    env.storage().instance().set(
        &DataKey::LastSuccess(asset.clone()),
        &env.ledger().timestamp(),
    );
    crate::types::HealthSuccessEvent {
        asset: asset.clone(),
    }
    .publish(env);
}

/// Read-only health snapshot without side effects.
pub fn get_health(env: &Env, asset: &Bytes) -> OracleHealthStatus {
    OracleHealthStatus {
        asset: asset.clone(),
        consecutive_failures: get_consecutive_failures(env, asset),
        last_success_timestamp: get_last_success(env, asset),
        circuit_breaker_open: is_frozen(env, asset),
        auto_triggered: get_breaker(env, asset).auto,
        active_feeds: active_feed_count(env, asset),
    }
}

/// If an auto-opened breaker is active but a fresh aggregation succeeded, the
/// asset has stabilized: clear the breaker and reset the failure counters.
pub fn recover_breaker_if_healthy(env: &Env, asset: &Bytes) {
    let state = get_breaker(env, asset);
    if state.open_until != 0 && state.auto {
        set_breaker(
            env,
            asset,
            &BreakerState {
                open_until: 0,
                auto: false,
            },
        );
        record_oracle_success(env, asset);
        crate::types::BreakerUnfrozenEvent {
            asset: asset.clone(),
        }
        .publish(env);
    }
}

/// Governance freeze of a single asset for the default cooldown.
pub fn freeze_asset(env: &Env, asset: &Bytes) {
    let now = env.ledger().timestamp();
    let state = BreakerState {
        open_until: now.saturating_add(DEFAULT_BREAKER_COOLDOWN_SECONDS),
        auto: false,
    };
    set_breaker(env, asset, &state);
    crate::types::BreakerOpenedEvent {
        asset: asset.clone(),
        open_until: state.open_until,
        auto: false,
    }
    .publish(env);
}

/// Governance unfreeze of a single asset.
pub fn unfreeze_asset(env: &Env, asset: &Bytes) {
    set_breaker(
        env,
        asset,
        &BreakerState {
            open_until: 0,
            auto: false,
        },
    );
    crate::types::BreakerUnfrozenEvent {
        asset: asset.clone(),
    }
    .publish(env);
}

/// Per-feed health classification used by `check_feed_health`.
pub fn classify_feed(env: &Env, asset: &Bytes, feed: &crate::types::PriceFeed) -> FeedStatus {
    let now = env.ledger().timestamp();
    let globally_frozen: bool = env
        .storage()
        .instance()
        .get(&DataKey::Frozen)
        .unwrap_or(false);
    let asset_frozen = is_frozen(env, asset);

    let latest_key = DataKey::LatestPrice(asset.clone(), feed.priority as u32);
    let price = env
        .storage()
        .instance()
        .get::<_, crate::types::PricePoint>(&latest_key);

    let (status_code, last_update, is_stale) = if !feed.enabled {
        (FeedStatusCode::Disabled, 0, true)
    } else if globally_frozen || asset_frozen {
        (FeedStatusCode::Frozen, 0, true)
    } else if let Some(p) = price {
        let stale = now.saturating_sub(p.timestamp) > feed.stale_threshold_seconds;
        if stale {
            (FeedStatusCode::Stale, p.timestamp, true)
        } else {
            (FeedStatusCode::Active, p.timestamp, false)
        }
    } else {
        (FeedStatusCode::Stale, 0, true)
    };

    FeedStatus {
        asset: asset.clone(),
        status: status_code,
        last_update,
        is_stale,
    }
}

/// Health classification for every registered feed slot of an asset.
pub fn check_feeds(env: &Env, asset: &Bytes) -> Vec<FeedStatus> {
    let mut statuses: Vec<FeedStatus> = Vec::new(env);
    for priority in 0u32..=2 {
        let feed_key = DataKey::Feed(asset.clone(), priority);
        if let Some(feed) = env
            .storage()
            .instance()
            .get::<_, crate::types::PriceFeed>(&feed_key)
        {
            statuses.push_back(classify_feed(env, asset, &feed));
        }
    }
    statuses
}
