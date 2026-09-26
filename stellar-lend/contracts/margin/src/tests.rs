#![cfg(test)]

extern crate alloc;

use alloc::string::ToString as _;

use crate::account::{CollateralAssetConfig, MarginAccount, MarginCallLevel, MarginMode};
use crate::cross::{
    borrow_cross_margin, calculate_multi_collateral_summary, deposit_cross_collateral,
    liquidate_cross_margin_multi_collateral, withdraw_cross_collateral,
};
use crate::emergency::{EmergencyOp, EmergencyState, MarketRegistry, OpPause, PriceContext};
use crate::isolated::{
    borrow_isolated_market, deposit_isolated_market, liquidate_isolated_market_position,
    repay_isolated_market, withdraw_isolated_market, IsolatedMarket, IsolatedUserPosition,
};
use soroban_sdk::testutils::{Address as _, ContractFunctionSet, Events as _};
use soroban_sdk::xdr::{ContractEventBody, ScVal};
use soroban_sdk::{Address, Env, Map, TryFromVal, Val, Vec};

#[test]
fn test_multi_collateral_cross_margin_summary_and_borrowing() {
    let env = Env::default();
    let user = Address::generate(&env);
    let usdc = Address::generate(&env);
    let xlm = Address::generate(&env);
    let btc = Address::generate(&env);

    let mut configs = Map::new(&env);
    // USDC: $1.00, 80% LTV, 85% liq threshold, 7 decimals
    configs.set(
        usdc.clone(),
        CollateralAssetConfig {
            asset: usdc.clone(),
            collateral_factor: 8000,
            liquidation_threshold: 8500,
            price: 10_000_000,
            decimals: 7,
        },
    );
    // XLM: $0.50, 60% LTV, 70% liq threshold, 7 decimals
    configs.set(
        xlm.clone(),
        CollateralAssetConfig {
            asset: xlm.clone(),
            collateral_factor: 6000,
            liquidation_threshold: 7000,
            price: 5_000_000,
            decimals: 7,
        },
    );
    // BTC: $50,000, 70% LTV, 75% liq threshold, 7 decimals
    configs.set(
        btc.clone(),
        CollateralAssetConfig {
            asset: btc.clone(),
            collateral_factor: 7000,
            liquidation_threshold: 7500,
            price: 500_000_000_000,
            decimals: 7,
        },
    );

    let mut account = MarginAccount {
        owner: user.clone(),
        mode: MarginMode::Cross,
        positions: Vec::new(&env),
        total_collateral_value: 0,
        total_debt_value: 0,
    };

    // Deposit 1000 USDC ($1,000)
    assert!(deposit_cross_collateral(
        &env,
        &mut account,
        usdc.clone(),
        1000 * 10_000_000,
        &configs
    )
    .is_ok());
    // Deposit 2000 XLM ($1,000)
    assert!(
        deposit_cross_collateral(&env, &mut account, xlm.clone(), 2000 * 10_000_000, &configs)
            .is_ok()
    );

    // Summary calculation
    let summary = calculate_multi_collateral_summary(&env, &account, &configs);
    // Total collateral: $1,000 + $1,000 = $2,000
    assert_eq!(summary.total_collateral_value, 2000 * 10_000_000);
    // Borrow power: ($1000 * 0.8) + ($1000 * 0.6) = $800 + $600 = $1,400
    assert_eq!(summary.weighted_borrow_power, 1400 * 10_000_000);
    // Liquidation collateral: ($1000 * 0.85) + ($1000 * 0.70) = $850 + $700 = $1,550
    assert_eq!(summary.liquidation_collateral_value, 1550 * 10_000_000);
    assert_eq!(summary.margin_call_level, MarginCallLevel::Safe);
    assert!(!summary.is_liquidatable);

    // Borrow 1200 USDC ($1,200 <= $1,400 max borrow power) -> Succeeds
    assert!(borrow_cross_margin(
        &env,
        &mut account,
        usdc.clone(),
        1200 * 10_000_000,
        &configs
    )
    .is_ok());

    // Borrow additional 300 USDC (total debt $1,500 > $1,400 borrow power) -> Rejected
    assert!(
        borrow_cross_margin(&env, &mut account, usdc.clone(), 300 * 10_000_000, &configs).is_err()
    );

    // Withdrawing XLM while debt is outstanding:
    // Withdrawing 1500 XLM would drop collateral to ($1000 USDC + $250 XLM) = $1250 < $1200 debt
    assert!(withdraw_cross_collateral(
        &env,
        &mut account,
        xlm.clone(),
        1500 * 10_000_000,
        &configs
    )
    .is_err());

    // Small withdrawal of 200 XLM ($100) -> Allowed
    assert!(
        withdraw_cross_collateral(&env, &mut account, xlm.clone(), 200 * 10_000_000, &configs)
            .is_ok()
    );
}

#[test]
fn test_multi_collateral_cross_margin_liquidation() {
    let env = Env::default();
    let user = Address::generate(&env);
    let usdc = Address::generate(&env);
    let xlm = Address::generate(&env);

    let mut configs = Map::new(&env);
    configs.set(
        usdc.clone(),
        CollateralAssetConfig {
            asset: usdc.clone(),
            collateral_factor: 8000,
            liquidation_threshold: 8500,
            price: 10_000_000,
            decimals: 7,
        },
    );
    // XLM starts at $1.00
    configs.set(
        xlm.clone(),
        CollateralAssetConfig {
            asset: xlm.clone(),
            collateral_factor: 6000,
            liquidation_threshold: 7000,
            price: 10_000_000,
            decimals: 7,
        },
    );

    let mut account = MarginAccount {
        owner: user.clone(),
        mode: MarginMode::Cross,
        positions: Vec::new(&env),
        total_collateral_value: 0,
        total_debt_value: 0,
    };

    deposit_cross_collateral(&env, &mut account, xlm.clone(), 1000 * 10_000_000, &configs).unwrap();
    borrow_cross_margin(&env, &mut account, usdc.clone(), 500 * 10_000_000, &configs).unwrap();

    // Now XLM price crashes from $1.00 to $0.40
    configs.set(
        xlm.clone(),
        CollateralAssetConfig {
            asset: xlm.clone(),
            collateral_factor: 6000,
            liquidation_threshold: 7000,
            price: 4_000_000,
            decimals: 7,
        },
    );

    // Collateral value: $400, Liq collateral value: $400 * 0.70 = $280. Debt = $500.
    // Health factor = $280 / $500 = 0.56x (< 0.90x -> ForcedClose, is_liquidatable = true)
    let summary = calculate_multi_collateral_summary(&env, &account, &configs);
    assert!(summary.is_liquidatable);
    assert_eq!(summary.margin_call_level, MarginCallLevel::ForcedClose);

    // Liquidator repays 200 USDC debt, seizing XLM collateral with 5% bonus
    let seize_res = liquidate_cross_margin_multi_collateral(
        &env,
        &mut account,
        usdc.clone(),
        200 * 10_000_000,
        xlm.clone(),
        &configs,
    );
    assert!(seize_res.is_ok());
    let seized = seize_res.unwrap();
    // 200 USDC value * 1.05 = $210 value in XLM at $0.40 = 525 XLM
    assert_eq!(seized, 525 * 10_000_000);
}

#[test]
fn test_isolated_market_creation_and_risk_containment() {
    let env = Env::default();
    let market_id = Address::generate(&env);
    let volatile_token = Address::generate(&env);
    let usdc = Address::generate(&env);
    let attacker_token = Address::generate(&env);

    // Create isolated market with 50,000 USDC debt ceiling
    let mut market = IsolatedMarket::new(
        market_id.clone(),
        volatile_token.clone(),
        usdc.clone(),
        50_000 * 10_000_000, // debt ceiling
        6000,                // 60% LTV
        7000,                // 70% liq threshold
        500,                 // 5% bonus
    )
    .unwrap();

    let user = Address::generate(&env);
    let mut position = IsolatedUserPosition {
        owner: user.clone(),
        market_id: market_id.clone(),
        collateral_amount: 0,
        debt_amount: 0,
    };

    // 1. Deposit 100,000 volatile tokens ($1.00 each)
    assert!(deposit_isolated_market(
        &mut market,
        &mut position,
        volatile_token.clone(),
        100_000 * 10_000_000
    )
    .is_ok());
    assert_eq!(market.total_collateral, 100_000 * 10_000_000);

    // 2. Reject unapproved collateral asset
    assert!(
        deposit_isolated_market(&mut market, &mut position, attacker_token.clone(), 1000).is_err()
    );

    // 3. Reject borrowing unapproved asset (cannot borrow attacker token)
    assert!(borrow_isolated_market(
        &mut market,
        &mut position,
        attacker_token.clone(),
        1000 * 10_000_000,
        10_000_000,
        10_000_000,
        7
    )
    .is_err());

    // 4. Borrow 30,000 USDC (within 50,000 debt ceiling and 60,000 collateral capacity)
    assert!(borrow_isolated_market(
        &mut market,
        &mut position,
        usdc.clone(),
        30_000 * 10_000_000,
        10_000_000, // $1.00 volatile token
        10_000_000, // $1.00 usdc
        7
    )
    .is_ok());
    assert_eq!(market.current_debt, 30_000 * 10_000_000);

    // 5. Strict Risk Containment: Exceeding market debt ceiling
    // Trying to borrow 25,000 USDC more would make total debt 55,000 > 50,000 ceiling -> Must fail!
    assert!(borrow_isolated_market(
        &mut market,
        &mut position,
        usdc.clone(),
        25_000 * 10_000_000,
        10_000_000,
        10_000_000,
        7
    )
    .is_err());

    // 6. Partial repayment restores debt ceiling headroom
    let repaid = repay_isolated_market(
        &mut market,
        &mut position,
        usdc.clone(),
        10_000 * 10_000_000,
    )
    .unwrap();
    assert_eq!(repaid, 10_000 * 10_000_000);
    assert_eq!(market.current_debt, 20_000 * 10_000_000);
    assert_eq!(position.debt_amount, 20_000 * 10_000_000);

    // 7. Withdrawal constraint: cannot withdraw collateral below required margin
    assert!(withdraw_isolated_market(
        &mut market,
        &mut position,
        volatile_token.clone(),
        80_000 * 10_000_000, // remaining 20,000 collateral only supports 12,000 debt, but debt is 20,000
        10_000_000,
        10_000_000,
        7
    )
    .is_err());
}

#[test]
fn test_isolated_market_bad_debt_containment() {
    let env = Env::default();
    let market_id = Address::generate(&env);
    let volatile_token = Address::generate(&env);
    let usdc = Address::generate(&env);

    let mut market = IsolatedMarket::new(
        market_id.clone(),
        volatile_token.clone(),
        usdc.clone(),
        100_000 * 10_000_000,
        6000, // 60%
        7000, // 70%
        1000, // 10%
    )
    .unwrap();

    let user = Address::generate(&env);
    let mut position = IsolatedUserPosition {
        owner: user.clone(),
        market_id: market_id.clone(),
        collateral_amount: 100 * 10_000_000, // 100 tokens
        debt_amount: 50 * 10_000_000,        // 50 USDC debt
    };
    market.total_collateral = 100 * 10_000_000;
    market.current_debt = 50 * 10_000_000;

    // Volatile token price crashes from $1.00 to $0.10!
    // Collateral value: 100 * $0.10 = $10.00. Liq threshold: $7.00. Debt = $50.00.
    // Liquidator attempts to liquidate position by repaying 50 USDC
    let (repaid, seized) = liquidate_isolated_market_position(
        &mut market,
        &mut position,
        50 * 10_000_000,
        1_000_000,  // $0.10
        10_000_000, // $1.00
        7,
    )
    .unwrap();

    // Liquidator repays covered portion ($9.0909090) to seize all remaining collateral
    assert_eq!(repaid, 90_909_090);
    // All 100 collateral tokens were seized
    assert_eq!(seized, 100 * 10_000_000);
    assert_eq!(position.collateral_amount, 0);

    // CRITICAL RISK CONTAINMENT CHECK:
    // Bad debt ($40.9090910) is contained strictly in market.bad_debt_contained!
    // Remaining debt was wiped from position and isolated in the market pool.
    assert_eq!(market.bad_debt_contained, 409_090_910);
    assert_eq!(position.debt_amount, 0);
}

// -------------------------------------------------------------------------
// Protocol-wide emergency pause
// -------------------------------------------------------------------------

/// $1.00 with 7 decimals, the price scale the isolated-market math expects.
const UNIT_PRICE: i128 = 10_000_000;
/// 7 decimals.
const DECIMALS: u32 = 7;

/// Both assets priced at $1.00 with 7 decimals.
fn prices() -> PriceContext {
    PriceContext::symmetric(UNIT_PRICE, DECIMALS)
}

/// Build an isolated market with a 60 % LTV, 70 % liquidation threshold, and a
/// 5 % liquidator bonus.
fn test_market(env: &Env, collateral: &Address, borrow_asset: &Address) -> IsolatedMarket {
    IsolatedMarket::new(
        Address::generate(env),
        collateral.clone(),
        borrow_asset.clone(),
        1_000_000 * UNIT_PRICE,
        6000,
        7000,
        500,
    )
    .expect("valid market parameters")
}

/// Registry with `count` running markets, plus the admin, guardian, and a
/// stranger that should never be able to move the pause.
struct PauseFixture {
    env: Env,
    admin: Address,
    guardian: Address,
    stranger: Address,
    collateral: Address,
    usdc: Address,
    markets: Vec<Address>,
    registry: MarketRegistry,
}

fn pause_fixture(count: u32) -> PauseFixture {
    pause_fixture_with(Env::default(), count)
}

fn pause_fixture_with(env: Env, count: u32) -> PauseFixture {
    let admin = Address::generate(&env);
    let guardian = Address::generate(&env);
    let stranger = Address::generate(&env);
    let collateral = Address::generate(&env);
    let usdc = Address::generate(&env);

    let mut registry = MarketRegistry::new(&env, admin.clone(), Some(guardian.clone()));
    let mut markets: Vec<Address> = Vec::new(&env);
    for _ in 0..count {
        let market = test_market(&env, &collateral, &usdc);
        let id = market.market_id.clone();
        registry.open_market(0, market).expect("market registered");
        markets.push_back(id);
    }

    PauseFixture {
        env,
        admin,
        guardian,
        stranger,
        collateral,
        usdc,
        markets,
        registry,
    }
}

impl PauseFixture {
    /// A fresh position in market `i`.
    fn position(&self, i: u32) -> IsolatedUserPosition {
        IsolatedUserPosition {
            owner: Address::generate(&self.env),
            market_id: self.markets.get(i).unwrap(),
            collateral_amount: 0,
            debt_amount: 0,
        }
    }

    /// Fund market `i` with collateral and open a debt position, so the
    /// position is both solvent and liquidatable-capable.
    fn fund(&mut self, i: u32, collateral: i128, debt: i128) -> IsolatedUserPosition {
        let market_id = self.markets.get(i).unwrap();
        let mut position = self.position(i);
        self.registry
            .deposit(0, &market_id, &mut position, &self.collateral, collateral)
            .expect("deposit accepted");
        self.registry
            .borrow(0, &market_id, &mut position, &self.usdc, debt, &prices())
            .expect("borrow accepted");
        position
    }
}

/// Stand-in for a host contract, so events published by the pause engine are
/// recorded against a contract id the way they are in production.
struct HostContract;

impl ContractFunctionSet for HostContract {
    fn call(&self, _func: &str, _env: Env, _args: &[Val]) -> Option<Val> {
        None
    }
}

#[test]
fn test_incident_lifecycle_is_published_as_contract_events() {
    let env = Env::default();
    // Events only carry a contract id when published from inside a contract
    // frame, which is how a host contract would call this library.
    let contract = env.register(HostContract, ());

    env.as_contract(&contract, || {
        let mut f = pause_fixture_with(env.clone(), 2);
        f.registry
            .halt_all(&env, &f.guardian, 1_000, "oracle depeg")
            .expect("halt");
        f.registry.begin_recovery(&env, &f.admin).expect("recovery");
        f.registry
            .set_op_pause(&env, &f.admin, EmergencyOp::Borrow, true, 2_000, 600)
            .expect("kill switch");
        f.registry.resume_all(&env, &f.admin).expect("resume");
    });

    // An indexer must be able to reconstruct the incident from the event log
    // alone, including how many markets the cascade took down.
    let events = env.events().all();
    let mut lifecycle_events: alloc::vec::Vec<ContractEventBody> = alloc::vec::Vec::new();
    let mut saw_op_pause = false;
    for event in events.events().iter() {
        let ContractEventBody::V0(body) = &event.body;
        match body.topics.first() {
            Some(ScVal::Symbol(sym)) if sym.to_string() == "emergency" => {
                lifecycle_events.push(event.body.clone())
            }
            Some(ScVal::Symbol(sym)) if sym.to_string() == "op_pause" => saw_op_pause = true,
            _ => {}
        }
    }
    assert_eq!(
        lifecycle_events.len(),
        3,
        "halt, recovery, and resume are all logged"
    );
    assert!(saw_op_pause, "switch changes are logged too");

    // Event data is (from, to, caller, markets_suspended, at) for the halt.
    let ContractEventBody::V0(halt) = lifecycle_events.first().expect("halt event");
    let data: Vec<Val> = Vec::try_from_val(&env, &halt.data).expect("event data");
    assert_eq!(
        EmergencyState::try_from_val(&env, &data.get(1).expect("to")).expect("to state"),
        EmergencyState::Halted
    );
    assert_eq!(
        u32::try_from_val(&env, &data.get(3).expect("suspended")).expect("suspended count"),
        2
    );
    assert_eq!(
        u64::try_from_val(&env, &data.get(4).expect("at")).expect("at"),
        1_000
    );
}

#[test]
fn test_emergency_halt_cascades_to_every_market() {
    let mut f = pause_fixture(3);
    assert_eq!(f.registry.market_count(), 3);
    assert_eq!(f.registry.halted_market_count(), 0);

    let transition = f
        .registry
        .halt_all(&f.env, &f.admin, 1_000, "oracle depeg")
        .expect("admin may halt");
    assert_eq!(transition.from, EmergencyState::Normal);
    assert_eq!(transition.to, EmergencyState::Halted);
    assert_eq!(transition.at, 1_000);
    assert_eq!(f.registry.state(), EmergencyState::Halted);

    // Every market is halted, not just the first.
    assert_eq!(f.registry.halted_market_count(), 3);
    for i in 0..3 {
        let id = f.markets.get(i).unwrap();
        assert!(!f.registry.is_market_open(&id), "market {i} must be halted");
        let market = f.registry.market(&id).expect("market still registered");
        assert!(!market.is_active);
    }
    assert_eq!(f.registry.pause.suspended_count(), 3);
}

#[test]
fn test_emergency_halt_blocks_every_operation() {
    let mut f = pause_fixture(2);
    // Fund market 0 while the protocol is healthy.
    let mut position = f.fund(0, 1_000 * UNIT_PRICE, 500 * UNIT_PRICE);
    let market_id = f.markets.get(0).unwrap();

    f.registry
        .halt_all(&f.env, &f.admin, 1_000, "incident")
        .expect("halt");

    // Deposits, borrows, and market administration are refused.
    assert!(f
        .registry
        .deposit(1_000, &market_id, &mut position, &f.collateral, UNIT_PRICE)
        .is_err());
    assert!(f
        .registry
        .borrow(
            1_000,
            &market_id,
            &mut position,
            &f.usdc,
            UNIT_PRICE,
            &prices(),
        )
        .is_err());
    assert!(f
        .registry
        .open_market(1_000, test_market(&f.env, &f.collateral, &f.usdc))
        .is_err());

    // ... and so are the unwind operations, because a full stop is a full stop.
    assert!(f
        .registry
        .repay(1_000, &market_id, &mut position, &f.usdc, UNIT_PRICE)
        .is_err());
    assert!(f
        .registry
        .withdraw(
            1_000,
            &market_id,
            &mut position,
            &f.collateral,
            UNIT_PRICE,
            &prices(),
        )
        .is_err());
    assert!(f
        .registry
        .liquidate(1_000, &market_id, &mut position, UNIT_PRICE, &prices())
        .is_err());

    // A halt is reported ahead of an unknown market, so the pause state is
    // never masked by a caller mistake.
    assert_eq!(
        f.registry
            .deposit(1_000, &market_id, &mut position, &f.collateral, UNIT_PRICE)
            .unwrap_err(),
        "Protocol is halted by an emergency pause"
    );

    // Outside an incident, an unknown market id is reported as such.
    let mut healthy = pause_fixture(1);
    let unknown = Address::generate(&healthy.env);
    let mut position = healthy.position(0);
    assert_eq!(
        healthy
            .registry
            .deposit(0, &unknown, &mut position, &healthy.collateral, UNIT_PRICE)
            .unwrap_err(),
        "Market is not registered"
    );
}

#[test]
fn test_emergency_halt_authorizes_admin_and_guardian_only() {
    let mut f = pause_fixture(1);

    assert_eq!(
        f.registry
            .halt_all(&f.env, &f.stranger, 1_000, "nope")
            .unwrap_err(),
        "Caller is not authorized to halt the protocol"
    );
    assert_eq!(f.registry.state(), EmergencyState::Normal);
    assert_eq!(f.registry.halted_market_count(), 0);

    // The guardian exists so an incident can be contained without governance.
    f.registry
        .halt_all(&f.env, &f.guardian, 1_000, "guardian halt")
        .expect("guardian may halt");
    assert_eq!(f.registry.state(), EmergencyState::Halted);
    assert!(f.registry.pause.is_guardian(&f.guardian));
    assert!(!f.registry.pause.is_guardian(&f.stranger));
}

#[test]
fn test_guardian_cannot_resume_or_reconfigure() {
    let mut f = pause_fixture(1);
    f.registry
        .halt_all(&f.env, &f.guardian, 1_000, "guardian halt")
        .expect("halt");
    f.registry
        .begin_recovery(&f.env, &f.guardian)
        .expect("guardian may open the unwind path");

    // Stopping is urgent; restarting is a governance decision.
    assert_eq!(
        f.registry.resume_all(&f.env, &f.guardian).unwrap_err(),
        "Caller is not the protocol admin"
    );
    assert_eq!(
        f.registry
            .set_op_pause(&f.env, &f.guardian, EmergencyOp::Borrow, true, 1_000, 0,)
            .unwrap_err(),
        "Caller is not the protocol admin"
    );
    assert_eq!(
        f.registry
            .pause
            .set_guardian(&f.guardian, None)
            .unwrap_err(),
        "Caller is not the protocol admin"
    );
    assert_eq!(f.registry.state(), EmergencyState::Recovery);
}

#[test]
fn test_recovery_opens_the_unwind_path_and_keeps_risk_shut() {
    let mut f = pause_fixture(2);
    let mut position = f.fund(0, 1_000 * UNIT_PRICE, 500 * UNIT_PRICE);
    let market_id = f.markets.get(0).unwrap();

    f.registry
        .halt_all(&f.env, &f.admin, 1_000, "incident")
        .expect("halt");
    f.registry
        .begin_recovery(&f.env, &f.admin)
        .expect("recovery");
    assert_eq!(f.registry.state(), EmergencyState::Recovery);

    // The cascade has been lifted, so the market is running again...
    assert!(f.registry.is_market_open(&market_id));
    assert_eq!(f.registry.halted_market_count(), 0);

    // ... but nothing that opens new risk is allowed while unwinding.
    assert!(f
        .registry
        .deposit(2_000, &market_id, &mut position, &f.collateral, UNIT_PRICE)
        .is_err());
    assert!(f
        .registry
        .borrow(
            2_000,
            &market_id,
            &mut position,
            &f.usdc,
            UNIT_PRICE,
            &prices(),
        )
        .is_err());
    assert!(f
        .registry
        .open_market(2_000, test_market(&f.env, &f.collateral, &f.usdc))
        .is_err());

    // Exits stay open: repay, then withdraw what is left.
    let repaid = f
        .registry
        .repay(2_000, &market_id, &mut position, &f.usdc, 100 * UNIT_PRICE)
        .expect("repay stays open in recovery");
    assert_eq!(repaid, 100 * UNIT_PRICE);
    f.registry
        .withdraw(
            2_000,
            &market_id,
            &mut position,
            &f.collateral,
            10 * UNIT_PRICE,
            &prices(),
        )
        .expect("withdraw stays open in recovery");
    assert_eq!(position.debt_amount, 400 * UNIT_PRICE);
    assert_eq!(position.collateral_amount, 990 * UNIT_PRICE);
}

#[test]
fn test_liquidation_stays_open_during_recovery() {
    let mut f = pause_fixture(1);
    // 100 tokens of collateral against 50 USDC of debt: a comfortable 50 % LTV.
    let mut position = f.fund(0, 100 * UNIT_PRICE, 50 * UNIT_PRICE);

    f.registry
        .halt_all(&f.env, &f.admin, 1_000, "incident")
        .expect("halt");
    f.registry
        .begin_recovery(&f.env, &f.admin)
        .expect("recovery");

    // The collateral then loses 95 % of its value, which is exactly the
    // situation an emergency pause is called for.
    let crashed = PriceContext {
        collateral_price: 500_000,
        borrow_price: UNIT_PRICE,
        decimals: DECIMALS,
    };
    let market_id = f.markets.get(0).unwrap();
    let (repaid, seized) = f
        .registry
        .liquidate(2_000, &market_id, &mut position, 50 * UNIT_PRICE, &crashed)
        .expect("liquidation must never be the casualty of a pause");
    assert!(repaid > 0);
    assert_eq!(position.collateral_amount, 100 * UNIT_PRICE - seized);

    // The collateral is gone but 2.4 USDC of debt cannot be covered, so it is
    // contained inside this market instead of propagating anywhere else.
    let market = f.registry.market(&market_id).expect("market");
    assert_eq!(position.collateral_amount, 0);
    assert_eq!(position.debt_amount, 0);
    assert_eq!(market.total_collateral, 0);
    assert_eq!(market.current_debt, 0);
    assert_eq!(market.bad_debt_contained, 50 * UNIT_PRICE - repaid);
}

#[test]
fn test_resume_requires_the_recovery_step_first() {
    let mut f = pause_fixture(1);
    f.registry
        .halt_all(&f.env, &f.admin, 1_000, "incident")
        .expect("halt");

    // Skipping the unwind window would restart deposits against a protocol that
    // nobody has inspected yet.
    assert_eq!(
        f.registry.resume_all(&f.env, &f.admin).unwrap_err(),
        "Enter recovery before resuming so users can exit"
    );
    assert_eq!(f.registry.state(), EmergencyState::Halted);

    f.registry
        .begin_recovery(&f.env, &f.admin)
        .expect("recovery");
    let transition = f.registry.resume_all(&f.env, &f.admin).expect("resume");
    assert_eq!(transition.from, EmergencyState::Recovery);
    assert_eq!(transition.to, EmergencyState::Normal);
    assert_eq!(f.registry.state(), EmergencyState::Normal);
    assert_eq!(f.registry.halted_market_count(), 0);
}

#[test]
fn test_recovery_must_follow_a_halt() {
    let mut f = pause_fixture(1);
    assert_eq!(
        f.registry.begin_recovery(&f.env, &f.admin).unwrap_err(),
        "Recovery must follow a halt"
    );
    assert_eq!(f.registry.state(), EmergencyState::Normal);
}

#[test]
fn test_resume_restores_only_markets_the_cascade_suspended() {
    let mut f = pause_fixture(3);
    let market_id = f.markets.get(1).unwrap();

    // Governance took market 1 down for an unrelated reason before the incident.
    let mut offline = f.registry.market(&market_id).expect("market registered");
    offline.is_active = false;
    let index = f.registry.index_of(&market_id).unwrap();
    f.registry.markets.set(index, offline);
    assert_eq!(f.registry.halted_market_count(), 1);

    f.registry
        .halt_all(&f.env, &f.admin, 1_000, "incident")
        .expect("halt");
    assert_eq!(f.registry.halted_market_count(), 3);
    // Only the two running markets were recorded as cascade-suspended.
    assert_eq!(f.registry.pause.suspended_count(), 2);

    f.registry
        .resume_all(&f.env, &f.admin)
        .or_else(|_| {
            f.registry.begin_recovery(&f.env, &f.admin)?;
            f.registry.resume_all(&f.env, &f.admin)
        })
        .expect("resume");

    // Market 1 stays down; the cascade did not resurrect it.
    assert!(!f.registry.is_market_open(&market_id));
    assert_eq!(f.registry.halted_market_count(), 1);
    assert!(f.registry.is_market_open(&f.markets.get(0).unwrap()));
    assert!(f.registry.is_market_open(&f.markets.get(2).unwrap()));
    assert_eq!(f.registry.pause.suspended_count(), 0);
}

#[test]
fn test_rehalt_preserves_the_original_incident_time_and_reason() {
    let mut f = pause_fixture(1);
    f.registry
        .halt_all(&f.env, &f.admin, 1_000, "first reason")
        .expect("halt");

    // A second halt by the guardian must not overwrite the post-mortem record.
    f.registry
        .halt_all(&f.env, &f.guardian, 9_000, "second reason")
        .expect("re-halt");
    assert_eq!(f.registry.pause.halted_at, 1_000);
    assert_eq!(
        f.registry.pause.reason,
        soroban_sdk::Bytes::from_slice(&f.env, b"first reason")
    );
}

#[test]
fn test_per_operation_pause_is_independent_of_the_lifecycle() {
    let mut f = pause_fixture(1);
    let mut position = f.position(0);
    let market_id = f.markets.get(0).unwrap();

    f.registry
        .set_op_pause(&f.env, &f.admin, EmergencyOp::Borrow, true, 500, 0)
        .expect("admin may pause an operation");

    // Only borrowing is shut; the protocol is not in an incident.
    assert_eq!(f.registry.state(), EmergencyState::Normal);
    assert!(f
        .registry
        .deposit(
            500,
            &market_id,
            &mut position,
            &f.collateral,
            1_000 * UNIT_PRICE
        )
        .is_ok());
    assert!(f
        .registry
        .borrow(
            500,
            &market_id,
            &mut position,
            &f.usdc,
            UNIT_PRICE,
            &prices(),
        )
        .is_err());
    // Repaying and withdrawing are untouched: the switch is per operation.
    assert_eq!(
        f.registry
            .repay(500, &market_id, &mut position, &f.usdc, UNIT_PRICE)
            .expect("repay is not paused"),
        0
    );
    assert!(f
        .registry
        .withdraw(
            500,
            &market_id,
            &mut position,
            &f.collateral,
            UNIT_PRICE,
            &prices(),
        )
        .is_ok());
}

#[test]
fn test_per_operation_pause_fuse_expires_on_its_own() {
    let mut f = pause_fixture(1);
    let mut position = f.position(0);
    let market_id = f.markets.get(0).unwrap();

    // A 600-second fuse on deposits.
    f.registry
        .set_op_pause(&f.env, &f.admin, EmergencyOp::Deposit, true, 500, 600)
        .expect("fuse set");
    let switch = f.registry.pause.op_pause(EmergencyOp::Deposit);
    assert!(switch.paused);
    assert_eq!(switch.expires_at, 1_100);
    assert!(switch.engaged(1_099));
    assert!(!switch.engaged(1_100));

    assert!(f
        .registry
        .deposit(600, &market_id, &mut position, &f.collateral, UNIT_PRICE)
        .is_err());
    // A forgotten one-shot kill switch must not stay engaged forever.
    assert!(f
        .registry
        .deposit(1_100, &market_id, &mut position, &f.collateral, UNIT_PRICE)
        .is_ok());
}

#[test]
fn test_per_operation_pause_without_cooldown_stays_engaged() {
    let mut f = pause_fixture(1);
    let mut position = f.position(0);
    let market_id = f.markets.get(0).unwrap();

    f.registry
        .set_op_pause(&f.env, &f.admin, EmergencyOp::Withdraw, true, 500, 0)
        .expect("kill switch set");
    let switch = f.registry.pause.op_pause(EmergencyOp::Withdraw);
    assert!(switch.engaged(u64::MAX));

    assert!(f
        .registry
        .withdraw(
            10_000_000_000,
            &market_id,
            &mut position,
            &f.collateral,
            UNIT_PRICE,
            &prices(),
        )
        .is_err());

    // Lifting it explicitly is the only way out.
    f.registry
        .set_op_pause(
            &f.env,
            &f.admin,
            EmergencyOp::Withdraw,
            false,
            10_000_000_000,
            0,
        )
        .expect("kill switch lifted");
    assert!(!f
        .registry
        .pause
        .op_pause(EmergencyOp::Withdraw)
        .engaged(10_000_000_000));
}

#[test]
fn test_master_switch_cannot_be_set_by_hand() {
    let mut f = pause_fixture(1);
    assert_eq!(
        f.registry
            .set_op_pause(&f.env, &f.admin, EmergencyOp::All, true, 0, 0)
            .unwrap_err(),
        "Use halt/resume to control the master switch"
    );
    assert_eq!(
        f.registry.pause.op_pause(EmergencyOp::All),
        OpPause {
            paused: false,
            expires_at: 0
        }
    );
}

#[test]
fn test_resume_clears_every_inherited_kill_switch() {
    let mut f = pause_fixture(1);
    let mut position = f.fund(0, 1_000 * UNIT_PRICE, 100 * UNIT_PRICE);
    let market_id = f.markets.get(0).unwrap();

    f.registry
        .set_op_pause(&f.env, &f.admin, EmergencyOp::Borrow, true, 0, 0)
        .expect("pre-incident kill switch");

    f.registry
        .halt_all(&f.env, &f.admin, 1_000, "incident")
        .expect("halt");
    f.registry
        .begin_recovery(&f.env, &f.admin)
        .expect("recovery");
    f.registry.resume_all(&f.env, &f.admin).expect("resume");

    // Coming out of an incident must not silently inherit a switch that was set
    // before it.
    assert!(!f.registry.pause.op_pause(EmergencyOp::Borrow).paused);
    assert!(f
        .registry
        .borrow(
            2_000,
            &market_id,
            &mut position,
            &f.usdc,
            UNIT_PRICE,
            &prices(),
        )
        .is_ok());
}

#[test]
fn test_paused_operation_leaves_market_accounting_untouched() {
    let mut f = pause_fixture(1);
    let mut position = f.fund(0, 1_000 * UNIT_PRICE, 500 * UNIT_PRICE);
    let market_id = f.markets.get(0).unwrap();
    let before = f.registry.market(&market_id).expect("market");

    f.registry
        .halt_all(&f.env, &f.admin, 1_000, "incident")
        .expect("halt");

    // Rejected operations must not half-apply: a blocked borrow cannot be the
    // reason a market's debt accounting drifts.
    assert!(f
        .registry
        .borrow(
            1_000,
            &market_id,
            &mut position,
            &f.usdc,
            400 * UNIT_PRICE,
            &prices(),
        )
        .is_err());
    let during = f.registry.market(&market_id).expect("market");
    // Only `is_active` changed; debt and collateral are exactly as before.
    assert_eq!(during.current_debt, before.current_debt);
    assert_eq!(during.total_collateral, before.total_collateral);
    assert_eq!(during.bad_debt_contained, before.bad_debt_contained);
    assert!(!during.is_active);
    assert_eq!(position.debt_amount, 500 * UNIT_PRICE);
}

#[test]
fn test_duplicate_market_registration_is_rejected() {
    let mut f = pause_fixture(1);
    let existing = f.markets.get(0).unwrap();
    // Same market id, different assets: a re-registration must not be able to
    // silently replace the incumbent market.
    let duplicate = IsolatedMarket::new(
        existing.clone(),
        Address::generate(&f.env),
        Address::generate(&f.env),
        1_000_000 * UNIT_PRICE,
        6000,
        7000,
        500,
    )
    .expect("valid market parameters");

    assert_eq!(
        f.registry.open_market(0, duplicate).unwrap_err(),
        "Market is already registered"
    );
    assert_eq!(f.registry.market_count(), 1);
}

#[test]
fn test_guardian_rotation_is_admin_only_and_takes_effect() {
    let mut f = pause_fixture(1);
    let replacement = Address::generate(&f.env);

    f.registry
        .pause
        .set_guardian(&f.admin, Some(replacement.clone()))
        .expect("admin rotates the guardian");
    assert_eq!(f.registry.guardian(), Some(replacement.clone()));
    assert!(!f.registry.pause.is_guardian(&f.guardian));

    // The new guardian can halt; the old one can no longer.
    f.registry
        .halt_all(&f.env, &replacement, 1_000, "new guardian")
        .expect("new guardian may halt");
    assert_eq!(
        f.registry
            .halt_all(&f.env, &f.guardian, 2_000, "old guardian")
            .unwrap_err(),
        "Caller is not authorized to halt the protocol"
    );

    // ... and the guardian can be removed entirely.
    f.registry
        .pause
        .set_guardian(&f.admin, None)
        .expect("admin clears the guardian");
    assert_eq!(f.registry.guardian(), None);
}

#[test]
fn test_registry_without_a_guardian_only_halts_for_the_admin() {
    let env = Env::default();
    let admin = Address::generate(&env);
    let stranger = Address::generate(&env);
    let collateral = Address::generate(&env);
    let usdc = Address::generate(&env);

    let mut registry = MarketRegistry::new(&env, admin.clone(), None);
    registry
        .open_market(0, test_market(&env, &collateral, &usdc))
        .expect("market");

    assert!(registry.halt_all(&env, &stranger, 1_000, "nope").is_err());
    assert_eq!(registry.state(), EmergencyState::Normal);
    registry
        .halt_all(&env, &admin, 1_000, "ok")
        .expect("admin may halt");
    assert_eq!(registry.state(), EmergencyState::Halted);
}
