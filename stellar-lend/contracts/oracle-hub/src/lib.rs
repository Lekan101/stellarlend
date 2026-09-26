//! # Oracle Hub
//!
//! A dedicated, governance-managed contract for price feed management.
//!
//! The hub decouples price aggregation from lending logic:
//! - **Pluggable providers**: feeds can be push-based (providers call
//!   `report_price`) or pull-based (external contracts implementing the
//!   [`interface::PriceProvider`] interface are queried by the hub).
//! - **Multi-source aggregation**: up to [`types::MAX_FEEDS_PER_ASSET`]
//!   sources per asset, rescaled to one canonical precision, combined by
//!   median, confidence-weighted mean, or trimmed mean.
//! - **Deviation-checked fallback**: the leading source is demoted when it
//!   strays from the rest, and the demotion is published on-chain.
//! - **Health monitoring**: per-feed staleness classification, consecutive
//!   failure tracking, and an auto-opening per-asset circuit breaker.
//! - **Cached reads**: an opt-in, epoch-invalidated price cache that removes
//!   repeated aggregation and repeated provider calls from the hot path.
//! - **Upgrade mechanism**: governance stages a WASM hash and atomically swaps
//!   the contract code via `env.deployer().update_current_contract_wasm`.
//! - **Emergency controls**: global freeze plus per-asset governance freeze.
//!
//! See the `docs/` directory for the full architecture and integration guide.

#![no_std]

mod aggregation;
mod cache;
mod fallback;
mod feeds;
mod health;
mod heartbeat;
mod history;
mod incentives;
mod interface;
mod provider;
mod storage;
mod types;
mod upgrade;

#[cfg(test)]
mod tests;

use crate::types::{
    AggregatedPrice, AggregationParams, AggregationStrategy, CacheConfigUpdatedEvent, CacheStats,
    FeedMode, FeedPriority, FeedQuote, HeartbeatConfig, IncentiveConfig, OracleHealthStatus,
    PriceFeed, PriceHistoryConfig, PricePoint, PriceSource, ProviderPrice, ReporterRewards,
    VERSION,
};
use soroban_sdk::{
    contract, contracterror, contractimpl, panic_with_error, Address, Bytes, BytesN, Env, Vec,
};

/// Errors surfaced by the Oracle Hub.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum OracleHubError {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    Frozen = 3,
    FeedNotFound = 4,
    FeedDisabled = 5,
    InvalidPrice = 6,
    NoActiveFeeds = 7,
    FetchFailed = 8,
    InvalidConfig = 9,
    /// The requested feed slot already holds a feed.
    FeedSlotOccupied = 10,
    /// Too few sources agreed to publish a price.
    InsufficientSources = 11,
    /// A monitored feed slot stopped reporting past its expiry, so the hub
    /// refuses to serve a price nobody is updating.
    HeartbeatExpired = 12,
    /// The reward pool cannot cover a claim or withdrawal reporters have
    /// already earned against.
    RewardPoolShortfall = 13,
    /// A reporter's earnings would exceed the largest representable balance.
    RewardOverflow = 14,
}

/// The Oracle Hub contract.
#[contract]
pub struct OracleHubContract;

#[contractimpl]
impl OracleHubContract {
    // ── Initialization ─────────────────────────────────────────────────────

    pub fn initialize(env: Env, governance: Address, admin: Address) {
        if env
            .storage()
            .instance()
            .get::<_, Address>(&storage::DataKey::Governance)
            .is_some()
        {
            panic_with_error!(&env, OracleHubError::AlreadyInitialized);
        }

        // Governance controls feeds, freezes, and upgrades. Both authorities
        // must authorize the one-time configuration before it is persisted.
        governance.require_auth();
        admin.require_auth();

        env.storage()
            .instance()
            .set(&storage::DataKey::Governance, &governance);
        env.storage()
            .instance()
            .set(&storage::DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&storage::DataKey::Version, &VERSION);
        env.storage()
            .instance()
            .set(&storage::DataKey::Frozen, &false);
        env.storage()
            .instance()
            .set(&storage::DataKey::FeedCount, &0u32);
        env.storage().instance().set(
            &storage::DataKey::DefaultStrategy,
            &AggregationStrategy::Median,
        );
        env.storage()
            .instance()
            .set(&storage::DataKey::PriceDecimals, &types::CANONICAL_DECIMALS);
        env.storage()
            .instance()
            .set(&storage::DataKey::CacheTtlSeconds, &0u64);
        env.storage()
            .instance()
            .set(&storage::DataKey::CacheEpoch, &0u32);
    }

    pub fn version(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&storage::DataKey::Version)
            .unwrap_or(0)
    }

    // ── Upgrade mechanism ──────────────────────────────────────────────────

    /// Governance stages the next contract code.
    pub fn stage_upgrade(env: Env, new_wasm: BytesN<32>) {
        let governance = require_governance(&env);
        governance.require_auth();
        require_not_frozen(&env);
        upgrade::stage_upgrade(&env, new_wasm, &governance);
    }

    /// Governance applies the staged upgrade, atomically swapping contract code.
    ///
    /// Requires the installed multi-signature threshold to be met and, when the
    /// threshold is above 1, the 48-hour upgrade timelock to have elapsed.
    pub fn upgrade(env: Env) -> BytesN<32> {
        let governance = require_governance(&env);
        governance.require_auth();
        require_not_frozen(&env);
        upgrade::apply_upgrade(&env, &governance)
    }

    /// Installs the multi-signature approver set and threshold for upgrades.
    ///
    /// Admin-only. Until this is called, upgrades require only governance
    /// (threshold 1). Set a threshold > 1 and distribute keys to grant-free
    /// members to harden the protocol against single-key takeovers.
    pub fn init_upgrade_multisig(
        env: Env,
        caller: Address,
        approvers: Vec<Address>,
        threshold: u32,
    ) -> Result<(), upgrade::UpgradeError> {
        caller.require_auth();
        require_not_frozen(&env);
        upgrade::init_upgrade_multisig(&env, &caller, approvers, threshold)
    }

    /// A grant-free approver signs the pending upgrade.
    ///
    /// Once the threshold is met, the 48-hour timelock starts. Approval does
    /// not itself swap code — `upgrade()` does, after the timelock.
    pub fn approve_upgrade(env: Env, approver: Address) -> Result<u32, upgrade::UpgradeError> {
        approver.require_auth();
        require_not_frozen(&env);
        let count = upgrade::approve_upgrade(&env, &approver)?;
        let timelock_until: u64 = env
            .storage()
            .instance()
            .get(&storage::DataKey::UpgradeTimelockUntil)
            .unwrap_or(0);
        crate::types::UpgradeApprovedEvent {
            approver,
            approval_count: count,
            timelock_until,
        }
        .publish(&env);
        Ok(count)
    }

    /// Staged upgrade WASM hash, if any.
    pub fn pending_wasm_hash(env: Env) -> Option<BytesN<32>> {
        upgrade::pending_wasm(&env)
    }

    /// Approvals collected on the pending upgrade.
    pub fn upgrade_approval_count(env: Env) -> u32 {
        upgrade::approval_count(&env)
    }

    /// Installed upgrade threshold (1 = single governance key).
    pub fn upgrade_threshold(env: Env) -> u32 {
        upgrade::upgrade_threshold(&env)
    }

    /// Whether the pending upgrade can be executed (threshold met + timelock).
    pub fn upgrade_ready(env: Env) -> bool {
        upgrade::can_execute(&env)
    }

    // ── Feed management ────────────────────────────────────────────────────

    /// Governance registers a feed slot for an asset.
    ///
    /// A slot holds exactly one feed: re-registering an occupied slot is
    /// rejected instead of silently replacing the incumbent oracle.
    pub fn register_feed(
        env: Env,
        asset: Bytes,
        oracle_address: Address,
        priority: FeedPriority,
        stale_threshold_seconds: u64,
        mode: FeedMode,
        weight_bps: u32,
    ) {
        let governance = require_governance(&env);
        governance.require_auth();
        require_not_frozen(&env);

        assert!(!asset.is_empty(), "Asset must not be empty");
        assert!(
            oracle_address != env.current_contract_address(),
            "Oracle must not be the hub contract itself"
        );
        if feeds::is_registered(&env, &asset, priority as u32) {
            panic_with_error!(&env, OracleHubError::FeedSlotOccupied);
        }

        let threshold = if stale_threshold_seconds == 0 {
            types::DEFAULT_STALE_THRESHOLD_SECONDS
        } else {
            stale_threshold_seconds
        };
        let weight = if weight_bps == 0 {
            types::DEFAULT_FEED_WEIGHT_BPS
        } else {
            weight_bps
        };

        let feed = PriceFeed {
            asset: asset.clone(),
            oracle_address: oracle_address.clone(),
            priority,
            enabled: true,
            stale_threshold_seconds: threshold,
            registered_at: env.ledger().timestamp(),
            mode,
            weight_bps: weight,
        };

        env.storage().instance().set(
            &storage::DataKey::Feed(asset.clone(), priority as u32),
            &feed,
        );
        feeds::register_slot(&env, &asset, priority as u32);
        feeds::bump_feed_count(&env);
        // A new source changes what a correct aggregate looks like.
        cache::invalidate_all(&env);

        types::FeedRegisteredEvent {
            asset,
            oracle: oracle_address,
            priority: priority as u32,
            mode,
            weight_bps: weight,
        }
        .publish(&env);
    }

    /// Governance updates an existing feed's staleness, mode, and weight.
    pub fn update_feed(
        env: Env,
        asset: Bytes,
        priority: FeedPriority,
        stale_threshold_seconds: u64,
        mode: FeedMode,
        weight_bps: u32,
    ) {
        require_governance(&env).require_auth();
        require_not_frozen(&env);

        let feed_key = storage::DataKey::Feed(asset.clone(), priority as u32);
        let mut feed: PriceFeed = env
            .storage()
            .instance()
            .get(&feed_key)
            .unwrap_or_else(|| panic_with_error!(&env, OracleHubError::FeedNotFound));
        feed.stale_threshold_seconds = if stale_threshold_seconds == 0 {
            types::DEFAULT_STALE_THRESHOLD_SECONDS
        } else {
            stale_threshold_seconds
        };
        feed.mode = mode;
        feed.weight_bps = if weight_bps == 0 {
            types::DEFAULT_FEED_WEIGHT_BPS
        } else {
            weight_bps
        };
        env.storage().instance().set(&feed_key, &feed);
        cache::invalidate_all(&env);

        types::FeedUpdatedEvent {
            asset,
            priority: priority as u32,
            mode,
            stale_threshold_seconds: feed.stale_threshold_seconds,
        }
        .publish(&env);
    }

    /// View an asset's feed configuration for a priority slot.
    pub fn get_feed(env: Env, asset: Bytes, priority: FeedPriority) -> Option<PriceFeed> {
        feeds::feed(&env, &asset, priority)
    }

    /// Registered feed slots of an asset, ascending.
    pub fn get_feed_slots(env: Env, asset: Bytes) -> Vec<u32> {
        feeds::feed_index(&env, &asset)
    }

    /// Every registered feed of an asset, in slot order.
    pub fn get_feeds(env: Env, asset: Bytes) -> Vec<PriceFeed> {
        feeds::feeds(&env, &asset)
    }

    /// Total number of feed slots registered across all assets.
    pub fn get_feed_count(env: Env) -> u32 {
        feeds::feed_count(&env)
    }

    pub fn disable_feed(env: Env, asset: Bytes, priority: FeedPriority) {
        require_governance(&env).require_auth();
        let feed_key = storage::DataKey::Feed(asset.clone(), priority as u32);
        let mut feed: PriceFeed = env
            .storage()
            .instance()
            .get(&feed_key)
            .unwrap_or_else(|| panic_with_error!(&env, OracleHubError::FeedNotFound));
        feed.enabled = false;
        env.storage().instance().set(&feed_key, &feed);
        cache::invalidate_asset(&env, &asset);
        types::FeedDisabledEvent {
            asset,
            priority: priority as u32,
        }
        .publish(&env);
    }

    pub fn enable_feed(env: Env, asset: Bytes, priority: FeedPriority) {
        require_governance(&env).require_auth();
        let feed_key = storage::DataKey::Feed(asset.clone(), priority as u32);
        let mut feed: PriceFeed = env
            .storage()
            .instance()
            .get(&feed_key)
            .unwrap_or_else(|| panic_with_error!(&env, OracleHubError::FeedNotFound));
        feed.enabled = true;
        env.storage().instance().set(&feed_key, &feed);
        cache::invalidate_asset(&env, &asset);
        types::FeedEnabledEvent {
            asset,
            priority: priority as u32,
        }
        .publish(&env);
    }

    // ── Aggregation configuration ──────────────────────────────────────────

    /// Default aggregation strategy used when no per-asset override exists.
    pub fn get_default_strategy(env: Env) -> AggregationStrategy {
        env.storage()
            .instance()
            .get(&storage::DataKey::DefaultStrategy)
            .unwrap_or(AggregationStrategy::Median)
    }

    /// Governance sets the default strategy or a per-asset override.
    pub fn set_aggregation_strategy(env: Env, asset: Option<Bytes>, strategy: AggregationStrategy) {
        require_governance(&env).require_auth();
        match asset {
            Some(asset) => {
                let mut params = stored_params(&env, &asset);
                params.strategy = strategy;
                env.storage()
                    .instance()
                    .set(&storage::DataKey::AggregationParams(asset.clone()), &params);
                types::AssetStrategyUpdatedEvent { asset, strategy }.publish(&env);
            }
            None => {
                env.storage()
                    .instance()
                    .set(&storage::DataKey::DefaultStrategy, &strategy);
                types::DefaultStrategyUpdatedEvent { strategy }.publish(&env);
            }
        }
        cache::invalidate_all(&env);
    }

    /// Effective aggregation strategy for an asset (per-asset override or default).
    pub fn get_aggregation_strategy(env: Env, asset: Bytes) -> AggregationStrategy {
        effective_params(&env, &asset).strategy
    }

    /// Governance sets the deviation band and the minimum source count.
    ///
    /// `max_deviation_bps` of `0` disables the deviation checks, which leaves
    /// only the aggregation-level outlier filter. `min_sources` of `0` means
    /// "inherit the hub default". A deviation band above 10_000 bps is
    /// meaningless and is rejected.
    pub fn set_aggregation_params(
        env: Env,
        asset: Bytes,
        max_deviation_bps: i128,
        min_sources: u32,
    ) {
        require_governance(&env).require_auth();
        require_not_frozen(&env);
        if !(0..=types::BPS_DENOM).contains(&max_deviation_bps)
            || min_sources > types::MAX_FEEDS_PER_ASSET
        {
            panic_with_error!(&env, OracleHubError::InvalidConfig);
        }
        let mut params = stored_params(&env, &asset);
        params.max_deviation_bps = max_deviation_bps;
        params.deviation_configured = true;
        params.min_sources = min_sources;
        env.storage()
            .instance()
            .set(&storage::DataKey::AggregationParams(asset.clone()), &params);
        cache::invalidate_all(&env);
        types::AggregationParamsUpdatedEvent {
            asset,
            strategy: params.strategy,
            max_deviation_bps,
            min_sources,
        }
        .publish(&env);
    }

    /// Effective aggregation parameters for an asset, with defaults resolved.
    pub fn get_aggregation_params(env: Env, asset: Bytes) -> AggregationParams {
        effective_params(&env, &asset)
    }

    // ── Price precision ────────────────────────────────────────────────────

    /// Canonical precision every aggregated price is expressed in.
    pub fn get_price_decimals(env: Env) -> u32 {
        provider::canonical_decimals(&env)
    }

    /// Governance sets the canonical precision.
    ///
    /// Prices aggregated before the change are not comparable with prices
    /// aggregated after it, so the change is bounded and invalidates the
    /// cache.
    pub fn set_price_decimals(env: Env, decimals: u32) {
        require_governance(&env).require_auth();
        require_not_frozen(&env);
        if decimals > types::MAX_PROVIDER_DECIMALS {
            panic_with_error!(&env, OracleHubError::InvalidConfig);
        }
        provider::set_canonical_decimals(&env, decimals);
        cache::invalidate_all(&env);
    }

    // ── Price cache ────────────────────────────────────────────────────────

    /// Configured price cache TTL in seconds (`0` = disabled).
    pub fn get_cache_ttl(env: Env) -> u64 {
        cache::cache_ttl(&env)
    }

    /// Governance enables, retunes, or disables the price cache.
    pub fn set_cache_ttl(env: Env, ttl_seconds: u64) {
        require_governance(&env).require_auth();
        if !cache::valid_ttl(ttl_seconds) {
            panic_with_error!(&env, OracleHubError::InvalidConfig);
        }
        cache::set_cache_ttl(&env, ttl_seconds);
        CacheConfigUpdatedEvent {
            cache_ttl_seconds: ttl_seconds,
        }
        .publish(&env);
    }

    /// Cache hit/miss counters.
    pub fn get_cache_stats(env: Env) -> CacheStats {
        cache::stats(&env)
    }

    /// Memoized price for an asset, if one is stored.
    pub fn get_cached_price(env: Env, asset: Bytes) -> Option<types::CachedPrice> {
        cache::entry(&env, &asset)
    }

    /// Drops the memoized price for an asset. Governance-only, so a keeper can
    /// force the next read to hit its providers.
    pub fn invalidate_cache(env: Env, asset: Bytes) {
        require_governance(&env).require_auth();
        cache::invalidate_asset(&env, &asset);
    }

    // ── Emergency controls ─────────────────────────────────────────────────

    pub fn freeze(env: Env) {
        let governance = require_governance(&env);
        governance.require_auth();
        env.storage()
            .instance()
            .set(&storage::DataKey::Frozen, &true);
        cache::invalidate_all(&env);
        types::FrozenEvent { admin: governance }.publish(&env);
    }

    pub fn unfreeze(env: Env) {
        let governance = require_governance(&env);
        governance.require_auth();
        env.storage()
            .instance()
            .set(&storage::DataKey::Frozen, &false);
        cache::invalidate_all(&env);
        types::UnfrozenEvent { admin: governance }.publish(&env);
    }

    pub fn is_frozen(env: Env) -> bool {
        is_globally_frozen(&env)
    }

    /// Governance freezes a single asset's pricing for the default cooldown.
    pub fn freeze_asset(env: Env, asset: Bytes) {
        require_governance(&env).require_auth();
        health::freeze_asset(&env, &asset);
        cache::invalidate_asset(&env, &asset);
    }

    pub fn unfreeze_asset(env: Env, asset: Bytes) {
        require_governance(&env).require_auth();
        health::unfreeze_asset(&env, &asset);
        cache::invalidate_asset(&env, &asset);
    }

    pub fn is_asset_frozen(env: Env, asset: Bytes) -> bool {
        health::is_frozen(&env, &asset)
    }

    // ── Price reporting (push) ─────────────────────────────────────────────

    /// A registered push provider reports a new price for its feed slot.
    ///
    /// The price must already be expressed in the canonical precision
    /// ([`Self::get_price_decimals`]); pull providers declare their own
    /// precision and are rescaled by the hub.
    pub fn report_price(
        env: Env,
        asset: Bytes,
        price: i128,
        confidence: u32,
        priority: FeedPriority,
    ) {
        let feed_key = storage::DataKey::Feed(asset.clone(), priority as u32);
        let feed: PriceFeed = env
            .storage()
            .instance()
            .get(&feed_key)
            .unwrap_or_else(|| panic_with_error!(&env, OracleHubError::FeedNotFound));

        feed.oracle_address.require_auth();

        require_not_frozen(&env);
        if !feed.enabled {
            panic_with_error!(&env, OracleHubError::FeedDisabled);
        }
        if price <= 0 {
            panic_with_error!(&env, OracleHubError::InvalidPrice);
        }

        let price_point = PricePoint {
            asset: asset.clone(),
            price,
            timestamp: env.ledger().timestamp(),
            confidence,
        };
        let latest_key = storage::DataKey::LatestPrice(asset.clone(), priority as u32);
        env.storage().instance().set(&latest_key, &price_point);

        // The report is the slot's heartbeat.
        heartbeat::record_beat(&env, &asset, priority as u32);

        // Reporting is what the programme pays for, so an accepted report earns
        // its reward here and nowhere else.
        incentives::accrue(&env, &feed.oracle_address, &asset);

        types::PriceReportedEvent {
            asset,
            priority: priority as u32,
            price,
            confidence,
        }
        .publish(&env);
    }

    // ── Price pulls (provider interface) ───────────────────────────────────

    /// Pull a live price from any registered `PriceProvider` contract.
    ///
    /// Returns the quote exactly as the provider reported it; use
    /// [`Self::get_price`] to get a quote already rescaled to the canonical
    /// precision.
    pub fn fetch_provider_price(env: Env, asset: Bytes, provider: Address) -> ProviderPrice {
        require_not_frozen(&env);
        provider::fetch_provider_price(&env, &asset, &provider)
    }

    // ── Price queries ──────────────────────────────────────────────────────

    /// Raw price (i128) for an asset using its effective strategy.
    pub fn price(env: Env, asset: Bytes) -> i128 {
        resolve(&env, &asset).price
    }

    /// Aggregate every usable source for an asset into a single price.
    ///
    /// Pull-mode feeds are fetched live from their provider; push-mode feeds
    /// use the latest reported point. Stale feeds are auto-disabled, sources
    /// that deviate from the rest are demoted, and the surviving quotes are
    /// combined by the asset's strategy. A successful read self-heals any
    /// auto-opened asset breaker and memoizes the result.
    ///
    /// Returns a memoized price when the cache is enabled and its entry is
    /// still inside its effective TTL; `from_cache` says which happened.
    pub fn get_price(env: Env, asset: Bytes) -> AggregatedPrice {
        resolve(&env, &asset)
    }

    /// Recompute the price, ignoring and replacing any memoized entry.
    pub fn refresh_price(env: Env, asset: Bytes) -> AggregatedPrice {
        cache::invalidate_asset(&env, &asset);
        resolve(&env, &asset)
    }

    // ── Health monitoring ──────────────────────────────────────────────────

    /// Per-feed health classification for all registered slots of an asset.
    pub fn check_feed_health(env: Env, asset: Bytes) -> Vec<types::FeedStatus> {
        health::check_feeds(&env, &asset)
    }

    /// Record a failed fetch; auto-opens the per-asset breaker on threshold.
    pub fn monitor_oracle_health(env: Env, asset: Bytes) -> OracleHealthStatus {
        health::monitor_oracle_health(&env, &asset)
    }

    /// Record a successful fetch, resetting the failure counter and last-success.
    pub fn record_oracle_success(env: Env, asset: Bytes) {
        health::record_oracle_success(&env, &asset);
    }

    /// Read-only health snapshot for an asset.
    pub fn get_health(env: Env, asset: Bytes) -> OracleHealthStatus {
        health::get_health(&env, &asset)
    }

    // ── Heartbeat monitoring ───────────────────────────────────────────────

    /// Heartbeat expectations for an asset, or the hub default.
    pub fn get_heartbeat_config(env: Env, asset: Option<Bytes>) -> HeartbeatConfig {
        match asset {
            Some(asset) => heartbeat::config(&env, &asset),
            None => heartbeat::default_config(&env),
        }
    }

    /// Set heartbeat expectations for one asset, or for every asset.
    ///
    /// A `None` asset rewrites the default that unconfigured assets inherit, so
    /// a hub can turn monitoring on protocol-wide and override single assets
    /// later. Governance only.
    pub fn set_heartbeat_config(env: Env, asset: Option<Bytes>, config: HeartbeatConfig) {
        let governance = require_governance(&env);
        governance.require_auth();
        require_not_frozen(&env);
        heartbeat::set_config(&env, asset.as_ref(), &config);
    }

    /// Liveness of every registered feed slot of an asset, without side
    /// effects. Use this to alert; use [`Self::sweep_heartbeats`] to act.
    pub fn get_heartbeats(env: Env, asset: Bytes) -> Vec<types::HeartbeatSlotStatus> {
        heartbeat::statuses(&env, &asset)
    }

    /// Liveness of a single feed slot.
    pub fn get_heartbeat(
        env: Env,
        asset: Bytes,
        priority: FeedPriority,
    ) -> types::HeartbeatSlotStatus {
        heartbeat::slot_status(&env, &asset, priority as u32)
    }

    /// Advance heartbeat state and trip the asset breaker on expiry.
    ///
    /// Permissionless: a keeper, a consumer, or anyone else may call it. The
    /// call is idempotent and publishes only the transitions it has not
    /// published before, so running it on a schedule costs nothing extra.
    pub fn sweep_heartbeats(env: Env, asset: Bytes) -> Vec<types::HeartbeatSlotStatus> {
        require_not_frozen(&env);
        heartbeat::sweep(&env, &asset)
    }

    /// Ledger time at which a slot's silence expires, or `0` when it has never
    /// reported.
    pub fn heartbeat_expiry(env: Env, asset: Bytes, priority: FeedPriority) -> u64 {
        heartbeat::expires_at(&env, &asset, priority as u32)
    }

    // -- Price history -----------------------------------------------------

    /// Set how many resolved prices are retained for audit.
    ///
    /// Pass `None` for the hub-wide default, or an asset for an override. A
    /// limit of `0` disables history for that scope and drops what it holds;
    /// lowering a limit prunes the entries that no longer fit.
    pub fn set_history_limit(env: Env, asset: Option<Bytes>, max_entries: u32) {
        require_not_frozen(&env);
        require_governance(&env).require_auth();
        history::set_limit(&env, asset.as_ref(), max_entries);
    }

    /// Retention configured for an asset, and how much of it is used.
    pub fn get_history_config(env: Env, asset: Bytes) -> PriceHistoryConfig {
        history::config(&env, &asset)
    }

    /// The retained history of an asset, oldest first.
    pub fn get_price_history(env: Env, asset: Bytes) -> Vec<types::PriceHistoryEntry> {
        history::entries(&env, &asset)
    }

    /// The retained history of an asset within a closed timestamp window.
    pub fn get_price_history_range(
        env: Env,
        asset: Bytes,
        from_timestamp: u64,
        to_timestamp: u64,
    ) -> Vec<types::PriceHistoryEntry> {
        history::entries_between(&env, &asset, from_timestamp, to_timestamp)
    }

    /// Drop an asset's history entirely.
    pub fn clear_price_history(env: Env, asset: Bytes) {
        require_not_frozen(&env);
        require_governance(&env).require_auth();
        if history::limit(&env, &asset) == 0 {
            return;
        }
        history::clear(&env, &asset);
    }

    // ── Reporter incentives ─────────────────────────────────────────────────

    /// Choose the token reporter rewards are paid in. Settable once: outstanding
    /// rewards are denominated in it, and swapping it afterwards would
    /// reinterpret them.
    pub fn set_reward_token(env: Env, token: Address) {
        require_not_frozen(&env);
        require_governance(&env).require_auth();
        incentives::set_token(&env, &token);
    }

    /// Turn the incentive programme on or off. Turning it off stops accrual;
    /// already earned rewards stay claimable.
    pub fn set_incentives_enabled(env: Env, enabled: bool) {
        require_not_frozen(&env);
        require_governance(&env).require_auth();
        incentives::set_enabled(&env, enabled);
    }

    /// Set the reward for one accepted report of an asset, in the reward
    /// token's base units. Pass `None` for the hub-wide default.
    ///
    /// Refuses a rate above `incentives::MAX_REWARD_PER_REPORT`, so a typo
    /// cannot drain a funded pool in one block.
    pub fn set_reward_per_report(env: Env, asset: Option<Bytes>, reward: i128) {
        require_not_frozen(&env);
        require_governance(&env).require_auth();
        incentives::set_rate(&env, asset.as_ref(), reward);
    }

    /// Set how long one oracle must wait before a second report of the same
    /// asset earns a reward again. `0` removes the guard.
    pub fn set_reward_min_interval(env: Env, seconds: u64) {
        require_not_frozen(&env);
        require_governance(&env).require_auth();
        incentives::set_min_interval(&env, seconds);
    }

    /// Incentive settings in force for an asset, as a view.
    pub fn get_incentive_config(env: Env, asset: Option<Bytes>) -> IncentiveConfig {
        incentives::config(&env, asset.as_ref())
    }

    /// Governance tops the reward pool up.
    pub fn fund_rewards(env: Env, amount: i128) {
        require_not_frozen(&env);
        let governance = require_governance(&env);
        governance.require_auth();
        incentives::fund(&env, &governance, amount);
    }

    /// Pay a reporter everything it has earned.
    ///
    /// Reverts when the pool cannot cover the claim, rather than paying out
    /// less than the books promise. The reporter's own authorization is
    /// required.
    pub fn claim_rewards(env: Env, oracle: Address) {
        require_not_frozen(&env);
        oracle.require_auth();
        incentives::claim(&env, &oracle);
    }

    /// Governance recovers tokens from the reward pool, e.g. to wind the
    /// programme down. Refuses to take the pool below what reporters have
    /// already earned.
    pub fn withdraw_rewards(env: Env, to: Address, amount: i128) {
        require_not_frozen(&env);
        require_governance(&env).require_auth();
        incentives::withdraw(&env, &to, amount);
    }

    /// A reporter's earnings and the state of the pool funding them.
    pub fn get_rewards(env: Env, oracle: Address) -> ReporterRewards {
        incentives::rewards_of(&env, &oracle)
    }
}

// ── Internal helpers ────────────────────────────────────────────────────────

fn is_globally_frozen(env: &Env) -> bool {
    env.storage()
        .instance()
        .get::<_, bool>(&storage::DataKey::Frozen)
        .unwrap_or(false)
}

fn require_not_frozen(env: &Env) {
    if is_globally_frozen(env) {
        panic_with_error!(env, OracleHubError::Frozen);
    }
}

fn require_governance(env: &Env) -> Address {
    env.storage()
        .instance()
        .get(&storage::DataKey::Governance)
        .unwrap_or_else(|| panic_with_error!(env, OracleHubError::NotInitialized))
}

/// Per-asset parameters as stored, without default resolution.
fn stored_params(env: &Env, asset: &Bytes) -> AggregationParams {
    env.storage()
        .instance()
        .get::<_, AggregationParams>(&storage::DataKey::AggregationParams(asset.clone()))
        .unwrap_or(AggregationParams {
            strategy: AggregationStrategy::Median,
            max_deviation_bps: 0,
            deviation_configured: false,
            min_sources: 0,
        })
}

/// Per-asset parameters with hub defaults filled in.
fn effective_params(env: &Env, asset: &Bytes) -> AggregationParams {
    let stored = env
        .storage()
        .instance()
        .get::<_, AggregationParams>(&storage::DataKey::AggregationParams(asset.clone()));
    let strategy = match stored {
        Some(params) => params.strategy,
        None => env
            .storage()
            .instance()
            .get(&storage::DataKey::DefaultStrategy)
            .unwrap_or(AggregationStrategy::Median),
    };
    AggregationParams {
        strategy,
        max_deviation_bps: match stored {
            Some(params) if params.deviation_configured => params.max_deviation_bps,
            _ => types::OUTLIER_DEVIATION_BPS,
        },
        deviation_configured: matches!(stored, Some(params) if params.deviation_configured),
        min_sources: match stored {
            Some(params) if params.min_sources > 0 => params.min_sources,
            _ => types::DEFAULT_MIN_SOURCES,
        },
    }
}

/// Collect the quotes of every usable source, in ascending slot order.
fn collect_quotes(env: &Env, asset: &Bytes) -> (Vec<FeedQuote>, bool) {
    let now = env.ledger().timestamp();
    let canonical = provider::canonical_decimals(env);
    let slots = feeds::feed_index(env, asset);
    let mut quotes: Vec<FeedQuote> = Vec::new(env);
    let mut pulled = false;
    let len = slots.len();
    let mut i = 0u32;
    while i < len {
        let slot = slots.get(i).unwrap_or(0);
        i += 1;

        let feed = match feeds::feed_at(env, asset, slot) {
            Some(feed) => feed,
            None => continue,
        };
        if !feed.enabled {
            continue;
        }

        let latest_key = storage::DataKey::LatestPrice(asset.clone(), slot);
        let point: Option<PricePoint> = if feed.mode == FeedMode::Pull {
            // Live pull from the provider. Failures revert the read, which is
            // the safe behavior: never return a price when a registered
            // provider is unavailable.
            let fetched =
                provider::fetch_normalized_price(env, asset, &feed.oracle_address, canonical);
            pulled = true;
            types::PricePulledEvent {
                asset: asset.clone(),
                provider: feed.oracle_address.clone(),
                price: fetched.price,
                confidence: fetched.confidence,
            }
            .publish(env);
            let point = provider::to_price_point(env, asset, fetched);
            env.storage().instance().set(&latest_key, &point);
            // A live pull is proof the provider is alive, so it counts as a
            // heartbeat for the slot.
            heartbeat::record_beat(env, asset, slot);
            Some(point)
        } else {
            env.storage().instance().get::<_, PricePoint>(&latest_key)
        };

        if let Some(point) = point {
            if now.saturating_sub(point.timestamp) > feed.stale_threshold_seconds {
                auto_disable_feed(env, asset, slot);
                continue;
            }
            quotes.push_back(FeedQuote {
                price: point.price,
                timestamp: point.timestamp,
                confidence: point.confidence,
                priority: slot,
                weight_bps: feed.weight_bps,
                stale_threshold_seconds: feed.stale_threshold_seconds,
            });
        }
    }
    (quotes, pulled)
}

/// The single price-resolution path behind `price`, `get_price`, and
/// `refresh_price`.
///
/// Order of operations: freeze checks, cache lookup, source collection,
/// deviation-checked selection, quorum check, aggregation, breaker recovery,
/// and finally the cache write.
fn resolve(env: &Env, asset: &Bytes) -> AggregatedPrice {
    require_not_frozen(env);
    if health::is_frozen(env, asset) {
        panic_with_error!(env, OracleHubError::Frozen);
    }
    // An expired heartbeat means a monitored source stopped reporting. Serving
    // its last price anyway is how a dead oracle becomes a protocol loss.
    if heartbeat::is_expired(env, asset) {
        panic_with_error!(env, OracleHubError::HeartbeatExpired);
    }

    let now = env.ledger().timestamp();
    if let Some(hit) = cache::fresh_entry(env, asset, now) {
        return cache::record_hit(env, asset, &hit, now);
    }

    let params = effective_params(env, asset);
    let (quotes, pulled) = collect_quotes(env, asset);
    cache::record_miss(env, pulled);

    if quotes.is_empty() {
        panic_with_error!(env, OracleHubError::NoActiveFeeds);
    }
    let candidates = quotes.len();

    // Deviation check: demote the leading source when the rest disagree.
    let selection = fallback::select_sources(env, asset, &quotes, params.max_deviation_bps);
    if candidates > 1 && selection.accepted.len() < params.min_sources {
        panic_with_error!(env, OracleHubError::InsufficientSources);
    }

    let outcome = aggregation::aggregate(
        env,
        &selection.accepted,
        params.strategy,
        params.max_deviation_bps,
    )
    .unwrap_or_else(|_| panic_with_error!(env, OracleHubError::NoActiveFeeds));

    let source = if candidates == 1 {
        PriceSource::Sole
    } else {
        PriceSource::Consensus
    };
    // A price that did not come from a multi-source consensus — either
    // because the leader was demoted or because only one source was usable —
    // is flagged so consumers can tighten their own risk parameters.
    let used_fallback = selection.demoted_leading || candidates < 2;

    let aggregated = AggregatedPrice {
        price: outcome.price,
        timestamp: outcome.timestamp,
        confidence: outcome.confidence,
        num_feeds: outcome.kept,
        num_active_feeds: candidates,
        strategy: params.strategy,
        rejected_sources: selection.rejected + outcome.rejected,
        used_fallback,
        from_cache: false,
        source,
        deviation_bps: if selection.demoted_leading {
            selection.deviation_bps
        } else {
            outcome.deviation_bps
        },
    };

    // A successful read proves the asset recovered; clear any auto-opened breaker.
    health::recover_breaker_if_healthy(env, asset);
    cache::store(
        env,
        asset,
        aggregated.clone(),
        now,
        outcome.min_stale_threshold,
    );
    // Recorded only on a fresh resolution, so the audit trail is the sequence of
    // prices the hub published rather than one row per consumer that read them.
    history::record(
        env,
        asset,
        aggregated.price,
        aggregated.confidence,
        aggregated.num_feeds,
        aggregated.num_active_feeds,
        &aggregated.strategy,
        aggregated.used_fallback,
        aggregated.deviation_bps,
    );
    aggregated
}

fn auto_disable_feed(env: &Env, asset: &Bytes, priority: u32) {
    let feed_key = storage::DataKey::Feed(asset.clone(), priority);
    let mut feed: PriceFeed = env
        .storage()
        .instance()
        .get(&feed_key)
        .expect("Feed not found");
    if feed.enabled {
        feed.enabled = false;
        env.storage().instance().set(&feed_key, &feed);
        cache::invalidate_asset(env, asset);
        types::FeedAutoDisabledEvent {
            asset: asset.clone(),
            priority,
        }
        .publish(env);
    }
}
