# Oracle Hub – Architecture

## Overview

The hub is a single Soroban contract (`OracleHubContract`) whose logic is split
into focused modules. Prices are produced by combining one or more per-asset
`PriceFeed` slots, each of which is either **push** (an oracle signs a price)
or **pull** (an external `PriceProvider` contract is queried live). Feed
quotes are filtered for outliers and combined with the asset's effective
aggregation strategy. Health monitoring and emergency freezes gate whether an
asset may serve prices at all.

## Module layout

| Module | Responsibility |
| ------ | -------------- |
| `lib.rs` | Contract entry points, auth, global freeze, orchestration |
| `types.rs` | Data types, constants, events |
| `storage.rs` | `DataKey` layout |
| `feeds.rs` | Per-asset feed index, slot lookups, iteration |
| `interface.rs` | `PriceProvider` trait + generated `PriceProviderClient` |
| `provider.rs` | Pull-based provider fetch, validation, timestamp clamp, decimal rescale |
| `aggregation.rs` | Median, weighted, and trimmed-mean strategies, outlier filtering |
| `fallback.rs` | Deviation-checked selection of the leading source |
| `cache.rs` | TTL price cache, epoch invalidation, counters |
| `health.rs` | Feed classification, failure counters, circuit breaker |
| `heartbeat.rs` | Reporting-cadence expectations, slot liveness, expiry sweep |
| `history.rs` | Bounded per-asset retention of resolved prices for audit |
| `incentives.rs` | Governance-funded reward pool for accepted reports |
| `upgrade.rs` | Staged/apply upgrade mechanism |
| `tests/` | 13 focused test suites (158 tests) |

## Price production flow

```
report_price (push) ──► PricePoint per (asset, priority)
                          └─ accepted ─► heartbeat::record_beat
                                        incentives::accrue(oracle, asset)
provider get_price  ──► fetch_provider_price ─► PricePoint per (asset, priority)
                             (pull)
                             (pull feeds accrue no reward)

resolve(asset):                              cache::fresh_entry?
    require_not_frozen / asset breaker            └─ hit ─► record_hit, return Cached
    heartbeat::is_expired(asset)? ──► revert    cache miss
    for slot in feed_index(asset)            cache miss
        skip disabled / no point feeds
        jump out stale feeds (auto-disable; event)
        pull feeds: fetch → rescale → PricePoint
        collect FeedQuote
    fallback::select_sources(quotes, band)    leader vs median(others) ≥ 3 quotes
        └─ demoted ──► DeviationRejected + FallbackActivated events
    accepted.len() < min_sources?  ──► revert InsufficientSources
    aggregation::aggregate(accepted, strategy, band)
        reject outliers > band from median
        Median      -> lower median, avg confidence
        Weighted    -> Σ(price*w*conf)/Σ(w*conf)
        TrimmedMean -> drop both tails, average the rest
    recover auto-breaker if healthy
    cache::store(quote, clamped ttl)
    history::record(...)                      fresh resolutions only
    return AggregatedPrice
```

Effective strategy resolution in `get_aggregation_strategy`:
per-asset override (`DataKey::AggregationParams(asset)`) else the hub default
(`DataKey::DefaultStrategy`, initialized to `Median`).

## Storage layout

All keys live in instance storage. `DataKey` variants (see `storage.rs`):

| Key | Type | Purpose |
| --- | ---- | ------- |
| `Governance` | `Address` | Governor (auth for management ops) |
| `Admin` | `Address` | Admin (initialized, reserved) |
| `Version` | `u32` | Protocol version, bumped by upgrade |
| `Frozen` | `bool` | Global freeze flag |
| `DefaultStrategy` | `AggregationStrategy` | Hub-wide default |
| `AggregationParams(Bytes)` | `AggregationParams` | Per-asset strategy, deviation band, source floor |
| `FeedCount` | `u32` | Total registered feed slots |
| `FeedIndex(Bytes)` | `Vec<u32>` | Registered slots of an asset |
| `Feed(Bytes, u32)` | `PriceFeed` | Feed config per (asset, priority) |
| `LatestPrice(Bytes, u32)` | `PricePoint` | Last report / pull per slot |
| `ConsecutiveFailures(Bytes)` | `u32` | Health failure counter |
| `LastSuccess(Bytes)` | `u64` | Last successful read timestamp |
| `AssetBreaker(Bytes)` | `BreakerState` | Per-asset circuit breaker |
| `PriceDecimals` | `u32` | Canonical precision (default 8, max 18) |
| `CacheTtl` | `u64` | Configured cache TTL in seconds (default 0) |
| `CacheEpoch` | `u32` | Bumped by every governance mutation |
| `CacheStats` | `CacheStats` | hits / misses / writes / pull_reads |
| `CachedPrice(Bytes)` | `CachedPrice` | Memoized aggregate, its TTL and epoch |
| `DefaultHeartbeatConfig` | `HeartbeatConfig` | Hub-wide cadence expectations (disabled by default) |
| `HeartbeatConfig(Bytes)` | `HeartbeatConfig` | Per-asset cadence expectations |
| `Heartbeat(Bytes, u32)` | `HeartbeatSlotState` | Last beat and last published status per slot |
| `DefaultHeartbeatArmedAt` | `u64` | When the default config was armed |
| `HeartbeatArmedAt(Bytes)` | `u64` | When an asset's config was armed |
| `DefaultHistoryLimit` | `u32` | Hub-wide retention (default 0, history off) |
| `HistoryLimit(Bytes)` | `u32` | Per-asset retention override |
| `HistoryCount(Bytes)` | `u32` | Entries ever appended for an asset |
| `HistoryEntry(Bytes, u32)` | `PriceHistoryEntry` | One retained price in a fixed ring slot |
| `IncentivesEnabled` | `bool` | Whether accepted reports earn a reward |
| `RewardToken` | `Address` | Token rewards are paid in, set once |
| `DefaultRewardPerReport` | `i128` | Hub-wide reward per accepted report |
| `RewardPerReport(Bytes)` | `i128` | Per-asset reward override |
| `RewardMinInterval` | `u64` | Anti-spin spacing between rewarded reports |
| `Accrued(Address)` | `i128` | Claimable balance of one reporter |
| `TotalAccrued` | `i128` | Hub-wide liability governance cannot withdraw |
| `Earned(Address, Bytes)` | `i128` | Lifetime earnings of a reporter per asset |
| `RewardedReports(Address)` | `u32` | Reports of a reporter that earned a reward |
| `LastRewarded(Address, Bytes)` | `u64` | Start of a reporter's anti-spin window |
| `ProposedWasm` | `BytesN<32>` | Staged upgrade hash |

## Pluggable provider interface

```rust
#[contractclient(name = "PriceProviderClient")]
pub trait PriceProvider {
    fn get_price(env: Env, asset: Bytes) -> ProviderPrice;
}
```

`ProviderPrice { price: i128, decimals: u32, timestamp: u64, confidence: u32 }`.

Provider contract expectations (enforced/assumed by `provider.rs`):

- `price > 0`, otherwise the pull reverts (`InvalidPrice`-style assert). A
  failing pull makes the whole `get_price` revert — never serve a wrong price.
- `timestamp` is clamped down to the current ledger time so a future-stamped
  quote can never be treated as fresher than it is.
- read-only; no authorization required from the hub.

`fetch_provider_price(asset, provider)` exposes live pulls to callers.

## Health monitoring

Per-feed classification (`classify_feed`) produces `FeedStatusCode`:

| Status | Condition |
| ------ | --------- |
| `Active` | enabled, not frozen, last point within staleness window |
| `Stale` | enabled, no point or point older than the threshold |
| `Disabled` | feed explicitly disabled by governance or auto-disabled |
| `Frozen` | global freeze or asset breaker open |

Circuit breaker state machine (`health.rs`):

```
get_price succeeds ──(recover_breaker_if_healthy)──► breaker cleared, failures reset
       │
failures: external monitor calls monitor_oracle_health(asset):
       0 ─► 1 ─► 2 ─► 3+ (AUTO_BREAKER_FAILURE_THRESHOLD)
                            └──► open BreakerState{open_until: now+600, auto: true}
                                 └──► is_asset_frozen=true; get_price reverts (Frozen)
                                 └──► after cooldown: reads resume; successful read clears
```

- **Auto breaker** (`auto=true`): opened by the failure counter, self-heals on
  the first successful `get_price` after cooldown.
- **Governance freeze** (`auto=false`): opened by `freeze_asset`, only closed by
  `unfreeze_asset`; never self-heals.
- **Global freeze** (`Frozen=true`): halts all pricing and all mutation;
  `get_price`, `report_price`, `stage_upgrade`, and feed changes revert.

`check_feed_health(asset) -> Vec<FeedStatus>` and
`get_health(asset) -> OracleHealthStatus` are read-only views for off-chain
monitors.

## Heartbeat monitoring

`health.rs` answers "is this source serving a fresh price right now, for
somebody who is asking?". `heartbeat.rs` answers the complementary question:
"is this source still alive at all, for nobody in particular?". The existing
per-feed staleness window only fires while a price is being read, so a push
oracle that dies quietly leaves no trace at all until a consumer happens to look.

`HeartbeatConfig` (per asset, else the hub-wide default; **disabled by default**)
declares three thresholds:

| Field | Meaning |
| ----- | ------- |
| `interval_seconds` | expected reporting cadence; past it a slot is `Due` |
| `stale_after_seconds` | silence that marks a slot `Stale` |
| `expiry_seconds` | silence that marks a slot `Expired` |

Slot lifecycle (`classify`):

```
beat (report or live pull) ─► Fresh ─(interval)─► Due ─(stale_after)─► Stale ─(expiry)─► Expired
   ^                              │                                            │
   └──────────────────────────────┴──────────── a beat at any time ────────────┘
```

- A slot with no beat at all is `Silent` rather than `Fresh`, because "never
  reported" and "reported just now" are not the same claim. Its silence is
  measured from the moment monitoring was armed (`HeartbeatArmedAt`), not from
  the start of the ledger, so enabling monitoring never expires a feed
  retroactively.
- `Stale` is informational: the existing staleness path already refuses to price
  from a late source, so a stale slot is demoted while a healthy second source
  keeps serving.
- `Expired` fails the asset closed, in two places on purpose: the price path
  raises `HeartbeatExpired` so no consumer can read a price nobody updates, and
  `sweep` opens the same auto breaker the health monitor uses, so the failure is
  published even when nobody is asking. Recovery is therefore the ordinary
  breaker cooldown plus one successful read.
- **Pull feeds are exempt** (`is_exempt`): a pull source has no cadence to keep,
  so only its last successful pull is recorded and its silence never expires an
  asset. This is why `record_beat` is called from the pull path but
  `is_expired` skips pull slots.
- `sweep_heartbeats(asset)` is permissionless and idempotent. It publishes
  `HeartbeatStaleEvent` / `HeartbeatExpiredEvent` only on a *change* of the
  slot's last published status, and `record_beat` publishes
  `HeartbeatRecoveredEvent` only on the transition back, so a working oracle
  reporting every minute does not flood the log.

## Price history

`heartbeat.rs` asks whether a source is alive; `history.rs` records what it said
while it was. A price read answers a question about *now*, which is the wrong
question after the fact: a lending protocol that liquidated on a bad number needs
to know what the hub published at that ledger, with what agreement, and by which
strategy.

History is **off by default** (`DefaultHistoryLimit` = 0) and bounded per asset
(`HistoryLimit(asset)`, else the default). Storage is a fixed
`HISTORY_RING_SLOTS` (100) ring of `HistoryEntry(asset, slot)` values, with
`HistoryCount(asset)` as the append counter:

```
fresh resolution (cache miss) ─► history::record
    identical to the newest entry (same second, price, feeds, strategy)? ─► skip
    append at HistoryCount(asset) % 100
    entries(asset) = newest first, capped at the effective limit
```

- Recording happens on a fresh resolution only. A cache hit writes nothing, so
  the trail is the sequence of prices the hub *published* rather than one row
  per consumer that read them.
- A fixed ring means retention changes never reinterpret what is already stored:
  a limit above the ring size is simply capped by the slots that exist, and
  lowering a limit prunes the entries that no longer fit. Setting a limit to `0`
  drops the asset's history and disables recording for that scope.
- Each entry keeps the inputs to the decision — price, timestamp, confidence,
  participating and active feed counts, strategy, `used_fallback`, and the
  deviation inside the accepted set — so a dispute can be settled from the record
  rather than re-derived from a chain of arguments.
- Identical observations inside one ledger second collapse: one second is one
  fact about a market, however many times it is read or re-resolved.

## Reporter incentives

`incentives.rs` is the other half of `heartbeat.rs`: the hub can tell you a
source died, but it cannot make anyone stay online. A governance-funded reward
pool pays oracle addresses for the work the protocol depends on.

```
accepted report (report_price) ─► incentives::accrue(oracle, asset)
    programme off / no token / rate 0 ─► no-op
    now - LastRewarded(oracle, asset) < min_interval ─► no-op
    Accrued(oracle) += rate, TotalAccrued += rate, Earned(oracle, asset) += rate

claim_rewards(oracle)          fund_rewards(amount)         withdraw_rewards(to, amount)
    require_auth(oracle)           require_auth(governance)     require_auth(governance)
    Accrued -> 0, TotalAccrued -=  transfer in, event            balance - TotalAccrued floor
    transfer out, event            funding_makes_a_pending_claim
```

Design decisions worth stating:

- **The token balance is the pool.** No mirrored balance is kept, so a funded
  pool cannot silently disagree with what the token contract holds.
  `get_rewards` therefore reports the balance rather than a cached figure.
- **Accrual is a liability, not a payment.** An unfunded claim reverts with
  `RewardPoolShortfall` instead of paying out less than the books promise, and
  `withdraw_rewards` refuses to take the pool below `TotalAccrued`, so a promise
  made to a reporter cannot be taken back by the party that made it.
- **Only accepted reports pay.** The accrual hook sits on the write path after
  validation, so a rejected or unauthorized report earns nothing, and a pull feed
  earns nothing at all — it is queried, not reporting.
- **Two anti-drain rules.** A per-(oracle, asset) minimum interval
  (`RewardMinInterval`, default 60 s) means a spinning oracle earns at most one
  reward per window, and `set_reward_per_report` refuses a rate above
  `MAX_REWARD_PER_REPORT`.
- **One token, once.** `set_reward_token` refuses a second choice: outstanding
  rewards are denominated in it, and swapping it later would quietly redefine
  what was owed.
- **Claim safety.** The reporter authorizes its own claim, and the balance is
  zeroed before the transfer so a re-entrant token cannot claim it twice.

## Aggregation

Runs in three stages, all bounded by `MAX_FEEDS_PER_ASSET` (5):

1. **Selection** (`fallback.rs`) — see below.
2. **Outlier filter**: a quote is kept iff
   `deviation_bps(price, median(quotes)) <= band`. Non-positive prices are
   always dropped. If fewer than two quotes survive, aggregation fails closed:
   an unvalidated single quote must not control the price. A single quote with
   no competitors short-circuits and is returned directly, flagged `Sole`.
3. **Combination**:

- Median: lower median of kept prices (index `(len-1)/2`); confidence = mean of
  kept confidences; timestamp = latest.
- Weighted: effective weight `w = feed.weight_bps * max(confidence, 1)`;
  `price = Σ(price·w) / Σw`; confidence = confidence-weighted mean. Feed
  weight defaults to `10_000` bps.
- TrimmedMean: drops the highest and lowest kept price and averages the rest
  (median for fewer than three kept quotes). Removes one manipulated quote from
  either tail, which a plain mean would absorb.

The reported `deviation_bps` is re-anchored on the surviving set, so consumers
learn how far the sources they actually used disagree.

## Deviation-checked selection

`fallback::select_sources` runs before aggregation and answers one question:
may the leading source be trusted?

- **Quorum.** With fewer than three candidates there is no independent
  reference, so the leader is left alone and aggregation decides.
- **Reference.** The median of every candidate except the leader.
- **Band.** `max_deviation_bps` (default `OUTLIER_DEVIATION_BPS` = 20 %); `0`
  disables the check.
- **Demotion.** Beyond the band the leader is dropped, `DeviationRejectedEvent`
  records `{price, reference, deviation_bps}` and `FallbackActivatedEvent`
  records `{rejected_priority, serving_priority, remaining_sources}`.
- **Fail closed.** If the survivor count would drop below `min_sources` while
  the asset has more than one source, `resolve` reverts with
  `InsufficientSources` instead of publishing a thinner consensus.

Deviation is computed with saturating arithmetic and returns `i128::MAX` for
non-positive operands, so a zero or negative quote can never be counted as
agreeing with the reference.

## Price cache

`cache.rs` memoizes the aggregated price per asset for `min(configured TTL,
shortest source staleness budget)` seconds. Entries carry the configuration
epoch they were produced under, and every governance mutation
(`bump_epoch`) invalidates all of them, so a cached aggregate can never outlive
the configuration it was computed from. `set_cache_ttl` is deliberately not an
invalidating mutation. `refresh_price` bypasses the cache, and
`get_cache_stats` exposes hits, misses, writes, and pull reads.

The cache is off by default (`DEFAULT_CACHE_TTL_SECONDS = 0`): serving a
memoized price is a security decision governance has to make explicitly.

## Upgrade mechanism

```
stage_upgrade(hash) [governance, not frozen] ──► DataKey::ProposedWasm = hash
upgrade()            [governance, not frozen] ──► require staged
    1. remove ProposedWasm
    2. Version += 1
    3. emit UpgradeExecutedEvent(old, new, hash)
    4. env.deployer().update_current_contract_wasm(hash)   // atomic swap
```

Instance storage (all `DataKey` entries above) survives the swap, so upgraded
code must keep a storage-compatible layout. The swap itself is a Soroban VM
operation: it cannot be exercised in the unit-test env (which rejects
`upload_wasm`), so it is validated in integration deployments; unit tests cover
the full governance lifecycle and the VM's rejection of invalid targets.

## Events

All re-exported from `types.rs`: `FeedRegisteredEvent`, `FeedUpdatedEvent`,
`FeedDisabledEvent`, `FeedEnabledEvent`, `FeedAutoDisabledEvent`,
`PriceReportedEvent`, `PricePulledEvent`, `ProviderPriceRescaledEvent`,
`DeviationRejectedEvent`, `FallbackActivatedEvent`,
`PriceCachedEvent`, `CacheConfigUpdatedEvent`, `PriceDecimalsUpdatedEvent`,
`AggregationParamsUpdatedEvent`, `AssetStrategyUpdatedEvent`,
`DefaultStrategyUpdatedEvent`, `FrozenEvent`, `UnfrozenEvent`,
`HealthFailureEvent`, `HealthSuccessEvent`, `BreakerOpenedEvent`,
`BreakerUnfrozenEvent`, `HeartbeatConfigUpdatedEvent`, `HeartbeatStaleEvent`,
`HeartbeatExpiredEvent`, `HeartbeatRecoveredEvent`,
`PriceHistoryConfigUpdatedEvent`, `PriceHistoryClearedEvent`,
`IncentiveConfigUpdatedEvent`, `RewardTokenSetEvent`, `RewardsFundedEvent`,
`RewardsClaimedEvent`, `RewardsWithdrawnEvent`, `UpgradeStagedEvent`,
`UpgradeExecutedEvent`.

## Security model

- All management paths require governance `require_auth`.
- `report_price` authorizes the *registered oracle address* for the slot.
- Pull providers are untrusted inputs: prices are validated (`> 0`),
  timestamps clamped, and decimals normalized before use.
- No source can steer the aggregate by itself: a demoted leader is removed and
  reported, and a read that cannot reach `min_sources` reverting rather than
  publishing a thin consensus.
- Aggregation is bounded by `MAX_FEEDS_PER_ASSET`, so the read cost cannot grow
  with governance configuration; the feed index keeps it proportional to the
  sources actually registered.
- The price cache is opt-in, clamped to source freshness, and invalidated by
  every governance mutation.
- Health failures only auto-freeze the affected asset, never the whole hub.
- A disabled or stale feed never participates in aggregation; a halt returns
  errors rather than a fabricated price.
- Heartbeat monitoring is opt-in and only ever fails an asset *closed*: it can
  withhold a price, never substitute a fabricated one, and pull sources are
  exempt from it entirely.
- Price history is opt-in and bounded by a fixed ring, so its cost cannot grow
  with configuration, and it never changes the price path — it only records what
  the price path already decided.
- The reward pool can only move the one token governance chose, only to the
  reporter that earned it, and never below what reporters are already owed.