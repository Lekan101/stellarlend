//! Storage keys for the Oracle Hub contract.

use soroban_sdk::{contracttype, Address, Bytes};

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    /// Governance address authorized to manage feeds, strategies, and upgrades.
    Governance,
    /// Secondary administrator address (informational role).
    Admin,
    /// Current contract version.
    Version,
    /// Global emergency freeze flag.
    Frozen,
    /// Number of registered feeds across all assets.
    FeedCount,
    /// Ascending list of feed slots registered for an asset.
    /// Value: `soroban_sdk::Vec<u32>`. Lets the read path touch only the
    /// slots that exist instead of probing every slot.
    FeedIndex(Bytes),
    /// Per (asset, priority) feed configuration. Value: `crate::types::PriceFeed`.
    Feed(Bytes, u32),
    /// Latest price point per (asset, priority). Value: `crate::types::PricePoint`.
    LatestPrice(Bytes, u32),
    /// Per-asset aggregation strategy override. Value: `crate::types::AggregationStrategy`.
    Strategy(Bytes),
    /// Default aggregation strategy used when no per-asset override exists.
    DefaultStrategy,
    /// Per-asset deviation/fallback parameters.
    /// Value: `crate::types::AggregationParams`.
    AggregationParams(Bytes),
    /// Price cache TTL in seconds. `0` disables the cache.
    CacheTtlSeconds,
    /// Monotonic counter bumped by every governance change that invalidates
    /// memoized prices. Value: `u32`.
    CacheEpoch,
    /// Cache observability counters. Value: `crate::types::CacheStats`.
    CacheStats,
    /// Memoized aggregate per asset. Value: `crate::types::CachedPrice`.
    CachedPrice(Bytes),
    /// Canonical decimal precision every aggregated price is expressed in.
    /// Value: `u32`.
    PriceDecimals,
    /// Per-asset circuit-breaker state. Value: `crate::types::BreakerState`.
    AssetBreaker(Bytes),
    /// Consecutive failed price fetches for an asset. Value: `u32`.
    ConsecutiveFailures(Bytes),
    /// Ledger timestamp of the last successful price fetch. Value: `u64`.
    LastSuccess(Bytes),
    /// Staged upgrade WASM hash. Value: `soroban_sdk::BytesN<32>`.
    ProposedWasm,
    /// Multi-signature approver set for protocol upgrades. Value: `Vec<Address>`.
    UpgradeApprovers,
    /// Number of distinct approvals required to execute an upgrade. Value: `u32`.
    UpgradeThreshold,
    /// Approvals collected on the pending upgrade. Value: `Vec<Address>`.
    UpgradeApprovals,
    /// Ledger timestamp at which the pending upgrade was staged. Value: `u64`.
    UpgradeStagedAt,
    /// Ledger timestamp after which the pending upgrade may be executed. Value: `u64`.
    UpgradeTimelockUntil,
    /// Default heartbeat expectations for assets without an override.
    /// Value: `crate::types::HeartbeatConfig`.
    DefaultHeartbeatConfig,
    /// Per-asset heartbeat expectations.
    /// Value: `crate::types::HeartbeatConfig`.
    HeartbeatConfig(Bytes),
    /// Liveness bookkeeping for one (asset, slot) pair.
    /// Value: `crate::types::HeartbeatSlotState`.
    Heartbeat(Bytes, u32),
    /// Ledger time at which the default heartbeat config was last armed.
    /// Value: `u64`.
    DefaultHeartbeatArmedAt,
    /// Ledger time at which an asset's heartbeat config was last armed, the
    /// start of the grace window for slots that have never reported.
    /// Value: `u64`.
    HeartbeatArmedAt(Bytes),
    /// Hub-wide price-history retention, `0` when history is off.
    /// Value: `u32`.
    DefaultHistoryLimit,
    /// Per-asset price-history retention, overriding the hub-wide default.
    /// Value: `u32`.
    HistoryLimit(Bytes),
    /// Price-history entries ever appended for an asset.
    /// Value: `u32`.
    HistoryCount(Bytes),
    /// One retained price, in a fixed ring slot.
    /// Value: `crate::types::PriceHistoryEntry`.
    HistoryEntry(Bytes, u32),
    /// Whether accepted reports earn a reward. Value: `bool`.
    IncentivesEnabled,
    /// Token reporter rewards are paid in, settable once. Value: `Address`.
    RewardToken,
    /// Hub-wide reward for one accepted report, in token base units. Value: `i128`.
    DefaultRewardPerReport,
    /// Per-asset reward for one accepted report, overriding the default.
    /// Value: `i128`.
    RewardPerReport(Bytes),
    /// Minimum spacing between two rewarded reports by one oracle for one
    /// asset. Value: `u64`.
    RewardMinInterval,
    /// Claimable reward balance of one reporter, in token base units.
    /// Value: `i128`.
    Accrued(Address),
    /// Claimable reward balances owed to every reporter, in token base units.
    /// The part of the pool governance may not withdraw.
    /// Value: `i128`.
    TotalAccrued,
    /// Lifetime reward earnings of one reporter on one asset.
    /// Value: `i128`.
    Earned(Address, Bytes),
    /// How many reports of one reporter have earned a reward. Value: `u32`.
    RewardedReports(Address),
    /// Ledger time of one reporter's last rewarded report for one asset, the
    /// start of its anti-spin window. Value: `u64`.
    LastRewarded(Address, Bytes),
}
