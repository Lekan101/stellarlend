//! Price history tests: opt-in recording, what does and does not reach the
//! audit trail, bounded retention, ring wrap, range queries, and governance.

extern crate std;

use super::helpers::{
    allow_all, client, has_event, mk_asset, register_and_report, register_push_feed, report, setup,
};
use crate::types::{AggregationStrategy, FeedPriority};
use soroban_sdk::testutils::{Address as _, Ledger};
use soroban_sdk::{Address, Bytes};

/// Resolve `times` distinct prices, one per 100s ledger step, so each
/// resolution lands in its own ledger second.
fn resolve_over_time(
    te: &super::helpers::TestEnv,
    asset: &Bytes,
    prices: &[i128],
    oracle: &Address,
) {
    for (i, price) in prices.iter().enumerate() {
        te.env.ledger().set_timestamp(100 * (i as u64 + 1));
        report(te, asset, oracle, *price, &FeedPriority::Primary);
        client(te).get_price(asset);
    }
}

#[test]
fn test_history_is_off_by_default() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    register_and_report(&te, &asset, &[100]);
    allow_all(&te);

    client(&te).get_price(&asset);

    let config = client(&te).get_history_config(&asset);
    assert_eq!(config.max_entries, 0, "recording must be opt-in");
    assert_eq!(config.recorded, 0);
    assert_eq!(client(&te).get_price_history(&asset).len(), 0);

    // Disabling recording leaves the published price untouched.
    assert_eq!(client(&te).get_price(&asset).price, 100);
}

#[test]
fn test_resolution_is_recorded_with_its_audit_context() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    register_and_report(&te, &asset, &[100, 101, 102]);
    allow_all(&te);
    client(&te).set_history_limit(&None, &5);

    te.env.ledger().set_timestamp(500);
    let published = client(&te).get_price(&asset);

    let history = client(&te).get_price_history(&asset);
    assert_eq!(history.len(), 1);
    let entry = history.get(0).unwrap();
    assert_eq!(entry.asset, asset);
    assert_eq!(entry.timestamp, 500);
    assert_eq!(entry.price, published.price);
    assert_eq!(entry.confidence, published.confidence);
    assert_eq!(entry.num_feeds, 3);
    assert_eq!(entry.num_active_feeds, 3);
    assert_eq!(entry.strategy, AggregationStrategy::Median);
    assert!(!entry.used_fallback);
    assert_eq!(entry.deviation_bps, published.deviation_bps);
}

#[test]
fn test_cache_hits_do_not_append() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    register_and_report(&te, &asset, &[100]);
    allow_all(&te);
    client(&te).set_history_limit(&None, &10);
    client(&te).set_cache_ttl(&600);

    let first = client(&te).get_price(&asset);
    // A burst of reads served from the cache is one published price, not six.
    for _ in 0..5 {
        assert!(client(&te).get_price(&asset).from_cache);
    }
    let history = client(&te).get_price_history(&asset);
    assert_eq!(history.len(), 1);
    assert_eq!(history.get(0).unwrap().price, first.price);

    // A recompute in a later ledger second is a new observation.
    te.env.ledger().set_timestamp(60);
    client(&te).refresh_price(&asset);
    assert_eq!(client(&te).get_price_history(&asset).len(), 2);
}

#[test]
fn test_identical_resolutions_in_one_ledger_are_collapsed() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    allow_all(&te);
    client(&te).set_history_limit(&None, &10);
    client(&te).set_cache_ttl(&0);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);

    // Same price, same ledger second, same agreement: one fact, not three.
    client(&te).get_price(&asset);
    client(&te).refresh_price(&asset);
    client(&te).get_price(&asset);
    assert_eq!(client(&te).get_price_history(&asset).len(), 1);

    // A different price in the same second is a new observation.
    report(&te, &asset, &oracle, 101, &FeedPriority::Primary);
    client(&te).refresh_price(&asset);
    let history = client(&te).get_price_history(&asset);
    assert_eq!(history.len(), 2);
    assert_eq!(history.get(1).unwrap().price, 101);
}

#[test]
fn test_history_is_ordered_oldest_first() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    allow_all(&te);
    client(&te).set_history_limit(&None, &10);

    resolve_over_time(&te, &asset, &[100, 200, 300], &oracle);

    let history = client(&te).get_price_history(&asset);
    assert_eq!(history.len(), 3);
    assert_eq!(history.get(0).unwrap().price, 100);
    assert_eq!(history.get(0).unwrap().timestamp, 100);
    assert_eq!(history.get(1).unwrap().price, 200);
    assert_eq!(history.get(1).unwrap().timestamp, 200);
    assert_eq!(history.get(2).unwrap().price, 300);
    assert_eq!(history.get(2).unwrap().timestamp, 300);
}

#[test]
fn test_retention_bounds_the_history() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    allow_all(&te);
    client(&te).set_history_limit(&None, &3);

    resolve_over_time(&te, &asset, &[100, 200, 300, 400, 500], &oracle);

    let history = client(&te).get_price_history(&asset);
    assert_eq!(history.len(), 3, "retention is a hard bound");
    // The ring keeps the newest prices and forgets the oldest.
    assert_eq!(history.get(0).unwrap().price, 300);
    assert_eq!(history.get(1).unwrap().price, 400);
    assert_eq!(history.get(2).unwrap().price, 500);
    assert_eq!(client(&te).get_history_config(&asset).recorded, 3);
}

#[test]
fn test_range_query_selects_a_window() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    allow_all(&te);
    client(&te).set_history_limit(&None, &10);

    resolve_over_time(&te, &asset, &[100, 200, 300, 400, 500], &oracle);

    let window = client(&te).get_price_history_range(&asset, &200, &400);
    assert_eq!(window.len(), 3);
    assert_eq!(window.get(0).unwrap().price, 200);
    assert_eq!(window.get(2).unwrap().price, 400);

    // A window outside the recorded range is empty, not an error.
    assert_eq!(
        client(&te).get_price_history_range(&asset, &0, &50).len(),
        0
    );
    assert_eq!(
        client(&te)
            .get_price_history_range(&asset, &10_000, &20_000)
            .len(),
        0
    );
}

#[test]
fn test_asset_limit_overrides_the_default() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let other = mk_asset(&te.env, "USDC");
    let oracle = Address::generate(&te.env);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    let other_oracle = Address::generate(&te.env);
    register_push_feed(&te, &other, &other_oracle, &FeedPriority::Primary, 3600);
    allow_all(&te);

    client(&te).set_history_limit(&None, &2);
    client(&te).set_history_limit(&Some(asset.clone()), &4);
    assert!(has_event(&te, "history_config_updated", &asset));
    assert_eq!(client(&te).get_history_config(&asset).max_entries, 4);
    assert_eq!(client(&te).get_history_config(&other).max_entries, 2);

    resolve_over_time(&te, &asset, &[100, 200, 300], &oracle);
    assert_eq!(client(&te).get_price_history(&asset).len(), 3);
}

#[test]
fn test_disabling_history_drops_what_it_holds() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    allow_all(&te);
    client(&te).set_history_limit(&None, &10);
    resolve_over_time(&te, &asset, &[100, 200], &oracle);
    assert_eq!(client(&te).get_price_history(&asset).len(), 2);

    // eport re-arms only the reporter's auth, so governance is mocked again here.
    allow_all(&te);
    client(&te).set_history_limit(&Some(asset.clone()), &0);
    assert_eq!(client(&te).get_price_history(&asset).len(), 0);
    assert_eq!(client(&te).get_history_config(&asset).max_entries, 0);

    // And nothing is recorded while it stays off.
    resolve_over_time(&te, &asset, &[300], &oracle);
    assert_eq!(client(&te).get_price_history(&asset).len(), 0);
    // The price itself is unaffected.
    assert_eq!(client(&te).get_price(&asset).price, 300);
}

#[test]
fn test_lowering_a_limit_prunes_the_surplus() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    allow_all(&te);
    client(&te).set_history_limit(&None, &5);
    resolve_over_time(&te, &asset, &[100, 200, 300, 400], &oracle);
    assert_eq!(client(&te).get_price_history(&asset).len(), 4);

    // eport re-arms only the reporter's auth, so governance is mocked again here.
    allow_all(&te);
    client(&te).set_history_limit(&Some(asset.clone()), &2);
    let history = client(&te).get_price_history(&asset);
    assert_eq!(history.len(), 2);
    assert_eq!(history.get(0).unwrap().price, 300);
    assert_eq!(history.get(1).unwrap().price, 400);

    // Raising it again does not resurrect what was dropped.
    client(&te).set_history_limit(&Some(asset.clone()), &5);
    assert_eq!(client(&te).get_price_history(&asset).len(), 2);
}

#[test]
fn test_clear_price_history_is_governance_gated() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    allow_all(&te);
    client(&te).set_history_limit(&None, &5);
    resolve_over_time(&te, &asset, &[100, 200], &oracle);

    // eport re-arms only the reporter's auth, so governance is mocked again here.
    allow_all(&te);
    client(&te).clear_price_history(&asset);
    assert!(has_event(&te, "history_cleared", &asset));
    assert_eq!(client(&te).get_price_history(&asset).len(), 0);
}

#[test]
#[should_panic(expected = "HostError")]
fn test_clear_price_history_requires_governance() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    client(&te).clear_price_history(&asset);
}

#[test]
#[should_panic(expected = "HostError")]
fn test_set_history_limit_requires_governance() {
    let te = setup();
    client(&te).set_history_limit(&None, &10);
}

#[test]
#[should_panic(expected = "HostError")]
fn test_history_limit_above_the_ceiling_is_rejected() {
    let te = setup();
    allow_all(&te);
    client(&te).set_history_limit(&None, &1_000);
}

#[test]
#[should_panic(expected = "HostError")]
fn test_history_is_refused_while_frozen() {
    let te = setup();
    allow_all(&te);
    client(&te).freeze();
    client(&te).set_history_limit(&None, &10);
}

#[test]
fn test_history_is_per_asset() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let other = mk_asset(&te.env, "USDC");
    register_and_report(&te, &asset, &[100]);
    allow_all(&te);
    client(&te).set_history_limit(&None, &5);

    client(&te).get_price(&asset);

    assert_eq!(client(&te).get_price_history(&asset).len(), 1);
    assert_eq!(
        client(&te).get_price_history(&other).len(),
        0,
        "an untouched asset has no history"
    );
}

#[test]
fn test_a_solo_source_is_flagged_in_the_trail() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    allow_all(&te);
    client(&te).set_history_limit(&None, &5);

    te.env.ledger().set_timestamp(60);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);
    client(&te).get_price(&asset);

    let entry = client(&te).get_price_history(&asset).get(0).unwrap();
    assert_eq!(entry.num_feeds, 1);
    assert!(
        entry.used_fallback,
        "a single source is exactly what a consumer must be able to spot"
    );
}
