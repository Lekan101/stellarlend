//! Reporter incentive tests: who earns, who is paid, and what stops the pool
//! from being drained by a spinning oracle.

extern crate std;

use super::helpers::{
    allow_all, client, mk_asset, register_mock_provider, register_push_feed, report, setup, TestEnv,
};
use crate::incentives::{MAX_MIN_REWARD_INTERVAL_SECONDS, MAX_REWARD_PER_REPORT};
use crate::types::FeedPriority;
use soroban_sdk::testutils::{Address as _, Events, Ledger, MockAuth, MockAuthInvoke};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::xdr::{ContractEventBody, ScVal};
use soroban_sdk::{Address, Env, IntoVal, Symbol, TryFromVal, Val};

/// A reward of 0.0001 tokens, small but non-zero.
const RATE: i128 = 100;

// ── Harness ──────────────────────────────────────────────────────────────────

/// Deploy a real Stellar Asset Contract, so payouts move actual tokens.
fn new_token(te: &TestEnv) -> Address {
    let issuer = Address::generate(&te.env);
    te.env.register_stellar_asset_contract_v2(issuer).address()
}

fn sac<'a>(te: &'a TestEnv, token: &Address) -> StellarAssetClient<'a> {
    StellarAssetClient::new(&te.env, token)
}

fn mint(te: &TestEnv, token: &Address, to: &Address, amount: i128) {
    allow_all(te);
    sac(te, token).mint(to, &amount);
}

/// A hub with reporting live and a pool of `pool` base units behind it.
fn funded_programme(te: &TestEnv, rate: i128, pool: i128) -> Address {
    let reward_token = new_token(te);
    mint(te, &reward_token, &te.governance, pool);
    allow_all(te);
    client(te).set_reward_token(&reward_token);
    client(te).set_reward_per_report(&None, &rate);
    client(te).set_incentives_enabled(&true);
    allow_all(te);
    client(te).fund_rewards(&pool);
    reward_token
}

/// Authorize a claim by `oracle`.
fn authorize_claim(te: &TestEnv, oracle: &Address) {
    te.env.mock_auths(&[MockAuth {
        address: oracle,
        invoke: &MockAuthInvoke {
            contract: &te.contract_id,
            fn_name: "claim_rewards",
            args: (oracle,).into_val(&te.env),
            sub_invokes: &[],
        },
    }]);
}

/// Whether an event named `name` was published carrying `topic`.
fn has_event_topic(te: &TestEnv, name: &str, topic: &impl IntoVal<Env, Val>) -> bool {
    let name_val: Val = Symbol::new(&te.env, name).into_val(&te.env);
    let expected_name = ScVal::try_from_val(&te.env, &name_val).unwrap();
    let topic_val: Val = topic.into_val(&te.env);
    let expected_topic = ScVal::try_from_val(&te.env, &topic_val).unwrap();
    te.env.events().all().events().iter().any(|event| {
        let ContractEventBody::V0(body) = &event.body;
        body.topics.first() == Some(&expected_name) && body.topics.contains(&expected_topic)
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn test_the_programme_is_off_by_default() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);

    let rewards = client(&te).get_rewards(&oracle);
    assert_eq!(rewards.accrued, 0);
    assert_eq!(rewards.reports_rewarded, 0);
    assert_eq!(rewards.token, None);
    assert_eq!(rewards.pool_balance, 0);
    assert!(!client(&te).get_incentive_config(&None).enabled);
}

#[test]
fn test_a_token_and_a_rate_alone_do_not_start_paying() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    let reward_token = new_token(&te);
    allow_all(&te);
    client(&te).set_reward_token(&reward_token);
    client(&te).set_reward_per_report(&None, &RATE);

    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);

    // Everything is configured except the switch, and that is the point.
    assert_eq!(client(&te).get_rewards(&oracle).accrued, 0);
    assert_eq!(
        client(&te).get_incentive_config(&None).reward_per_report,
        RATE
    );
}

#[test]
fn test_an_accepted_report_earns_the_configured_reward() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    let pool = 10 * RATE;
    let reward_token = funded_programme(&te, RATE, pool);

    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);

    let rewards = client(&te).get_rewards(&oracle);
    assert_eq!(rewards.accrued, RATE);
    assert_eq!(rewards.reports_rewarded, 1);
    assert_eq!(rewards.token, Some(reward_token.clone()));
    // Earning is not paying: the tokens stay in the pool until claimed.
    assert_eq!(rewards.pool_balance, pool);
    assert_eq!(sac(&te, &reward_token).balance(&te.contract_id), pool);
}

#[test]
fn test_a_rejected_report_earns_nothing() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    funded_programme(&te, RATE, 10 * RATE);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        report(&te, &asset, &oracle, 0, &FeedPriority::Primary);
    }));
    assert!(result.is_err(), "a price of zero must be refused");
    assert_eq!(client(&te).get_rewards(&oracle).accrued, 0);
    assert_eq!(client(&te).get_rewards(&oracle).reports_rewarded, 0);
}

#[test]
fn test_a_pull_feed_earns_nothing() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let reward_token = funded_programme(&te, RATE, 10 * RATE);
    let provider = register_mock_provider(&te, &asset, &FeedPriority::Primary);
    super::helpers::MockProviderClient::new(&te.env, &provider).set_price(
        &asset,
        &100_000_000,
        &100,
    );

    // Pull sources are queried, not reporting, so they are not paid.
    client(&te).get_price(&asset);
    assert_eq!(client(&te).get_rewards(&provider).accrued, 0);
    assert_eq!(sac(&te, &reward_token).balance(&provider), 0);
}

#[test]
fn test_repeat_reports_inside_the_minimum_interval_earn_nothing() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    funded_programme(&te, RATE, 10 * RATE);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    allow_all(&te);
    client(&te).set_reward_min_interval(&60);

    let start = te.env.ledger().timestamp();
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);
    te.env.ledger().set_timestamp(start + 1);
    report(&te, &asset, &oracle, 101, &FeedPriority::Primary);
    te.env.ledger().set_timestamp(start + 59);
    report(&te, &asset, &oracle, 102, &FeedPriority::Primary);
    assert_eq!(client(&te).get_rewards(&oracle).accrued, RATE);
    assert_eq!(client(&te).get_rewards(&oracle).reports_rewarded, 1);

    // Once the window has passed, reporting pays again.
    te.env.ledger().set_timestamp(start + 60);
    report(&te, &asset, &oracle, 103, &FeedPriority::Primary);
    assert_eq!(client(&te).get_rewards(&oracle).accrued, 2 * RATE);
    assert_eq!(client(&te).get_rewards(&oracle).reports_rewarded, 2);
}

#[test]
fn test_each_asset_keeps_its_own_reward_clock() {
    let te = setup();
    let xlm = mk_asset(&te.env, "XLM");
    let usdc = mk_asset(&te.env, "USDC");
    let oracle = Address::generate(&te.env);
    funded_programme(&te, RATE, 10 * RATE);
    register_push_feed(&te, &xlm, &oracle, &FeedPriority::Primary, 3600);
    register_push_feed(&te, &usdc, &oracle, &FeedPriority::Primary, 3600);

    report(&te, &xlm, &oracle, 100, &FeedPriority::Primary);
    report(&te, &usdc, &oracle, 1, &FeedPriority::Primary);
    assert_eq!(client(&te).get_rewards(&oracle).accrued, 2 * RATE);
}

#[test]
fn test_a_per_asset_rate_overrides_the_default() {
    let te = setup();
    let xlm = mk_asset(&te.env, "XLM");
    let usdc = mk_asset(&te.env, "USDC");
    let xlm_oracle = Address::generate(&te.env);
    let usdc_oracle = Address::generate(&te.env);
    let reward_token = funded_programme(&te, RATE, 100 * RATE);
    allow_all(&te);
    client(&te).set_reward_per_report(&Some(xlm.clone()), &(7 * RATE));
    register_push_feed(&te, &xlm, &xlm_oracle, &FeedPriority::Primary, 3600);
    register_push_feed(&te, &usdc, &usdc_oracle, &FeedPriority::Primary, 3600);

    report(&te, &xlm, &xlm_oracle, 100, &FeedPriority::Primary);
    report(&te, &usdc, &usdc_oracle, 1, &FeedPriority::Primary);

    assert_eq!(client(&te).get_rewards(&xlm_oracle).accrued, 7 * RATE);
    assert_eq!(client(&te).get_rewards(&usdc_oracle).accrued, RATE);
    assert_eq!(
        client(&te)
            .get_incentive_config(&Some(xlm))
            .reward_per_report,
        7 * RATE
    );
    assert_eq!(
        client(&te)
            .get_incentive_config(&Some(usdc))
            .reward_per_report,
        RATE
    );
    // A per-asset rate pays nobody else from that asset.
    assert_eq!(sac(&te, &reward_token).balance(&te.contract_id), 100 * RATE);
}

#[test]
fn test_an_asset_paying_nothing_earns_nothing() {
    let te = setup();
    let other = mk_asset(&te.env, "USDC");
    let oracle = Address::generate(&te.env);
    funded_programme(&te, RATE, 10 * RATE);
    allow_all(&te);
    client(&te).set_reward_per_report(&Some(other.clone()), &0);
    register_push_feed(&te, &other, &oracle, &FeedPriority::Primary, 3600);

    report(&te, &other, &oracle, 1, &FeedPriority::Primary);
    assert_eq!(client(&te).get_rewards(&oracle).accrued, 0);
    assert_eq!(
        client(&te)
            .get_incentive_config(&Some(other))
            .reward_per_report,
        0
    );
}

#[test]
fn test_claiming_pays_the_reporter_and_empties_the_balance() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    let pool = 10 * RATE;
    let reward_token = funded_programme(&te, RATE, pool);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    allow_all(&te);
    client(&te).set_reward_min_interval(&0);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);
    report(&te, &asset, &oracle, 101, &FeedPriority::Primary);

    authorize_claim(&te, &oracle);
    client(&te).claim_rewards(&oracle);
    // Checked before any other call: the harness keeps only the events of the
    // most recent invocation.
    assert!(has_event_topic(&te, "rewards_claimed", &oracle));

    assert_eq!(sac(&te, &reward_token).balance(&oracle), 2 * RATE);
    assert_eq!(client(&te).get_rewards(&oracle).accrued, 0);
    assert_eq!(
        client(&te).get_rewards(&oracle).pool_balance,
        pool - 2 * RATE
    );
}

#[test]
fn test_claiming_twice_pays_only_once() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    let reward_token = funded_programme(&te, RATE, 10 * RATE);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);

    authorize_claim(&te, &oracle);
    client(&te).claim_rewards(&oracle);
    authorize_claim(&te, &oracle);
    client(&te).claim_rewards(&oracle);

    assert_eq!(sac(&te, &reward_token).balance(&oracle), RATE);
    assert_eq!(sac(&te, &reward_token).balance(&te.contract_id), 9 * RATE);
}

#[test]
fn test_claiming_with_nothing_owed_is_a_no_op() {
    let te = setup();
    let oracle = Address::generate(&te.env);
    let reward_token = funded_programme(&te, RATE, 10 * RATE);

    authorize_claim(&te, &oracle);
    client(&te).claim_rewards(&oracle);

    assert_eq!(sac(&te, &reward_token).balance(&oracle), 0);
    assert_eq!(client(&te).get_rewards(&oracle).pool_balance, 10 * RATE);
}

#[test]
fn test_a_claim_the_pool_cannot_cover_reverts() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    let reward_token = new_token(&te);
    allow_all(&te);
    client(&te).set_reward_token(&reward_token);
    client(&te).set_reward_per_report(&None, &RATE);
    client(&te).set_incentives_enabled(&true);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);

    // Earning created a liability governance has not funded yet.
    assert_eq!(client(&te).get_rewards(&oracle).accrued, RATE);
    authorize_claim(&te, &oracle);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client(&te).claim_rewards(&oracle);
    }));
    assert!(result.is_err(), "an unfunded claim must not pay out");
    // The earnings survive the failed attempt.
    assert_eq!(client(&te).get_rewards(&oracle).accrued, RATE);
}

#[test]
fn test_funding_makes_a_pending_claim_succeed() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    let reward_token = new_token(&te);
    allow_all(&te);
    client(&te).set_reward_token(&reward_token);
    client(&te).set_reward_per_report(&None, &RATE);
    client(&te).set_incentives_enabled(&true);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);

    mint(&te, &reward_token, &te.governance, 5 * RATE);
    allow_all(&te);
    client(&te).fund_rewards(&(5 * RATE));
    assert!(has_event_topic(&te, "rewards_funded", &te.governance));
    assert_eq!(client(&te).get_rewards(&oracle).total_owed, RATE);

    authorize_claim(&te, &oracle);
    client(&te).claim_rewards(&oracle);
    assert_eq!(sac(&te, &reward_token).balance(&oracle), RATE);
    assert_eq!(sac(&te, &reward_token).balance(&te.contract_id), 4 * RATE);
}

#[test]
#[should_panic(expected = "HostError")]
fn test_claiming_requires_the_reporters_own_authorization() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    funded_programme(&te, RATE, 10 * RATE);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);

    client(&te).claim_rewards(&oracle);
}

#[test]
#[should_panic(expected = "HostError")]
fn test_a_reporter_cannot_claim_another_reporters_rewards() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    let thief = Address::generate(&te.env);
    funded_programme(&te, RATE, 10 * RATE);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);

    te.env.mock_auths(&[MockAuth {
        address: &thief,
        invoke: &MockAuthInvoke {
            contract: &te.contract_id,
            fn_name: "claim_rewards",
            args: (&oracle,).into_val(&te.env),
            sub_invokes: &[],
        },
    }]);
    client(&te).claim_rewards(&oracle);
}

#[test]
#[should_panic(expected = "HostError")]
fn test_funding_requires_governance() {
    let te = setup();
    let reward_token = new_token(&te);
    allow_all(&te);
    client(&te).set_reward_token(&reward_token);
    client(&te).set_reward_per_report(&None, &RATE);
    client(&te).set_incentives_enabled(&true);
    mint(&te, &reward_token, &te.governance, 10 * RATE);
    // `mint` leaves every authorization mocked; drop them so funding is
    // attempted on its own merits.
    te.env.mock_auths(&[]);

    client(&te).fund_rewards(&(10 * RATE));
}

#[test]
#[should_panic(expected = "HostError")]
fn test_choosing_the_reward_token_requires_governance() {
    let te = setup();
    let reward_token = new_token(&te);

    client(&te).set_reward_token(&reward_token);
}

#[test]
fn test_setting_a_reward_rate_requires_governance() {
    let te = setup();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client(&te).set_reward_per_report(&None, &RATE);
    }));
    assert!(result.is_err(), "only governance may set a reward rate");
}

#[test]
fn test_governance_can_recover_unspent_rewards() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    let pool = 10 * RATE;
    let reward_token = funded_programme(&te, RATE, pool);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);
    allow_all(&te);

    let treasury = Address::generate(&te.env);
    client(&te).withdraw_rewards(&treasury, &(pool - RATE));
    assert!(has_event_topic(&te, "rewards_withdrawn", &treasury));

    assert_eq!(sac(&te, &reward_token).balance(&treasury), pool - RATE);
    assert_eq!(sac(&te, &reward_token).balance(&te.contract_id), RATE);
}

#[test]
fn test_withdrawing_cannot_dip_into_reported_earnings() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    let reward_token = funded_programme(&te, RATE, 5 * RATE);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);
    allow_all(&te);

    let treasury = Address::generate(&te.env);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client(&te).withdraw_rewards(&treasury, &(5 * RATE));
    }));
    assert!(
        result.is_err(),
        "unearned pool must be recoverable, owed pool must not"
    );
    assert_eq!(sac(&te, &reward_token).balance(&te.contract_id), 5 * RATE);
}

#[test]
#[should_panic(expected = "HostError")]
fn test_the_reward_token_is_settable_once() {
    let te = setup();
    let first = new_token(&te);
    let second = new_token(&te);
    allow_all(&te);
    client(&te).set_reward_token(&first);

    client(&te).set_reward_token(&second);
}

#[test]
fn test_a_rate_above_the_ceiling_is_refused() {
    let te = setup();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client(&te).set_reward_per_report(&None, &(MAX_REWARD_PER_REPORT + 1));
    }));
    assert!(result.is_err(), "a runaway rate must be refused");
    assert_eq!(client(&te).get_incentive_config(&None).reward_per_report, 0);
}

#[test]
fn test_a_negative_rate_is_refused() {
    let te = setup();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client(&te).set_reward_per_report(&None, &(-1));
    }));
    assert!(result.is_err(), "rewards may not be negative");
}

#[test]
fn test_a_minimum_interval_beyond_a_day_is_refused() {
    let te = setup();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client(&te).set_reward_min_interval(&(MAX_MIN_REWARD_INTERVAL_SECONDS + 1));
    }));
    assert!(result.is_err());
    assert_eq!(
        client(&te).get_incentive_config(&None).min_interval_seconds,
        crate::incentives::DEFAULT_MIN_REWARD_INTERVAL_SECONDS
    );
}

#[test]
fn test_turning_the_programme_off_stops_paying_but_keeps_earnings() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    let reward_token = funded_programme(&te, RATE, 10 * RATE);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    allow_all(&te);
    client(&te).set_reward_min_interval(&0);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);

    // `report` re-arms only the reporter's authorization.
    allow_all(&te);
    client(&te).set_incentives_enabled(&false);
    te.env.ledger().with_mut(|li| li.timestamp += 1);
    report(&te, &asset, &oracle, 101, &FeedPriority::Primary);
    assert_eq!(client(&te).get_rewards(&oracle).accrued, RATE);

    // What was earned before the switch is still owed.
    authorize_claim(&te, &oracle);
    client(&te).claim_rewards(&oracle);
    assert_eq!(sac(&te, &reward_token).balance(&oracle), RATE);
}

#[test]
fn test_configuration_changes_are_announced() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let reward_token = new_token(&te);
    allow_all(&te);
    client(&te).set_reward_token(&reward_token);
    client(&te).set_reward_per_report(&None, &RATE);
    client(&te).set_incentives_enabled(&true);
    client(&te).set_reward_min_interval(&30);
    // The last change made is the one whose event is still readable, since the
    // harness only keeps the events of the most recent invocation.
    client(&te).set_reward_per_report(&Some(asset.clone()), &(2 * RATE));

    assert!(has_event_topic(
        &te,
        "incentives_config_updated",
        &asset.clone()
    ));
}

#[test]
fn test_reports_keep_working_before_a_reward_token_is_chosen() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    allow_all(&te);
    client(&te).set_reward_per_report(&None, &RATE);
    client(&te).set_incentives_enabled(&true);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);

    // No token, no credits, and no stuck reports: the price path is untouched.
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);
    assert_eq!(client(&te).get_rewards(&oracle).accrued, 0);
    assert_eq!(client(&te).get_rewards(&oracle).token, None);
    assert_eq!(client(&te).get_price(&asset).price, 100);
}

#[test]
fn test_a_frozen_hub_pays_nobody() {
    let te = setup();
    let asset = mk_asset(&te.env, "XLM");
    let oracle = Address::generate(&te.env);
    let reward_token = funded_programme(&te, RATE, 10 * RATE);
    register_push_feed(&te, &asset, &oracle, &FeedPriority::Primary, 3600);
    report(&te, &asset, &oracle, 100, &FeedPriority::Primary);
    allow_all(&te);
    client(&te).freeze();

    // A frozen hub takes no reports and no funds.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        report(&te, &asset, &oracle, 101, &FeedPriority::Primary);
    }));
    assert!(result.is_err());
    authorize_claim(&te, &oracle);
    let claim = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client(&te).claim_rewards(&oracle);
    }));
    assert!(claim.is_err());
    // Unfreezing leaves the earned reward intact.
    allow_all(&te);
    client(&te).unfreeze();
    assert_eq!(client(&te).get_rewards(&oracle).accrued, RATE);
    assert_eq!(sac(&te, &reward_token).balance(&te.contract_id), 10 * RATE);
}
