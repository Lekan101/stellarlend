//! Shared data types, constants, and contract events for the Oracle Hub.

use soroban_sdk::{contractevent, contracttype, Address, Bytes};

/// Current protocol version baked into the initial deployment.
pub const VERSION: u32 = 2;

/// Number of feed slots an asset may register (`FeedPriority` variants).
///
/// Aggregation is bounded by this value: a quote vector can never hold more
/// than [`MAX_FEEDS_PER_ASSET`] entries, which keeps the read path gas
/// predictable no matter how many sources governance wires up.
pub const MAX_FEEDS_PER_ASSET: u32 = 5;

/// Default maximum deviation from the median (in basis points) before a quote
/// is treated as an outlier during aggregation. Defaults to 20 %.
pub const OUTLIER_DEVIATION_BPS: i128 = 2_000;

/// Default number of sources that must agree before a price is published when
/// the asset has more than one registered source.
pub const DEFAULT_MIN_SOURCES: u32 = 2;

/// Default staleness threshold used when a feed is registered without one.
pub const DEFAULT_STALE_THRESHOLD_SECONDS: u64 = 3600;

/// Default per-feed weight used by the weighted aggregation strategy.
pub const DEFAULT_FEED_WEIGHT_BPS: u32 = 10_000;

/// Canonical number of decimals every aggregated price is expressed in.
///
/// Push reporters must submit canonical-scale prices; pull providers declare
/// their own scale in [`ProviderPrice::decimals`] and the hub rescales the
/// quote before it takes part in aggregation.
pub const CANONICAL_DECIMALS: u32 = 8;

/// Largest provider decimal count the hub will rescale from. Beyond this the
/// rescale can overflow `i128`, so the quote is rejected instead.
pub const MAX_PROVIDER_DECIMALS: u32 = 18;

/// Default price cache TTL in seconds. `0` disables caching, which is the
/// default because serving a memoized price to a lending market is a
/// security-relevant choice; governance opts in with `set_cache_ttl`.
pub const DEFAULT_CACHE_TTL_SECONDS: u64 = 0;

/// Largest cache TTL governance may configure (1 hour).
pub const MAX_CACHE_TTL_SECONDS: u64 = 3600;

/// Basis-point denominator used across the hub.
pub const BPS_DENOM: i128 = 10_000;

/// Priority of a feed slot. Lower slots are consulted first during fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[contracttype]
pub enum FeedPriority {
    /// Highest-priority feed slot for an asset.
    Primary = 0,
    /// Second feed slot; used when the primary is stale or disabled.
    Secondary = 1,
    /// Third feed slot. Historical slot of the `Fallback` tier; kept stable so
    /// previously registered deployments keep resolving the same way.
    Fallback = 2,
    /// Fourth feed slot; used only when every higher slot is unusable.
    Quaternary = 3,
    /// Fifth and final feed slot.
    Quinary = 4,
}

/// How a feed obtains prices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[contracttype]
pub enum FeedMode {
    /// Providers push prices via `report_price`.
    Push = 0,
    /// The hub pulls prices live from a `PriceProvider` contract.
    Pull = 1,
}

/// Aggregation strategy used to combine multiple feed quotes for an asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[contracttype]
pub enum AggregationStrategy {
    /// Robust, outlier-resistant median across active feeds.
    Median = 0,
    /// Confidence- and weight-adjusted mean across active feeds.
    Weighted = 1,
    /// Mean of the accepted quotes after dropping the highest and the lowest.
    /// Robust against a single corrupted source on either side of the median.
    TrimmedMean = 2,
}

/// Origin of the price returned by `get_price`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[contracttype]
pub enum PriceSource {
    /// Aggregated from every source that passed the deviation check.
    Consensus = 0,
    /// The asset's only usable source.
    Sole = 1,
    /// A memoized price served from the cache entry.
    Cached = 2,
}

/// Per-asset aggregation and deviation-check parameters.
///
/// A `0` on either numeric field means "inherit the hub default", which keeps
/// the stored struct small for the common case.
#[derive(Clone, Copy, Debug, PartialEq)]
#[contracttype]
pub struct AggregationParams {
    /// Strategy used for this asset.
    pub strategy: AggregationStrategy,
    /// Maximum deviation (bps) from the median before a source is rejected.
    /// Read only when `deviation_configured` is set; a value of `0` then
    /// disables the deviation check entirely.
    pub max_deviation_bps: i128,
    /// Whether governance pinned `max_deviation_bps` for this asset. Without
    /// it the hub default applies, which keeps `0` free to mean "no deviation
    /// check" instead of "unset".
    pub deviation_configured: bool,
    /// Sources that must agree before a price is published.
    /// `0` inherits [`DEFAULT_MIN_SOURCES`].
    pub min_sources: u32,
}

/// Registered configuration of a single feed slot for an asset.
#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub struct PriceFeed {
    /// Asset this feed prices.
    pub asset: Bytes,
    /// Oracle address. For `Push` feeds this is the reporter that must
    /// authorize `report_price`; for `Pull` feeds it is the `PriceProvider`
    /// contract the hub queries.
    pub oracle_address: Address,
    /// Priority slot this feed occupies.
    pub priority: FeedPriority,
    /// Whether the feed participates in aggregation.
    pub enabled: bool,
    /// Maximum age (seconds) before a quote is considered stale.
    pub stale_threshold_seconds: u64,
    /// Ledger timestamp when the feed was registered.
    pub registered_at: u64,
    /// Push or pull mode.
    pub mode: FeedMode,
    /// Relative weight used by the `Weighted` strategy (0 = default weight).
    pub weight_bps: u32,
}

/// Latest reported (or last pulled) price point for a feed slot.
#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub struct PricePoint {
    /// Asset this point prices.
    pub asset: Bytes,
    /// Price in the smallest relevant unit.
    pub price: i128,
    /// Ledger timestamp when the quote was recorded.
    pub timestamp: u64,
    /// Provider confidence for the quote (0 = unknown).
    pub confidence: u32,
}

/// A single candidate quote collected during aggregation.
#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub struct FeedQuote {
    /// Price already rescaled to the canonical decimal precision.
    pub price: i128,
    pub timestamp: u64,
    pub confidence: u32,
    /// Feed slot (0..[`MAX_FEEDS_PER_ASSET`]) the quote came from.
    pub priority: u32,
    /// Feed-configured weight for the `Weighted` strategy.
    pub weight_bps: u32,
    /// Freshness budget of the source that produced the quote. A memoized
    /// price may never outlive the shortest budget that fed into it.
    pub stale_threshold_seconds: u64,
}

/// Result of aggregating the accepted quotes of an asset.
#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub struct AggregatedPrice {
    pub price: i128,
    pub timestamp: u64,
    pub confidence: u32,
    /// Number of quotes that participated in the aggregation.
    pub num_feeds: u32,
    /// Number of active (non-stale, enabled) feeds observed.
    pub num_active_feeds: u32,
    /// Strategy used for this asset.
    pub strategy: AggregationStrategy,
    /// Number of candidate sources the deviation check threw away.
    pub rejected_sources: u32,
    /// True when a lower-priority source replaced the leading one.
    pub used_fallback: bool,
    /// True when the price was served from the cache instead of being recomputed.
    pub from_cache: bool,
    /// Where the returned price came from.
    pub source: PriceSource,
    /// Largest deviation (in basis points) observed inside the accepted set.
    pub deviation_bps: i128,
}

/// Memoized aggregate plus the bookkeeping the cache needs on read.
#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub struct CachedPrice {
    /// The memoized aggregate.
    pub price: AggregatedPrice,
    /// Ledger timestamp the entry was written at.
    pub cached_at: u64,
    /// Effective TTL: the smaller of the configured TTL and the freshness
    /// budget of every source that fed the aggregate.
    pub ttl_seconds: u64,
    /// Configuration epoch the entry was produced under. Any governance
    /// change bumps the epoch, which invalidates every entry at once.
    pub epoch: u32,
}

/// Cache observability counters, exposed through `get_cache_stats`.
#[derive(Clone, Copy, Debug, PartialEq)]
#[contracttype]
pub struct CacheStats {
    /// Reads answered from the cache.
    pub hits: u32,
    /// Reads that had to recompute.
    pub misses: u32,
    /// Entries written after a recompute.
    pub writes: u32,
    /// Reads served straight from a pull provider.
    pub pull_reads: u32,
}

/// Health classification of a single feed slot.
#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub enum FeedStatusCode {
    Active = 0,
    Stale = 1,
    Disabled = 2,
    Frozen = 3,
}

/// Health snapshot of a single feed slot.
#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub struct FeedStatus {
    pub asset: Bytes,
    pub status: FeedStatusCode,
    pub last_update: u64,
    pub is_stale: bool,
}

/// Per-asset circuit-breaker state.
#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub struct BreakerState {
    /// Ledger timestamp before which pricing for the asset is halted.
    pub open_until: u64,
    /// True when the breaker was opened automatically by health monitoring.
    pub auto: bool,
}

/// Health summary for an asset used by monitoring infrastructure.
#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub struct OracleHealthStatus {
    pub asset: Bytes,
    pub consecutive_failures: u32,
    pub last_success_timestamp: u64,
    pub circuit_breaker_open: bool,
    /// True when `monitor_oracle_health` auto-opened the breaker.
    pub auto_triggered: bool,
    pub active_feeds: u32,
}

/// Price quote returned by an external `PriceProvider` contract.
#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub struct ProviderPrice {
    pub price: i128,
    pub decimals: u32,
    pub timestamp: u64,
    pub confidence: u32,
}

// ── Heartbeat monitoring ────────────────────────────────────────────────────

/// Liveness classification of a single feed slot.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub enum HeartbeatStatus {
    /// The slot reported inside its interval.
    Fresh = 0,
    /// The interval elapsed but the slot is not stale yet.
    Due = 1,
    /// The slot has missed too long and its last price is not trustworthy.
    Stale = 2,
    /// The slot has been silent past its expiry. Pricing for the asset is
    /// refused until a report arrives.
    Expired = 3,
    /// The slot has never reported since monitoring was configured.
    Silent = 4,
}

/// Per-asset (or default) heartbeat expectations.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct HeartbeatConfig {
    /// When false the asset is not monitored at all.
    pub enabled: bool,
    /// Expected reporting cadence.
    pub interval_seconds: u64,
    /// Silence past this age marks the slot stale.
    pub stale_after_seconds: u64,
    /// Silence past this age expires the slot and fails the asset closed.
    pub expiry_seconds: u64,
}

impl HeartbeatConfig {
    /// Monitoring off; every slot is treated as healthy.
    pub fn disabled() -> Self {
        HeartbeatConfig {
            enabled: false,
            interval_seconds: crate::heartbeat::DEFAULT_INTERVAL_SECONDS,
            stale_after_seconds: crate::heartbeat::DEFAULT_STALE_AFTER_SECONDS,
            expiry_seconds: crate::heartbeat::DEFAULT_EXPIRY_SECONDS,
        }
    }
}

/// Persisted liveness bookkeeping for one feed slot.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct HeartbeatSlotState {
    /// Ledger time of the last accepted report or successful pull.
    pub last_seen: u64,
    /// Missed intervals since `last_seen`, as of the last sweep.
    pub missed_beats: u32,
    /// Classification produced by the last sweep, so only transitions are
    /// published.
    pub last_status: HeartbeatStatus,
}

/// Read-only liveness view of one feed slot.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct HeartbeatSlotStatus {
    pub asset: Bytes,
    /// Feed slot, i.e. the `FeedPriority` of the source.
    pub priority: u32,
    pub status: HeartbeatStatus,
    /// Ledger time of the last accepted report or pull. `0` when silent.
    pub last_seen: u64,
    /// Age of that report at query time.
    pub age_seconds: u64,
    pub missed_beats: u32,
}

// ── Contract events ────────────────────────────────────────────────────────

#[contractevent]
#[derive(Clone, Debug)]
pub struct FeedRegisteredEvent {
    #[topic]
    pub asset: Bytes,
    pub oracle: Address,
    pub priority: u32,
    pub mode: FeedMode,
    pub weight_bps: u32,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct FeedUpdatedEvent {
    #[topic]
    pub asset: Bytes,
    pub priority: u32,
    pub mode: FeedMode,
    pub stale_threshold_seconds: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct FeedDisabledEvent {
    #[topic]
    pub asset: Bytes,
    pub priority: u32,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct FeedEnabledEvent {
    #[topic]
    pub asset: Bytes,
    pub priority: u32,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct PriceReportedEvent {
    #[topic]
    pub asset: Bytes,
    pub priority: u32,
    pub price: i128,
    pub confidence: u32,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct PricePulledEvent {
    #[topic]
    pub asset: Bytes,
    pub provider: Address,
    pub price: i128,
    pub confidence: u32,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct ProviderPriceRescaledEvent {
    #[topic]
    pub asset: Bytes,
    pub provider: Address,
    pub raw_price: i128,
    pub price: i128,
    pub from_decimals: u32,
    pub to_decimals: u32,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct DeviationRejectedEvent {
    #[topic]
    pub asset: Bytes,
    pub priority: u32,
    pub price: i128,
    pub reference: i128,
    pub deviation_bps: i128,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct FallbackActivatedEvent {
    #[topic]
    pub asset: Bytes,
    pub rejected_priority: u32,
    pub serving_priority: u32,
    pub remaining_sources: u32,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct PriceCachedEvent {
    #[topic]
    pub asset: Bytes,
    pub price: i128,
    pub ttl_seconds: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct CacheServedEvent {
    #[topic]
    pub asset: Bytes,
    pub price: i128,
    pub age_seconds: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct CacheConfigUpdatedEvent {
    pub cache_ttl_seconds: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct PriceDecimalsUpdatedEvent {
    pub decimals: u32,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct AggregationParamsUpdatedEvent {
    #[topic]
    pub asset: Bytes,
    pub strategy: AggregationStrategy,
    pub max_deviation_bps: i128,
    pub min_sources: u32,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct DefaultStrategyUpdatedEvent {
    pub strategy: AggregationStrategy,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct AssetStrategyUpdatedEvent {
    #[topic]
    pub asset: Bytes,
    pub strategy: AggregationStrategy,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct FeedAutoDisabledEvent {
    #[topic]
    pub asset: Bytes,
    pub priority: u32,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct BreakerOpenedEvent {
    #[topic]
    pub asset: Bytes,
    pub open_until: u64,
    pub auto: bool,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct BreakerUnfrozenEvent {
    #[topic]
    pub asset: Bytes,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct HealthFailureEvent {
    #[topic]
    pub asset: Bytes,
    pub consecutive_failures: u32,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct HealthSuccessEvent {
    #[topic]
    pub asset: Bytes,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct FrozenEvent {
    pub admin: Address,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct UnfrozenEvent {
    pub admin: Address,
}

/// Emitted when a slot's heartbeat is accepted after a degraded period.
#[contractevent]
#[derive(Clone, Debug)]
pub struct HeartbeatRecoveredEvent {
    #[topic]
    pub asset: Bytes,
    pub priority: u32,
    pub status: HeartbeatStatus,
    pub silence_seconds: u64,
}

/// Emitted when a slot crosses the stale threshold.
#[contractevent]
#[derive(Clone, Debug)]
pub struct HeartbeatStaleEvent {
    #[topic]
    pub asset: Bytes,
    pub priority: u32,
    pub age_seconds: u64,
    pub missed_beats: u32,
}

/// Emitted when a slot passes its expiry and the asset fails closed.
#[contractevent]
#[derive(Clone, Debug)]
pub struct HeartbeatExpiredEvent {
    #[topic]
    pub asset: Bytes,
    pub priority: u32,
    pub age_seconds: u64,
    pub missed_beats: u32,
}

/// Emitted when heartbeat expectations are changed.
#[contractevent]
#[derive(Clone, Debug)]
pub struct HeartbeatConfigUpdatedEvent {
    #[topic]
    pub asset: Bytes,
    pub enabled: bool,
    pub interval_seconds: u64,
    pub stale_after_seconds: u64,
    pub expiry_seconds: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct UpgradeStagedEvent {
    pub wasm_hash: soroban_sdk::BytesN<32>,
    pub staged_by: Address,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct UpgradeExecutedEvent {
    pub old_version: u32,
    pub new_version: u32,
    pub wasm_hash: soroban_sdk::BytesN<32>,
    pub executed_by: Address,
}

// The explicit topic overrides the derived snake-case name, which would
// exceed the 32-character symbol limit.
#[contractevent(topics = ["upgrade_multisig_configured"])]
#[derive(Clone, Debug)]
pub struct UpgradeMultisigConfiguredEvent {
    pub threshold: u32,
}

#[contractevent(topics = ["upgrade_approved"])]
#[derive(Clone, Debug)]
pub struct UpgradeApprovedEvent {
    #[topic]
    pub approver: Address,
    pub approval_count: u32,
    pub timelock_until: u64,
}

// -- Price history -----------------------------------------------------------

/// One resolved price, retained for audit.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct PriceHistoryEntry {
    /// Asset this price was published for.
    pub asset: Bytes,
    /// Ledger time of the resolution.
    pub timestamp: u64,
    /// Aggregated price in canonical decimals.
    pub price: i128,
    /// Aggregate confidence of the quotes behind it.
    pub confidence: u32,
    /// Quotes that participated in the aggregate.
    pub num_feeds: u32,
    /// Non-stale, enabled sources observed for the asset.
    pub num_active_feeds: u32,
    /// Strategy that produced it.
    pub strategy: AggregationStrategy,
    /// True when a single source, or a demoted leader, produced it.
    pub used_fallback: bool,
    /// Largest disagreement inside the accepted set, in basis points.
    pub deviation_bps: i128,
}

/// Configured retention for an asset and how much of it is used.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct PriceHistoryConfig {
    pub asset: Bytes,
    /// Entries retained; `0` means history is off for this asset.
    pub max_entries: u32,
    /// Entries currently retained.
    pub recorded: u32,
}

/// Emitted when history retention is changed.
///
/// The explicit topic overrides the derived snake-case name, which would exceed
/// the 32-character symbol limit.
#[contractevent(topics = ["history_config_updated"])]
#[derive(Clone, Debug)]
pub struct PriceHistoryConfigUpdatedEvent {
    #[topic]
    pub asset: Bytes,
    pub max_entries: u32,
}

/// Emitted when an asset's history is dropped, whether by a limit change or an
/// explicit clear.
#[contractevent(topics = ["history_cleared"])]
#[derive(Clone, Debug)]
pub struct PriceHistoryClearedEvent {
    #[topic]
    pub asset: Bytes,
    pub removed: u32,
}

// -- Reporter incentives ------------------------------------------------------

/// Incentive settings in force for an asset.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct IncentiveConfig {
    /// Whether an accepted report earns anything.
    pub enabled: bool,
    /// Reward for one accepted report, in the reward token's base units.
    /// `0` means reporting earns nothing for this asset.
    pub reward_per_report: i128,
    /// Minimum spacing between two rewarded reports by one oracle for one asset.
    pub min_interval_seconds: u64,
}

/// A reporter's earnings and the state of the pool funding them.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct ReporterRewards {
    pub oracle: Address,
    /// The token rewards are paid in, if governance has chosen one.
    pub token: Option<Address>,
    /// Claimable balance, in token base units.
    pub accrued: i128,
    /// Tokens the hub holds for rewards.
    pub pool_balance: i128,
    /// Everything reporters have earned and not yet claimed, hub-wide.
    pub total_owed: i128,
    /// How many of this reporter's reports have earned a reward.
    pub reports_rewarded: u32,
}

/// Emitted when the incentive programme or a reward rate changes.
#[contractevent(topics = ["incentives_config_updated"])]
#[derive(Clone, Debug)]
pub struct IncentiveConfigUpdatedEvent {
    /// Asset the change applies to; empty means it applies hub-wide.
    #[topic]
    pub asset: Bytes,
    pub enabled: bool,
    pub reward_per_report: i128,
    pub min_interval_seconds: u64,
}

/// Emitted when governance chooses the reward token. This happens once.
#[contractevent(topics = ["reward_token_set"])]
#[derive(Clone, Debug)]
pub struct RewardTokenSetEvent {
    #[topic]
    pub token: Address,
}

/// Emitted when governance funds the reward pool.
#[contractevent(topics = ["rewards_funded"])]
#[derive(Clone, Debug)]
pub struct RewardsFundedEvent {
    #[topic]
    pub funder: Address,
    pub amount: i128,
    /// Pool balance after the transfer.
    pub pool_balance: i128,
}

/// Emitted when a reporter claims what it has earned.
#[contractevent(topics = ["rewards_claimed"])]
#[derive(Clone, Debug)]
pub struct RewardsClaimedEvent {
    #[topic]
    pub oracle: Address,
    pub amount: i128,
    /// Pool balance after the payout.
    pub pool_balance: i128,
}

/// Emitted when governance recovers tokens from the reward pool.
#[contractevent(topics = ["rewards_withdrawn"])]
#[derive(Clone, Debug)]
pub struct RewardsWithdrawnEvent {
    #[topic]
    pub to: Address,
    pub amount: i128,
    /// Pool balance after the withdrawal.
    pub pool_balance: i128,
}
