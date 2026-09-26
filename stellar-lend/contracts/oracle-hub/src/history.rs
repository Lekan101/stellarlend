//! Bounded historical price storage for audit trails.
//!
//! Aggregation answers "what is the price now", and says nothing about what it
//! was five minutes ago. That is fine for pricing and useless for an audit:
//! when a market is liquidated at a bad price, the first questions are which
//! sources were behind that number, whether they agreed, and what else the hub
//! was publishing around the time.
//!
//! This module keeps the last `N` resolved prices per asset so those questions
//! have an on-chain answer.
//!
//! # What is recorded, and what is not
//!
//! Only *fresh resolutions* are recorded. A cache hit returns the memoized
//! aggregate that was already recorded when it was computed, so serving it from
//! the cache neither appends an entry nor moves the head. The history is
//! therefore the sequence of distinct prices the hub published, not one row per
//! consumer that read them — which is what makes it evidence rather than a read
//! log. Two resolutions in the same ledger second that agree are also collapsed
//! into one entry, so a burst of identical reads cannot fill the ring.
//!
//! # Shape of the store
//!
//! A ring per asset, with a constant slot count so that changing the limit can
//! never reinterpret what is already on disk:
//!
//! ```text
//! DefaultHistoryLimit       -> u32               // hub-wide retention, 0 = off
//! HistoryLimit(Bytes)       -> u32               // per-asset override
//! HistoryCount(Bytes)       -> u32               // entries ever appended
//! HistoryEntry(Bytes, u32)  -> PriceHistoryEntry // slot = count % RING_SLOTS
//! ```
//!
//! An append is one `set` plus, once the ring wraps, an overwrite of the oldest
//! slot. Nothing is copied, so the cost of recording a price does not grow with
//! the size of the history. Reads walk backwards from the newest entry and
//! return oldest-first, which is the order an audit reads in.
//!
//! # Governance
//!
//! Recording is **off by default** (`limit == 0`). Every resolution pays for
//! history in storage the protocol has to pay for, so opting in is a governance
//! decision in the same spirit as the price cache. Governance sets a hub-wide
//! default and may override it per asset; `0` disables history for that scope,
//! and lowering a limit prunes the entries that no longer fit.

use crate::storage::DataKey;
use crate::types::{
    AggregationStrategy, PriceHistoryClearedEvent, PriceHistoryConfig,
    PriceHistoryConfigUpdatedEvent, PriceHistoryEntry,
};
use soroban_sdk::{panic_with_error, Bytes, Env, Vec};

use crate::OracleHubError;

/// Fixed number of ring slots an asset can ever occupy.
///
/// This is the ring's modulus, deliberately independent of the configured
/// limit: a limit change then only changes how much of the ring is visible, and
/// can never make an existing entry mean something else.
pub const RING_SLOTS: u32 = 100;

/// Ceiling on a configured retention.
pub const MAX_HISTORY_ENTRIES: u32 = RING_SLOTS;

// ---- governance ------------------------------------------------------------

/// Set the hub-wide retention, or the retention of one asset when `asset` is
/// `Some`. A limit of `0` disables history for that scope and drops what it
/// holds.
pub fn set_limit(env: &Env, asset: Option<&Bytes>, max_entries: u32) {
    if max_entries > MAX_HISTORY_ENTRIES {
        panic_with_error!(env, OracleHubError::InvalidConfig);
    }
    match asset {
        Some(asset) => env
            .storage()
            .instance()
            .set(&DataKey::HistoryLimit(asset.clone()), &max_entries),
        None => env
            .storage()
            .instance()
            .set(&DataKey::DefaultHistoryLimit, &max_entries),
    }
    if let Some(asset) = asset {
        prune(env, asset);
    }
    PriceHistoryConfigUpdatedEvent {
        asset: asset.cloned().unwrap_or_else(|| Bytes::new(env)),
        max_entries,
    }
    .publish(env);
}

/// Hub-wide default, which is also what assets without an override inherit.
pub fn default_limit(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&DataKey::DefaultHistoryLimit)
        .unwrap_or(0)
}

/// Entries retained for an asset: its own override, else the hub-wide default.
pub fn limit(env: &Env, asset: &Bytes) -> u32 {
    env.storage()
        .instance()
        .get(&DataKey::HistoryLimit(asset.clone()))
        .unwrap_or_else(|| default_limit(env))
}

/// Configured retention and how much of it is used, as a view.
pub fn config(env: &Env, asset: &Bytes) -> PriceHistoryConfig {
    PriceHistoryConfig {
        asset: asset.clone(),
        max_entries: limit(env, asset),
        recorded: retained(env, asset),
    }
}

/// Forget an asset's history.
pub fn clear(env: &Env, asset: &Bytes) {
    let mut removed = 0u32;
    for slot in 0..RING_SLOTS {
        let key = DataKey::HistoryEntry(asset.clone(), slot);
        if env.storage().instance().has(&key) {
            env.storage().instance().remove(&key);
            removed += 1;
        }
    }
    env.storage()
        .instance()
        .set(&DataKey::HistoryCount(asset.clone()), &0u32);
    PriceHistoryClearedEvent {
        asset: asset.clone(),
        removed,
    }
    .publish(env);
}

// ---- writing ---------------------------------------------------------------

/// Append one resolved price to an asset's ring.
///
/// A no-op when history is disabled for the asset, or when the newest entry
/// already describes this resolution.
#[allow(clippy::too_many_arguments)]
pub fn record(
    env: &Env,
    asset: &Bytes,
    price: i128,
    confidence: u32,
    num_feeds: u32,
    num_active_feeds: u32,
    strategy: &AggregationStrategy,
    used_fallback: bool,
    deviation_bps: i128,
) {
    let max_entries = limit(env, asset);
    if max_entries == 0 {
        return;
    }
    let count = count(env, asset);
    let now = env.ledger().timestamp();

    // Collapse a repeat of the newest observation: same price, same ledger
    // second, same agreement level is one fact, not two.
    if count > 0 {
        if let Some(newest) = read_slot(env, asset, newest_slot(count)) {
            if newest.timestamp == now
                && newest.price == price
                && newest.num_feeds == num_feeds
                && newest.num_active_feeds == num_active_feeds
            {
                return;
            }
        }
    }

    let entry = PriceHistoryEntry {
        asset: asset.clone(),
        timestamp: now,
        price,
        confidence,
        num_feeds,
        num_active_feeds,
        strategy: *strategy,
        used_fallback,
        deviation_bps,
    };
    env.storage().instance().set(
        &DataKey::HistoryEntry(asset.clone(), count % RING_SLOTS),
        &entry,
    );
    env.storage()
        .instance()
        .set(&DataKey::HistoryCount(asset.clone()), &(count + 1));
}

// ---- reading ---------------------------------------------------------------

/// The retained history of an asset, oldest first.
pub fn entries(env: &Env, asset: &Bytes) -> Vec<PriceHistoryEntry> {
    let max_entries = limit(env, asset);
    let count = count(env, asset);
    let retained = retained(env, asset);
    let mut out: Vec<PriceHistoryEntry> = Vec::new(env);
    if max_entries == 0 {
        return out;
    }
    for i in 0..retained {
        // Walk back from the newest and push to the front, so the caller reads
        // the history in the order it happened.
        if let Some(entry) = read_slot(env, asset, (count - 1 - i) % RING_SLOTS) {
            out.push_front(entry);
        }
    }
    out
}

/// The retained history within a closed timestamp window, oldest first.
pub fn entries_between(env: &Env, asset: &Bytes, from: u64, to: u64) -> Vec<PriceHistoryEntry> {
    let all = entries(env, asset);
    let mut out: Vec<PriceHistoryEntry> = Vec::new(env);
    for entry in all.iter() {
        if entry.timestamp >= from && entry.timestamp <= to {
            out.push_back(entry);
        }
    }
    out
}

/// How many entries are currently retained.
pub fn retained(env: &Env, asset: &Bytes) -> u32 {
    let max_entries = limit(env, asset);
    if max_entries == 0 {
        0
    } else {
        count(env, asset).min(max_entries)
    }
}

// ---- internals -------------------------------------------------------------

fn count(env: &Env, asset: &Bytes) -> u32 {
    env.storage()
        .instance()
        .get(&DataKey::HistoryCount(asset.clone()))
        .unwrap_or(0)
}

fn read_slot(env: &Env, asset: &Bytes, slot: u32) -> Option<PriceHistoryEntry> {
    env.storage()
        .instance()
        .get(&DataKey::HistoryEntry(asset.clone(), slot))
}

/// Slot holding the newest entry.
fn newest_slot(count: u32) -> u32 {
    (count - 1) % RING_SLOTS
}

/// Drop the entries a lowered limit no longer exposes.
///
/// The visible window is the newest `retained` entries; every populated slot
/// after it is unreachable now. Because the ring's modulus is fixed, the
/// window always starts at the same place for a given count, so this stays
/// correct whether or not the ring has wrapped.
fn prune(env: &Env, asset: &Bytes) {
    let count = count(env, asset);
    if count == 0 {
        return;
    }
    let retained = retained(env, asset);
    let window_start = (count - retained) % RING_SLOTS;
    for offset in retained..RING_SLOTS {
        let slot = (window_start + offset) % RING_SLOTS;
        env.storage()
            .instance()
            .remove(&DataKey::HistoryEntry(asset.clone(), slot));
    }
}
