# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog, and this project adheres to Semantic Versioning.

---

## [Unreleased]

### Added
- Heartbeat monitoring in the Oracle Hub: opt-in per-asset reporting-cadence expectations (`interval_seconds`, `stale_after_seconds`, `expiry_seconds`), a permissionless idempotent sweep, and a fail-closed expiry that withholds a price nobody is updating instead of serving a stale one (#1034)
- Bounded price-history retention in the Oracle Hub: an opt-in fixed ring of resolved prices per asset, recorded on fresh resolutions only, queryable by range and recorded with the inputs that produced each decision (#1041)
- Governance-funded reporter incentives in the Oracle Hub: a token reward pool that pays oracle addresses for accepted reports, with per-asset rates, a per-oracle minimum interval, a rate ceiling, reporter-authorized claims, and a withdrawal floor that cannot touch earnings already owed (#1042)
- Market-wide emergency pause in the lending contract, with a guardian/admin-controlled lifecycle, operation-specific gates, and a reversible pause state that survives protocol calls already in flight (#1031)
- Opt-in TTL price cache in the Oracle Hub with freshness clamping, epoch invalidation on governance changes, and read-cost regression tests (#1037)
- Per-asset aggregation parameters (deviation band, source floor) and an explicit switch to disable the deviation check in the Oracle Hub (#1035)
- Per-asset feed index so the price read cost stays proportional to the sources actually registered (#1037)
- Health, ring-buffer, and event-trail views on the TWAP oracle, plus a documented `force_record_price` escape hatch for genuine repricings (#1033)
- Anomaly detection for unusual transaction patterns (median/MAD liquidation outliers, velocity bursts, amount spikes, dusting) in the analytics pipeline
- Multi-signature requirement for protocol upgrades (Oracle Hub upgrade gate with approver threshold and 48 h timelock)
- Timelock enforcement for governance parameter changes (per-parameter minimums plus maximum cap and view helpers in parameter-store)
- Local Soroban devnet setup with Docker Compose (node, Horizon, Friendbot, seeded accounts/contracts)
- Multi-stage Dockerization for API and Oracle services
- OpenAPI/Swagger documentation for API
- Price staleness detection and alerting in oracle service
- Retry logic with exponential backoff for transaction submission
- Monorepo root package configuration
- Circuit breaker implementation for system stability
- LRU batch cache eviction mechanism
- E2E integration tests for Oracle–Contract–API pipeline
- Multi-asset collateral support
- WebSocket endpoint for real-time price updates
- Health factor and position query entrypoints
- Protocol fee collection and treasury management
- Stellar address validation and enhanced middleware support
- Protocol governance system
- Oracle contract updater

### Changed
- TWAP oracle now computes a true time-weighted average (price × elapsed seconds) instead of a mean over sample counts, and rejects manipulated observations on ingestion instead of blending them into the average (#1033)
- Oracle Hub aggregation now supports five feed slots per asset with median, weighted, and trimmed-mean strategies (#1036)
- Oracle Hub now demotes a leading source that deviates from the rest beyond the configured band, with a quorum rule, a source floor, and on-chain evidence (#1035)
- Oracle Hub pull quotes are normalized to a hub-wide canonical precision before aggregation (#1036)
- Refactored duplicated validation logic using factory functions
- Switched transaction polling to fixed interval strategy
- Improved project structure and test organization
- Expanded full lifecycle integration test coverage
- Enhanced oracle performance and rate-limit handling
- Updated CI pipeline to enforce blocking contract checks
- Added contributing documentation

### Fixed
- Repaid, withdrawn, and liquidated positions stay open while a market is paused, so the pause stops new risk instead of trapping users in it (#1031)
- Range-check overflow lint in the isolated-market validation path
- Borrow interest overflow error
- Integration test placeholders and inconsistencies
- Default debt asset initialization for borrow/repay
- Flash loan reentrancy locks and VM execution aborts
- Masked admin secret key in oracle logs and outputs
- Enforced HTTPS and HSTS security headers
- Strengthened JWT secret validation and removed insecure defaults
- Validated CONTRACT_ID environment variable on API startup
- Fixed Rust CI issues (formatting, clippy, tests)

---

## 📌 Notes

- The project is under active development and changes are tracked under the Unreleased section.
- A formal version release will be added once versioning is standardized across the project.
