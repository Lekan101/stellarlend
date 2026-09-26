use crate::account::{MarginAccount, MarginCallLevel, Position};
use soroban_sdk::{contracttype, Address, Env};

pub const BPS_SCALE: i128 = 10_000;

#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct IsolatedMarket {
    pub market_id: Address,
    pub collateral_asset: Address,
    pub allowed_borrow_asset: Address,
    /// Maximum aggregate debt that can be minted in this isolated market
    pub debt_ceiling: i128,
    /// Current aggregate debt minted in this market
    pub current_debt: i128,
    /// Maximum borrow capacity per unit of collateral (e.g. 7000 = 70%)
    pub collateral_factor_bps: i128,
    /// Threshold at which position becomes liquidatable (e.g. 7500 = 75%)
    pub liquidation_threshold_bps: i128,
    /// Bonus percentage given to liquidator (e.g. 500 = 5%)
    pub liquidation_bonus_bps: i128,
    /// Total collateral locked strictly in this market
    pub total_collateral: i128,
    /// Bad debt contained strictly within this isolated market
    pub bad_debt_contained: i128,
    pub is_active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct IsolatedUserPosition {
    pub owner: Address,
    pub market_id: Address,
    pub collateral_amount: i128,
    pub debt_amount: i128,
}

impl IsolatedMarket {
    pub fn new(
        market_id: Address,
        collateral_asset: Address,
        allowed_borrow_asset: Address,
        debt_ceiling: i128,
        collateral_factor_bps: i128,
        liquidation_threshold_bps: i128,
        liquidation_bonus_bps: i128,
    ) -> Result<Self, &'static str> {
        if debt_ceiling <= 0 {
            return Err("Debt ceiling must be positive");
        }
        if collateral_factor_bps <= 0 || collateral_factor_bps > BPS_SCALE {
            return Err("Invalid collateral factor");
        }
        if liquidation_threshold_bps < collateral_factor_bps
            || liquidation_threshold_bps > BPS_SCALE
        {
            return Err("Liquidation threshold must be >= collateral factor and <= 100%");
        }
        if !(0..=2_000).contains(&liquidation_bonus_bps) {
            return Err("Liquidation bonus must be between 0% and 20%");
        }

        Ok(Self {
            market_id,
            collateral_asset,
            allowed_borrow_asset,
            debt_ceiling,
            current_debt: 0,
            collateral_factor_bps,
            liquidation_threshold_bps,
            liquidation_bonus_bps,
            total_collateral: 0,
            bad_debt_contained: 0,
            is_active: true,
        })
    }

    /// Check if market debt ceiling has available headroom for new borrowing.
    pub fn has_debt_headroom(&self, amount: i128) -> bool {
        self.current_debt.saturating_add(amount) <= self.debt_ceiling
    }
}

/// Deposit collateral into an isolated lending market.
pub fn deposit_isolated_market(
    market: &mut IsolatedMarket,
    position: &mut IsolatedUserPosition,
    asset: Address,
    amount: i128,
) -> Result<(), &'static str> {
    if !market.is_active {
        return Err("Market is paused or inactive");
    }
    if asset != market.collateral_asset {
        return Err("Asset does not match isolated market collateral asset");
    }
    if amount <= 0 {
        return Err("Deposit amount must be positive");
    }

    market.total_collateral = market.total_collateral.saturating_add(amount);
    position.collateral_amount = position.collateral_amount.saturating_add(amount);

    Ok(())
}

/// Borrow from an isolated market strictly bounded by debt ceiling and collateral.
pub fn borrow_isolated_market(
    market: &mut IsolatedMarket,
    position: &mut IsolatedUserPosition,
    borrow_asset: Address,
    amount: i128,
    collateral_price: i128,
    borrow_price: i128,
    decimals: u32,
) -> Result<(), &'static str> {
    if !market.is_active {
        return Err("Market is inactive");
    }
    if borrow_asset != market.allowed_borrow_asset {
        return Err("Risk containment: asset cannot be borrowed from this isolated market");
    }
    if amount <= 0 {
        return Err("Borrow amount must be positive");
    }

    // Strict risk containment: Market-level debt ceiling
    if !market.has_debt_headroom(amount) {
        return Err("Risk containment: isolated market debt ceiling exceeded");
    }

    // Check position-level collateral factor
    let divisor = 10i128.pow(decimals);
    let collateral_val =
        (position.collateral_amount.saturating_mul(collateral_price)) / divisor.max(1);
    let max_borrow_val = (collateral_val.saturating_mul(market.collateral_factor_bps)) / BPS_SCALE;

    let total_pos_debt = position.debt_amount.saturating_add(amount);
    let total_pos_debt_val = (total_pos_debt.saturating_mul(borrow_price)) / divisor.max(1);

    if total_pos_debt_val > max_borrow_val {
        return Err("Borrow exceeds isolated collateral borrowing power");
    }

    market.current_debt = market.current_debt.saturating_add(amount);
    position.debt_amount = total_pos_debt;

    Ok(())
}

/// Repay debt in an isolated market.
pub fn repay_isolated_market(
    market: &mut IsolatedMarket,
    position: &mut IsolatedUserPosition,
    repay_asset: Address,
    amount: i128,
) -> Result<i128, &'static str> {
    if repay_asset != market.allowed_borrow_asset {
        return Err("Repay asset does not match market borrow asset");
    }
    if amount <= 0 {
        return Err("Repayment amount must be positive");
    }

    let actual_repay = amount.min(position.debt_amount);
    position.debt_amount -= actual_repay;
    market.current_debt = (market.current_debt - actual_repay).max(0);

    Ok(actual_repay)
}

/// Withdraw collateral from an isolated market, ensuring position remains solvent.
pub fn withdraw_isolated_market(
    market: &mut IsolatedMarket,
    position: &mut IsolatedUserPosition,
    asset: Address,
    amount: i128,
    collateral_price: i128,
    borrow_price: i128,
    decimals: u32,
) -> Result<(), &'static str> {
    if asset != market.collateral_asset {
        return Err("Asset does not match isolated collateral asset");
    }
    if amount <= 0 {
        return Err("Withdrawal amount must be positive");
    }
    if position.collateral_amount < amount {
        return Err("Insufficient collateral balance");
    }

    let remaining_collateral = position.collateral_amount - amount;
    if position.debt_amount > 0 {
        let divisor = 10i128.pow(decimals);
        let collateral_val =
            (remaining_collateral.saturating_mul(collateral_price)) / divisor.max(1);
        let borrow_power =
            (collateral_val.saturating_mul(market.collateral_factor_bps)) / BPS_SCALE;
        let debt_val = (position.debt_amount.saturating_mul(borrow_price)) / divisor.max(1);

        if debt_val > borrow_power {
            return Err("Withdrawal would breach isolated collateral ratio");
        }
    }

    position.collateral_amount = remaining_collateral;
    market.total_collateral = (market.total_collateral - amount).max(0);

    Ok(())
}

/// Liquidate an isolated market position.
/// Risk containment guarantee: Bad debt is strictly contained within this isolated market
/// and never propagates or impairs any other market or cross positions.
pub fn liquidate_isolated_market_position(
    market: &mut IsolatedMarket,
    position: &mut IsolatedUserPosition,
    repay_amount: i128,
    collateral_price: i128,
    borrow_price: i128,
    decimals: u32,
) -> Result<(i128, i128), &'static str> {
    if position.debt_amount <= 0 {
        return Err("Position has no debt to liquidate");
    }

    let divisor = 10i128.pow(decimals);
    let collateral_val =
        (position.collateral_amount.saturating_mul(collateral_price)) / divisor.max(1);
    let liq_collateral_val =
        (collateral_val.saturating_mul(market.liquidation_threshold_bps)) / BPS_SCALE;
    let debt_val = (position.debt_amount.saturating_mul(borrow_price)) / divisor.max(1);

    if debt_val <= liq_collateral_val {
        return Err("Position is healthy, cannot liquidate");
    }

    let actual_repay = repay_amount.min(position.debt_amount);
    let repay_val = (actual_repay.saturating_mul(borrow_price)) / divisor.max(1);

    let seize_val =
        (repay_val.saturating_mul(BPS_SCALE + market.liquidation_bonus_bps)) / BPS_SCALE;
    let target_seize = (seize_val.saturating_mul(divisor)) / collateral_price.max(1);

    let actual_seize = target_seize.min(position.collateral_amount);
    let effective_repay = if target_seize > position.collateral_amount {
        // Liquidator repays only what is covered by the available seized collateral
        (actual_seize
            .saturating_mul(collateral_price)
            .saturating_mul(BPS_SCALE))
            / ((BPS_SCALE + market.liquidation_bonus_bps)
                .saturating_mul(borrow_price)
                .max(1))
    } else {
        actual_repay
    };

    position.collateral_amount -= actual_seize;
    market.total_collateral = (market.total_collateral - actual_seize).max(0);

    position.debt_amount = (position.debt_amount - effective_repay).max(0);
    market.current_debt = (market.current_debt - effective_repay).max(0);

    // If collateral is exhausted but debt remains: bad debt is strictly contained
    if position.collateral_amount == 0 && position.debt_amount > 0 {
        let bad_debt = position.debt_amount;
        market.bad_debt_contained = market.bad_debt_contained.saturating_add(bad_debt);
        market.current_debt = (market.current_debt - bad_debt).max(0);
        position.debt_amount = 0;
    }

    Ok((effective_repay, actual_seize))
}

// -------------------------------------------------------------------------
// Legacy compatibility functions
// -------------------------------------------------------------------------

pub fn check_isolated_position_health(
    _env: &Env,
    position: &Position,
    current_price: i128,
) -> MarginCallLevel {
    if position.debt == 0 {
        return MarginCallLevel::Safe;
    }

    let collateral_value = (position.amount * current_price) / 10_000;
    let debt_ratio = (position.debt * 100) / collateral_value.max(1);

    if debt_ratio >= 90 {
        MarginCallLevel::ForcedClose
    } else if debt_ratio >= 80 {
        MarginCallLevel::Liquidation
    } else if debt_ratio >= 70 {
        MarginCallLevel::Warning
    } else {
        MarginCallLevel::Safe
    }
}

pub fn liquidate_isolated_position(
    env: &Env,
    account: &mut MarginAccount,
    position_index: u32,
    current_price: i128,
) -> Result<(), &'static str> {
    if !account.is_isolated() {
        return Err("Account is not in isolated mode");
    }

    if position_index as usize >= account.positions.len() as usize {
        return Err("Invalid position index");
    }

    let position = account.positions.get(position_index).unwrap();
    let health = check_isolated_position_health(env, &position, current_price);

    if health == MarginCallLevel::Safe || health == MarginCallLevel::Warning {
        return Err("Position is healthy, cannot liquidate");
    }

    account.positions.remove(position_index);

    Ok(())
}
