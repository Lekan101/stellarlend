//! Oracle heartbeat monitoring and staleness detection.
//!
//! A price is only as good as the newest report behind it. The hub already
//! drops an individual feed once its last point goes past
//! [`crate::types::PriceFeed::stale_threshold_seconds`], but that check is
//! per source and only fires while somebody is *reading* a price: a push oracle
//! that dies quietly leaves no trace until a consumer happens to look.
//!
//! A heartbeat closes that gap. Every accepted report (or successful live pull)
//! is a beat, and a beat that stops arriving is classified on its own schedule:
//!
//! ```text
//! report --> Fresh --(interval)--> Due --(stale_after)--> Stale --(expiry)--> Expired
//!   \-- a report at any point returns the slot to Fresh --/
//! ```
//!
//! Two properties matter more than the labels:
//!
//! 1. **Silence is a failure, not a value.** Once a push slot passes its expiry
//!    the hub refuses to price the asset, because serving a number nobody is
//!    updating is worse than serving no number at all.
//! 2. **Pull feeds are exempt.** A pull source has no reporting cadence — the
//!    hub queries it on demand — so only its last successful pull is recorded
//!    and its silence can never expire an asset that is answering every call.
//!
//! Sweeping is permissionless and idempotent: a keeper may call it as often as
//! it likes, and it publishes only the transitions it has not published before.
//!
//! # Recovery
//!
//! Expiry trips the hub's existing automatic circuit breaker rather than
//! inventing a second kind of halt, so an expired asset recovers exactly the
//! way any other auto-opened breaker does: the source reports again, and the
//! asset serves prices again once the breaker's cooldown
//! ([`crate::health::DEFAULT_BREAKER_COOLDOWN_SECONDS`]) has elapsed. Note the
//! consequence for configuration: a monitored feed must report *within* the
//! breaker cooldown, so an `expiry_seconds` at or below it leaves no window in
//! which a recovered feed can be observed. `stale_after_seconds` is the right
//! place to express urgency; `expiry_seconds` should exceed the cooldown.

use crate::storage::DataKey;
use crate::types::{
    HeartbeatConfig, HeartbeatConfigUpdatedEvent, HeartbeatExpiredEvent, HeartbeatRecoveredEvent,
    HeartbeatSlotState, HeartbeatSlotStatus, HeartbeatStaleEvent, HeartbeatStatus,
};
use soroban_sdk::{panic_with_error, Bytes, Env, Vec};

use crate::OracleHubError;

/// Expected reporting cadence of a push source.
pub const DEFAULT_INTERVAL_SECONDS: u64 = 300;
/// Silence past this age marks a slot stale.
pub const DEFAULT_STALE_AFTER_SECONDS: u64 = 900;
/// Silence past this age expires a slot and fails the asset closed.
pub const DEFAULT_EXPIRY_SECONDS: u64 = 3_600;
/// Upper bound on a reporting cadence, so a typo cannot disable monitoring
/// for years.
pub const MAX_INTERVAL_SECONDS: u64 = 86_400;

/// Hub-wide expectations for monitored assets.
pub fn default_config(env: &Env) -> HeartbeatConfig {
    env.storage()
        .instance()
        .get(&DataKey::DefaultHeartbeatConfig)
        .unwrap_or(HeartbeatConfig {
            enabled: false,
            interval_seconds: DEFAULT_INTERVAL_SECONDS,
            stale_after_seconds: DEFAULT_STALE_AFTER_SECONDS,
            expiry_seconds: DEFAULT_EXPIRY_SECONDS,
        })
}

/// Effective expectations for an asset: its own override, else the default.
pub fn config(env: &Env, asset: &Bytes) -> HeartbeatConfig {
    env.storage()
        .instance()
        .get::<_, HeartbeatConfig>(&DataKey::HeartbeatConfig(asset.clone()))
        .unwrap_or_else(|| default_config(env))
}

/// Store expectations for one asset, or for every asset when `asset` is `None`.
///
/// Enabling monitoring re-arms the clock: a slot that has never reported is
/// given until its first expiry window to prove itself, rather than being
/// expired retroactively for silence that predates the monitoring.
pub fn set_config(env: &Env, asset: Option<&Bytes>, config: &HeartbeatConfig) {
    validate(env, config);
    match asset {
        Some(asset) => env
            .storage()
            .instance()
            .set(&DataKey::HeartbeatConfig(asset.clone()), config),
        None => env
            .storage()
            .instance()
            .set(&DataKey::DefaultHeartbeatConfig, config),
    }
    if config.enabled {
        let now = env.ledger().timestamp();
        match asset {
            Some(asset) => env
                .storage()
                .instance()
                .set(&DataKey::HeartbeatArmedAt(asset.clone()), &now),
            None => env
                .storage()
                .instance()
                .set(&DataKey::DefaultHeartbeatArmedAt, &now),
        };
    }
    HeartbeatConfigUpdatedEvent {
        asset: asset.cloned().unwrap_or_else(|| Bytes::new(env)),
        enabled: config.enabled,
        interval_seconds: config.interval_seconds,
        stale_after_seconds: config.stale_after_seconds,
        expiry_seconds: config.expiry_seconds,
    }
    .publish(env);
}

/// Reject expectations that could not describe a working oracle.
fn validate(env: &Env, config: &HeartbeatConfig) {
    if !config.enabled {
        return;
    }
    let coherent = config.interval_seconds > 0
        && config.interval_seconds <= MAX_INTERVAL_SECONDS
        && config.stale_after_seconds >= config.interval_seconds
        && config.expiry_seconds > config.stale_after_seconds;
    if !coherent {
        panic_with_error!(env, OracleHubError::InvalidConfig);
    }
}

/// When monitoring was armed for an asset, else for the hub default.
///
/// A slot that has never reported has no `last_seen` of its own, so its silence
/// is measured from this moment.
fn armed_at(env: &Env, asset: &Bytes) -> u64 {
    env.storage()
        .instance()
        .get::<_, u64>(&DataKey::HeartbeatArmedAt(asset.clone()))
        .unwrap_or_else(|| {
            env.storage()
                .instance()
                .get::<_, u64>(&DataKey::DefaultHeartbeatArmedAt)
                .unwrap_or(0)
        })
}

/// Persisted bookkeeping for a slot, or `None` when it has never been heard
/// from. Absence is the whole point: a timestamp of `0` is a legitimate report
/// time in tests and in the first ledger of a deployment.
fn slot_state(env: &Env, asset: &Bytes, priority: u32) -> Option<HeartbeatSlotState> {
    env.storage()
        .instance()
        .get(&DataKey::Heartbeat(asset.clone(), priority))
}

fn write_slot_state(env: &Env, asset: &Bytes, priority: u32, state: &HeartbeatSlotState) {
    env.storage()
        .instance()
        .set(&DataKey::Heartbeat(asset.clone(), priority), state);
}

/// A slot's liveness inputs: the last beat actually recorded, and the instant
/// the current silence is measured from.
struct Liveness {
    /// Last recorded beat; `0` when the slot has never reported.
    last_seen: u64,
    /// Start of the current silence window.
    silence_from: u64,
}

impl Liveness {
    /// A slot that has never reported is not expired retroactively: its silence
    /// starts when monitoring was armed, so a newly monitored feed gets a full
    /// expiry window to say something.
    fn resolve(env: &Env, asset: &Bytes, state: &Option<HeartbeatSlotState>) -> Self {
        match state {
            Some(state) => Liveness {
                last_seen: state.last_seen,
                silence_from: state.last_seen,
            },
            None => Liveness {
                last_seen: 0,
                silence_from: armed_at(env, asset),
            },
        }
    }
}

/// Classify a slot from how long it has been silent.
///
/// Degradation is measured first and the `Silent` label second: a slot that has
/// never reported is distinguished from a late one while there is still time to
/// act, and stops being merely "silent" once its silence is itself a failure.
fn classify(
    now: u64,
    state: &Option<HeartbeatSlotState>,
    live: &Liveness,
    config: &HeartbeatConfig,
) -> HeartbeatStatus {
    let age = now.saturating_sub(live.silence_from);
    if age >= config.expiry_seconds {
        HeartbeatStatus::Expired
    } else if age >= config.stale_after_seconds {
        HeartbeatStatus::Stale
    } else if state.is_none() {
        HeartbeatStatus::Silent
    } else if age >= config.interval_seconds {
        HeartbeatStatus::Due
    } else {
        HeartbeatStatus::Fresh
    }
}

/// Whether a feed slot is exempt from heartbeat enforcement.
///
/// Pull sources are queried on demand, so their silence is expected; only push
/// sources have a cadence to keep.
fn is_exempt(env: &Env, asset: &Bytes, priority: u32) -> bool {
    crate::feeds::feed_at(env, asset, priority)
        .map(|feed| feed.mode == crate::types::FeedMode::Pull)
        .unwrap_or(true)
}

/// Liveness of one slot, without side effects.
pub fn slot_status(env: &Env, asset: &Bytes, priority: u32) -> HeartbeatSlotStatus {
    let config = config(env, asset);
    let now = env.ledger().timestamp();
    let state = slot_state(env, asset, priority);
    let live = Liveness::resolve(env, asset, &state);
    // Unmonitored slots are never degraded: there is no cadence to have missed,
    // and a pull source's silence is expected rather than suspicious.
    let monitored = config.enabled && !is_exempt(env, asset, priority);
    let status = if monitored {
        classify(now, &state, &live, &config)
    } else {
        HeartbeatStatus::Fresh
    };

    HeartbeatSlotStatus {
        asset: asset.clone(),
        priority,
        status,
        last_seen: live.last_seen,
        age_seconds: now.saturating_sub(live.silence_from),
        missed_beats: if monitored {
            missed_beats(now, live.silence_from, &config)
        } else {
            0
        },
    }
}

/// Liveness of every registered slot of an asset.
pub fn statuses(env: &Env, asset: &Bytes) -> Vec<HeartbeatSlotStatus> {
    let mut out: Vec<HeartbeatSlotStatus> = Vec::new(env);
    for priority in crate::feeds::feed_index(env, asset).iter() {
        out.push_back(slot_status(env, asset, priority));
    }
    out
}

/// Whole intervals elapsed since the last beat.
fn missed_beats(now: u64, last_seen: u64, config: &HeartbeatConfig) -> u32 {
    let interval = config.interval_seconds.max(1);
    let missed = now.saturating_sub(last_seen) / interval;
    if missed > u32::MAX as u64 {
        u32::MAX
    } else {
        missed as u32
    }
}

/// Accept a beat from a push report or a live pull.
///
/// Only the transition back to health is published: a working oracle that
/// reports every minute would otherwise flood the log with "still fine".
pub fn record_beat(env: &Env, asset: &Bytes, priority: u32) {
    let config = config(env, asset);
    if !config.enabled {
        return;
    }
    let now = env.ledger().timestamp();
    let previous = slot_state(env, asset, priority);
    let was_degraded = previous
        .as_ref()
        .map(|state| {
            matches!(
                state.last_status,
                HeartbeatStatus::Stale | HeartbeatStatus::Expired | HeartbeatStatus::Silent
            )
        })
        .unwrap_or(true);
    if was_degraded {
        let silence = now.saturating_sub(Liveness::resolve(env, asset, &previous).silence_from);
        HeartbeatRecoveredEvent {
            asset: asset.clone(),
            priority,
            status: HeartbeatStatus::Fresh,
            silence_seconds: silence,
        }
        .publish(env);
    }
    write_slot_state(
        env,
        asset,
        priority,
        &HeartbeatSlotState {
            last_seen: now,
            missed_beats: 0,
            last_status: HeartbeatStatus::Fresh,
        },
    );
}

/// Whether any monitored push slot of the asset has expired.
///
/// Read by the price path so an expired asset fails closed even between
/// keeper sweeps.
pub fn is_expired(env: &Env, asset: &Bytes) -> bool {
    let config = config(env, asset);
    if !config.enabled {
        return false;
    }
    let now = env.ledger().timestamp();
    for priority in crate::feeds::feed_index(env, asset).iter() {
        if is_exempt(env, asset, priority) {
            continue;
        }
        let state = slot_state(env, asset, priority);
        let live = Liveness::resolve(env, asset, &state);
        if classify(now, &state, &live, &config) == HeartbeatStatus::Expired {
            return true;
        }
    }
    false
}

/// Advance every slot of an asset to its current status and act on it.
///
/// Permissionless and idempotent: the same call at the same ledger time
/// produces the same state and no new events. Called by a keeper, and by any
/// consumer that wants the log to reflect reality before reading a price.
pub fn sweep(env: &Env, asset: &Bytes) -> Vec<HeartbeatSlotStatus> {
    let config = config(env, asset);
    let now = env.ledger().timestamp();
    let mut out: Vec<HeartbeatSlotStatus> = Vec::new(env);

    for priority in crate::feeds::feed_index(env, asset).iter() {
        if !config.enabled || is_exempt(env, asset, priority) {
            out.push_back(slot_status(env, asset, priority));
            continue;
        }

        let previous = slot_state(env, asset, priority);
        let live = Liveness::resolve(env, asset, &previous);
        let status = classify(now, &previous, &live, &config);
        let missed = missed_beats(now, live.silence_from, &config);
        let changed = previous
            .as_ref()
            .map(|s| s.last_status != status)
            .unwrap_or(true);

        if changed {
            let age = now.saturating_sub(live.silence_from);
            match status.clone() {
                HeartbeatStatus::Stale => HeartbeatStaleEvent {
                    asset: asset.clone(),
                    priority,
                    age_seconds: age,
                    missed_beats: missed,
                }
                .publish(env),
                HeartbeatStatus::Expired => HeartbeatExpiredEvent {
                    asset: asset.clone(),
                    priority,
                    age_seconds: age,
                    missed_beats: missed,
                }
                .publish(env),
                _ => {}
            }
            write_slot_state(
                env,
                asset,
                priority,
                &HeartbeatSlotState {
                    last_seen: live.last_seen,
                    missed_beats: missed,
                    last_status: status.clone(),
                },
            );
        }

        // Only expiry acts. A stale slot is still a working slot that is late,
        // and the existing staleness path already refuses to price the asset
        // from it; expiring here is what fails the asset closed, and doing it
        // during a sweep means a dead oracle is reported even when nobody is
        // asking for a price.
        if status == HeartbeatStatus::Expired {
            crate::health::open_auto_breaker(env, asset, &now);
        }

        out.push_back(HeartbeatSlotStatus {
            asset: asset.clone(),
            priority,
            status,
            last_seen: live.last_seen,
            age_seconds: now.saturating_sub(live.silence_from),
            missed_beats: missed,
        });
    }
    out
}

/// Expiry deadline of a slot, for consumers that want to schedule their own
/// check instead of relying on a keeper.
pub fn expires_at(env: &Env, asset: &Bytes, priority: u32) -> u64 {
    let config = config(env, asset);
    if !config.enabled || is_exempt(env, asset, priority) {
        return 0;
    }
    let live = Liveness::resolve(env, asset, &slot_state(env, asset, priority));
    live.silence_from.saturating_add(config.expiry_seconds)
}
