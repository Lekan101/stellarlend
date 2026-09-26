//! Oracle heartbeat monitoring tests: cadence classification, staleness
//! detection, expiry failing an asset closed, breaker integration, recovery,
//! pull-feed exemption, and sweep idempotence.

extern crate std;

use super::helpers::{
    allow_all, client, has_event, mk_asset, register_mock_provider, register_push_feed, report,
    setup, slot, MockProviderClient, TestEnv,
};
use crate::types::{FeedMode, FeedPriority, HeartbeatConfig, HeartbeatStatus};
use soroban_sdk::testutils::{Address as _, Ledger};
use soroban_sdk::{Address, Bytes};

/// A monitored asset: cadence 100s, stale after 300s, expiry 600s.
fn monitored_config() -> HeartbeatConfig {
    HeartbeatConfig {
        enabled: true,
        interval_seconds: 100,
        stale_after_seconds: 300,
        expiry_seconds: 600,
    }
}

/// Enable monitoring for the asset and register one push feed that reports.
fn monitored_push(te: &TestEnv, asset: &Bytes, price: i128) -> Address {
    let oracle = Address::generate(&te.env);
    register_push_feed(te, asset, &oracle, &FeedPriority::Primary, 3600);
    allow_all(te);
    client(te).set_heartbeat_config(&Some(asset.clone()), &monitored_config());
    report(te, asset, &oracle, price, &FeedPriority::Primary);
    oracle
}

/// A pull feed that is configured but not yet quoting.
fn monitored_pull(te: &TestEnv, asset: &Bytes) -> Address {
    let provider = register_mock_provider(te, asset, &FeedPriority::Primary);
    allow_all(te);
    client(te).set_heartbeat_config(&Some(asset.clone()), &monitored_config());
    provider
}

/// Point the mock provider at a fresh quote for the current ledger time.
fn quote(te: &TestEnv, provider: &Address, asset: &Bytes, price: i128) {
    MockProviderClient::new(&te.env, provider).set_price(asset, &price, &95u32);
}

/// Read the status of the primary slot after a sweep.
fn primary_status(te: &TestEnv, asset: &Bytes) -> crate::types::HeartbeatSlotStatus {
    client(te).get_heartbeat(asset, &FeedPriority::Primary)
}

#[test]
fn test_heartbeat_monitoring_is_off_by_default() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);

    let config = client(&te).get_heartbeat_config(&Some(asset.clone()));
    assert!(!config.enabled, "monitoring must be opt-in");

    // A slot that never reported is not degraded while monitoring is off.
    te.env.ledger().set_timestamp(100_000);
    let status = primary_status(&te, &asset);
    assert_eq!(status.status, HeartbeatStatus::Fresh);
    assert_eq!(status.last_seen, 0);
}

#[test]
fn test_report_counts_as_a_heartbeat() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = monitored_push(&te, &asset, 100_000_000);

    te.env.ledger().set_timestamp(90);
    let status = primary_status(&te, &asset);
    assert_eq!(status.status, HeartbeatStatus::Fresh);
    assert_eq!(status.last_seen, 0, "reported at ledger time 0");

    te.env.ledger().set_timestamp(100);
    report(&te, &asset, &oracle, 101_000_000, &FeedPriority::Primary);
    let status = primary_status(&te, &asset);
    assert_eq!(status.status, HeartbeatStatus::Fresh);
    assert_eq!(status.last_seen, 100);
    assert_eq!(status.age_seconds, 0);
}

#[test]
fn test_heartbeat_walks_fresh_due_stale_expired() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let _oracle = monitored_push(&te, &asset, 100_000_000);

    // Past the cadence but inside the stale window: due, not yet untrustworthy.
    te.env.ledger().set_timestamp(150);
    assert_eq!(primary_status(&te, &asset).status, HeartbeatStatus::Due);

    // Inside the stale window: still priced, but flagged.
    te.env.ledger().set_timestamp(299);
    assert_eq!(primary_status(&te, &asset).status, HeartbeatStatus::Due);

    te.env.ledger().set_timestamp(300);
    let status = primary_status(&te, &asset);
    assert_eq!(status.status, HeartbeatStatus::Stale);
    assert_eq!(status.age_seconds, 300);
    assert_eq!(status.missed_beats, 3);

    // Past expiry: the hub stops trusting the source.
    te.env.ledger().set_timestamp(600);
    let status = primary_status(&te, &asset);
    assert_eq!(status.status, HeartbeatStatus::Expired);
    assert_eq!(status.age_seconds, 600);
}

#[test]
fn test_never_reported_slot_is_silent_then_expires() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let _oracle = Address::generate(&te.env);
    register_push_feed(&te, &asset, &_oracle, &FeedPriority::Primary, 3600);
    allow_all(&te);
    client(&te).set_heartbeat_config(&Some(asset.clone()), &monitored_config());

    let status = primary_status(&te, &asset);
    assert_eq!(status.status, HeartbeatStatus::Silent);
    assert_eq!(status.last_seen, 0);

    // A slot that never spoke has no last report, so its silence is measured
    // from the moment monitoring started asking.
    te.env.ledger().set_timestamp(600);
    assert_eq!(primary_status(&te, &asset).status, HeartbeatStatus::Expired);
}

#[test]
fn test_sweep_publishes_only_transitions_and_trips_the_breaker() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let _oracle = monitored_push(&te, &asset, 100_000_000);
    allow_all(&te);

    te.env.ledger().set_timestamp(300);
    let swept = client(&te).sweep_heartbeats(&asset);
    assert_eq!(swept.len(), 1);
    assert_eq!(swept.get(0).unwrap().status, HeartbeatStatus::Stale);
    assert_eq!(swept.get(0).unwrap().missed_beats, 3);
    assert!(has_event(&te, "heartbeat_stale_event", &asset));
    assert!(!has_event(&te, "heartbeat_expired_event", &asset));
    // A late source is still a working source: only expiry trips the asset.
    assert!(!client(&te).is_asset_frozen(&asset));

    // Sweeping again at the same time changes nothing and logs nothing new.
    let again = client(&te).sweep_heartbeats(&asset);
    assert_eq!(again.get(0).unwrap().status, HeartbeatStatus::Stale);
    assert!(!has_event(&te, "heartbeat_stale_event", &asset));
    assert_eq!(client(&te).get_health(&asset).consecutive_failures, 0);

    te.env.ledger().set_timestamp(600);
    let swept = client(&te).sweep_heartbeats(&asset);
    assert_eq!(swept.get(0).unwrap().status, HeartbeatStatus::Expired);
    assert!(has_event(&te, "heartbeat_expired_event", &asset));
    // A dead oracle is reported even when nobody is asking for a price.
    assert!(client(&te).is_asset_frozen(&asset));
    assert!(client(&te).get_health(&asset).circuit_breaker_open);
}

#[test]
#[should_panic(expected = "HostError")]
fn test_expired_heartbeat_fails_the_asset_closed() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let _oracle = monitored_push(&te, &asset, 100_000_000);
    allow_all(&te);

    // A price is still served while the source is only late.
    te.env.ledger().set_timestamp(300);
    let price = client(&te).get_price(&asset);
    assert_eq!(price.price, 100_000_000);

    // Past expiry the hub refuses rather than serve a number nobody updates.
    te.env.ledger().set_timestamp(5_000);
    client(&te).get_price(&asset);
}

#[test]
fn test_report_recovers_an_expired_slot() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = monitored_push(&te, &asset, 100_000_000);
    allow_all(&te);

    te.env.ledger().set_timestamp(5_000);
    client(&te).sweep_heartbeats(&asset);
    assert!(has_event(&te, "heartbeat_expired_event", &asset));
    assert!(client(&te).is_asset_frozen(&asset));

    // The oracle comes back: the slot itself is healthy again.
    te.env.ledger().set_timestamp(5_100);
    report(&te, &asset, &oracle, 100_000_000, &FeedPriority::Primary);
    assert!(has_event(&te, "heartbeat_recovered_event", &asset));
    let status = primary_status(&te, &asset);
    assert_eq!(status.status, HeartbeatStatus::Fresh);
    assert_eq!(status.last_seen, 5_100);

    // The asset stays closed for the breaker's cooldown, as with any other
    // automatically opened breaker, and the next successful read clears it.
    assert!(client(&te).is_asset_frozen(&asset));
    te.env.ledger().set_timestamp(5_650);
    assert_eq!(client(&te).get_price(&asset).price, 100_000_000);
    assert!(!client(&te).is_asset_frozen(&asset));
}

#[test]
fn test_healthy_reports_do_not_flood_the_log() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = monitored_push(&te, &asset, 100_000_000);
    allow_all(&te);

    for i in 1..=5u64 {
        te.env.ledger().set_timestamp(50 * i);
        report(&te, &asset, &oracle, 100_000_000, &FeedPriority::Primary);
        client(&te).sweep_heartbeats(&asset);
    }

    // Nothing degraded, so nothing to recover: the beat at t=50 is the only
    // transition the log ever sees.
    let status = primary_status(&te, &asset);
    assert_eq!(status.status, HeartbeatStatus::Fresh);
    assert!(!has_event(&te, "heartbeat_stale_event", &asset));
    assert!(!has_event(&te, "heartbeat_expired_event", &asset));
}

#[test]
fn test_pull_feeds_are_exempt_from_expiry() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    allow_all(&te);
    let _provider = monitored_pull(&te, &asset);
    client(&te).set_heartbeat_config(&None, &monitored_config());

    // A pull source has no reporting cadence: it answers on demand, so its
    // silence must never expire an asset that is serving every call.
    te.env.ledger().set_timestamp(1_000_000);
    let swept = client(&te).sweep_heartbeats(&asset);
    assert_eq!(swept.get(0).unwrap().status, HeartbeatStatus::Fresh);
    assert!(!client(&te).is_asset_frozen(&asset));
    // A pull slot has no deadline to schedule against.
    assert_eq!(
        client(&te).heartbeat_expiry(&asset, &FeedPriority::Primary),
        0
    );

    // And the live pull is what a keeper actually observes.
    let feed = client(&te).get_feed(&asset, &FeedPriority::Primary);
    assert_eq!(feed.unwrap().mode, FeedMode::Pull);
}

#[test]
fn test_pull_counts_as_a_heartbeat_when_it_answers() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    allow_all(&te);
    let provider = monitored_pull(&te, &asset);

    te.env.ledger().set_timestamp(4_200);
    quote(&te, &provider, &asset, 100_000_000);
    assert_eq!(client(&te).get_price(&asset).price, 100_000_000);

    let status = primary_status(&te, &asset);
    assert_eq!(status.last_seen, 4_200, "a live pull is a beat");
    assert_eq!(status.status, HeartbeatStatus::Fresh);
}

#[test]
fn test_asset_config_overrides_the_hub_default() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let other = mk_asset(&te.env, "USDC");
    allow_all(&te);

    client(&te).set_heartbeat_config(&None, &monitored_config());
    let strict = HeartbeatConfig {
        interval_seconds: 10,
        stale_after_seconds: 20,
        expiry_seconds: 30,
        ..monitored_config()
    };
    client(&te).set_heartbeat_config(&Some(other.clone()), &strict);
    assert!(has_event(&te, "heartbeat_config_updated_event", &other));

    assert_eq!(
        client(&te).get_heartbeat_config(&Some(asset.clone())),
        monitored_config()
    );
    assert_eq!(
        client(&te).get_heartbeat_config(&Some(other.clone())),
        strict
    );
    assert_eq!(client(&te).get_heartbeat_config(&None), monitored_config());
}

#[test]
#[should_panic(expected = "HostError")]
fn test_invalid_heartbeat_config_is_rejected() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    allow_all(&te);

    // Expiry must come strictly after staleness.
    client(&te).set_heartbeat_config(
        &Some(asset.clone()),
        &HeartbeatConfig {
            enabled: true,
            interval_seconds: 100,
            stale_after_seconds: 300,
            expiry_seconds: 300,
        },
    );
}

#[test]
#[should_panic(expected = "HostError")]
fn test_set_heartbeat_config_requires_governance() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    // No governance auth mocked.
    client(&te).set_heartbeat_config(&Some(asset.clone()), &monitored_config());
}

#[test]
fn test_get_heartbeats_covers_every_registered_slot() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    allow_all(&te);
    client(&te).set_heartbeat_config(&None, &monitored_config());
    let _oracles = super::helpers::register_and_report(&te, &asset, &[100, 101, 102]);

    let statuses = client(&te).get_heartbeats(&asset);
    assert_eq!(statuses.len(), 3);
    for i in 0..3 {
        assert_eq!(statuses.get(i).unwrap().priority, slot(i as usize) as u32);
        assert_eq!(statuses.get(i).unwrap().status, HeartbeatStatus::Fresh);
    }
}

#[test]
fn test_heartbeat_expiry_deadline_is_published() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = monitored_push(&te, &asset, 100_000_000);
    allow_all(&te);

    te.env.ledger().set_timestamp(200);
    report(&te, &asset, &oracle, 100_000_000, &FeedPriority::Primary);
    // Last report at 200, expiry window is 600 seconds.
    assert_eq!(
        client(&te).heartbeat_expiry(&asset, &FeedPriority::Primary),
        800
    );
}

#[test]
fn test_monitoring_never_breaks_a_second_healthy_source() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    allow_all(&te);
    client(&te).set_heartbeat_config(&None, &monitored_config());

    let dead = Address::generate(&te.env);
    let live = Address::generate(&te.env);
    register_push_feed(&te, &asset, &dead, &FeedPriority::Primary, 3600);
    register_push_feed(&te, &asset, &live, &FeedPriority::Secondary, 3600);
    report(&te, &asset, &live, 100_000_000, &FeedPriority::Secondary);

    // The live source keeps its cadence; the dead one has gone quiet.
    te.env.ledger().set_timestamp(400);
    report(&te, &asset, &live, 100_000_000, &FeedPriority::Secondary);
    let swept = client(&te).sweep_heartbeats(&asset);
    let primary = swept.get(0).unwrap();
    let secondary = swept.get(1).unwrap();
    assert_eq!(primary.priority, 0);
    assert_eq!(primary.status, HeartbeatStatus::Stale);
    assert_eq!(secondary.priority, 1);
    assert_eq!(secondary.status, HeartbeatStatus::Fresh);

    // A dead source is demoted by the existing staleness path rather than
    // taking the healthy one down with it.
    let price = client(&te).get_price(&asset);
    assert_eq!(price.price, 100_000_000);
    assert_eq!(price.num_active_feeds, 1);
}
