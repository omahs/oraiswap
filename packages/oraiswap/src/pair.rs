use std::convert::TryInto;

use crate::{
    asset::{Asset, AssetInfo, PairInfo},
    error::ContractError,
};
use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Decimal256, Uint256};

use cosmwasm_std::{Addr, Decimal, Uint128};
use cw20::Cw20ReceiveMsg;

/// Default commission rate == 0.3%
/// in the future need to update ?
pub const DEFAULT_COMMISSION_RATE: &str = "0.003";

#[cw_serde]
pub struct InstantiateMsg {
    /// Asset infos
    pub asset_infos: [AssetInfo; 2],
    /// Token contract code id for initialization
    pub token_code_id: u64,

    /// Oracle contract for query oracle information
    pub oracle_addr: Addr,

    pub commission_rate: Option<String>,
}

#[cw_serde]
pub enum ExecuteMsg {
    Receive(Cw20ReceiveMsg),
    /// ProvideLiquidity a user provides pool liquidity
    ProvideLiquidity {
        assets: [Asset; 2],
        slippage_tolerance: Option<Decimal>,
        receiver: Option<Addr>,
    },
    /// Swap an offer asset to the other
    Swap {
        offer_asset: Asset,
        belief_price: Option<Uint128>,
        max_spread: Option<Decimal>,
        to: Option<Addr>,
    },
}

#[cw_serde]
pub enum Cw20HookMsg {
    /// Sell a given amount of asset
    Swap {
        belief_price: Option<Uint128>,
        max_spread: Option<Decimal>,
        to: Option<String>,
    },
    WithdrawLiquidity {},
}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    #[returns(PairResponse)]
    Pair {},
    #[returns(PoolResponse)]
    Pool {},
    #[returns(SimulationResponse)]
    Simulation { offer_asset: Asset },
    #[returns(ReverseSimulationResponse)]
    ReverseSimulation { ask_asset: Asset },
}

// We define a custom struct for each query response
#[cw_serde]
pub struct PoolResponse {
    pub assets: [Asset; 2],
    pub total_share: Uint128,
}

#[cw_serde]
pub struct PairResponse {
    pub info: PairInfo,
}

/// SimulationResponse returns swap simulation response
#[cw_serde]
pub struct SimulationResponse {
    pub return_amount: Uint128,
    pub spread_amount: Uint128,
    pub commission_amount: Uint128,
}

/// ReverseSimulationResponse returns reverse swap simulation response
#[cw_serde]
pub struct ReverseSimulationResponse {
    pub offer_amount: Uint128,
    pub spread_amount: Uint128,
    pub commission_amount: Uint128,
}

/// We currently take no arguments for migrations
#[cw_serde]
pub struct MigrateMsg {}

pub fn compute_swap(
    offer_pool: Uint128,
    ask_pool: Uint128,
    offer_amount: Uint128,
    commission_rate: Decimal256,
) -> Result<(Uint128, Uint128, Uint128), ContractError> {
    if offer_pool.is_zero() {
        return Err(ContractError::OfferPoolIsZero {});
    }

    // convert to uint256
    let offer_pool: Uint256 = offer_pool.into();
    let ask_pool: Uint256 = ask_pool.into();
    let offer_amount: Uint256 = offer_amount.into();

    // offer => ask
    // ask_amount = (ask_pool - cp / (offer_pool + offer_amount)) * (1 - commission_rate)
    let cp = offer_pool * ask_pool;

    let return_amount =
        ask_pool - Decimal256::from_ratio(cp, offer_pool + offer_amount) * Uint256::one();

    // calculate spread & commission
    let spread_amount =
        (offer_amount * Decimal256::from_ratio(ask_pool, offer_pool)) - return_amount;

    let commission_amount = return_amount * commission_rate;

    // commission will be absorbed to pool
    let return_amount = return_amount - commission_amount;
    Ok((
        u128::from_le_bytes(return_amount.to_le_bytes()[0..16].try_into().unwrap()).into(),
        u128::from_le_bytes(spread_amount.to_le_bytes()[0..16].try_into().unwrap()).into(),
        u128::from_le_bytes(commission_amount.to_le_bytes()[0..16].try_into().unwrap()).into(),
    ))
}

pub fn compute_offer_amount(
    offer_pool: Uint128,
    ask_pool: Uint128,
    ask_amount: Uint128,
    commission_rate: Decimal256,
) -> Result<(Uint128, Uint128, Uint128), ContractError> {
    let offer_pool: Uint256 = offer_pool.into();
    let ask_pool: Uint256 = ask_pool.into();
    let ask_amount: Uint256 = ask_amount.into();

    // ask => offer
    // offer_amount = cp / (ask_pool - ask_amount / (1 - commission_rate)) - offer_pool
    let cp: Uint256 = offer_pool * ask_pool;

    let one_minus_commission = Decimal256::one() - commission_rate;
    let inv_one_minus_commission = Decimal256::one() / one_minus_commission;

    let offer_amount: Uint256 = Uint256::one()
        .multiply_ratio(cp, ask_pool - ask_amount * inv_one_minus_commission)
        - offer_pool;

    let before_commission_deduction: Uint256 = ask_amount * inv_one_minus_commission;
    let before_spread_deduction: Uint256 =
        offer_amount * Decimal256::from_ratio(ask_pool, offer_pool);

    let spread_amount = if before_spread_deduction > before_commission_deduction {
        before_spread_deduction - before_commission_deduction
    } else {
        Uint256::zero()
    };

    let commission_amount = before_commission_deduction * commission_rate;

    // check small amount swap
    if spread_amount.is_zero() || commission_amount.is_zero() {
        return Err(ContractError::TooSmallOfferAmount {});
    }

    Ok((
        u128::from_le_bytes(offer_amount.to_le_bytes()[0..16].try_into().unwrap()).into(),
        u128::from_le_bytes(spread_amount.to_le_bytes()[0..16].try_into().unwrap()).into(),
        u128::from_le_bytes(commission_amount.to_le_bytes()[0..16].try_into().unwrap()).into(),
    ))
}
