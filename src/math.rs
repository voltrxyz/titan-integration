use anyhow::Result;

use crate::constants::{MAX_FEE_BPS, ONE_YEAR_U64};
use crate::errors::VoltrError;

/// Calculate LP tokens to mint on the **initial** deposit (when LP supply is 0).
///
/// Normalizes the asset `amount` from `from_decimals` to `to_decimals` (LP always 9).
pub fn calc_init_lp_to_mint(amount: u64, from_decimals: u8, to_decimals: u8) -> Result<u64> {
    let result = (amount as u128)
        .checked_mul(10u128.pow(to_decimals as u32))
        .and_then(|v| v.checked_div(10u128.pow(from_decimals as u32)))
        .ok_or(VoltrError::MathOverflow)?;
    Ok(u64::try_from(result)?)
}

/// Calculate LP tokens to mint on a subsequent deposit.
///
/// Maintains the ratio: `lp_to_mint / (total_lp + lp_to_mint) = amount_after_fee / (total_asset + amount)`
///
/// Formula: `x = (a * (10000 - i) * y) / (10000 * z - a * (10000 - i))`
/// where a = amount, i = issuance_fee_bps, y = total_lp, z = total_asset + amount
pub fn calc_deposit_lp_to_mint(
    amount: u64,
    total_lp_supply_pre_deposit: u64,
    total_asset_pre_deposit: u64,
    issuance_fee_bps: u16,
) -> Result<u64> {
    let total_asset_post_deposit = total_asset_pre_deposit
        .checked_add(amount)
        .ok_or(VoltrError::MathOverflow)? as u128;

    let fee_adjusted = MAX_FEE_BPS
        .checked_sub(issuance_fee_bps)
        .ok_or(VoltrError::MathOverflow)? as u128;

    let numerator = (amount as u128)
        .checked_mul(total_lp_supply_pre_deposit as u128)
        .and_then(|v| v.checked_mul(fee_adjusted))
        .ok_or(VoltrError::MathOverflow)?;

    let denominator = total_asset_post_deposit
        .checked_mul(MAX_FEE_BPS as u128)
        .and_then(|v| v.checked_sub((amount as u128).checked_mul(fee_adjusted)?))
        .ok_or(VoltrError::MathOverflow)?;

    if denominator == 0 {
        return Err(VoltrError::DivisionByZero.into());
    }

    let lp_to_mint = numerator
        .checked_div(denominator)
        .ok_or(VoltrError::DivisionByZero)?;

    Ok(u64::try_from(lp_to_mint)?)
}

/// Calculate the management fee in asset terms for a given time period.
pub fn calc_management_fee_amount_in_asset(
    time_elapsed: u64,
    total_asset_value: u64,
    management_fee_bps: u16,
) -> Result<u64> {
    let divisor = (MAX_FEE_BPS as u64)
        .checked_mul(ONE_YEAR_U64)
        .ok_or(VoltrError::MathOverflow)? as u128;

    let fee_amount = (total_asset_value as u128)
        .checked_mul(time_elapsed as u128)
        .and_then(|v| v.checked_mul(management_fee_bps as u128))
        .and_then(|v| {
            v.checked_add(divisor.saturating_sub(1))
                .and_then(|v| v.checked_div(divisor))
        })
        .ok_or(VoltrError::MathOverflow)?;

    Ok(u64::try_from(fee_amount)?)
}

/// Calculate asset tokens to redeem for a given LP burn amount.
///
/// Formula (from on-chain accounting.rs):
///   asset_pre_fee = lp_to_burn * (total_unlocked_asset / total_lp_supply_incl_fees)
///   asset_post_fee = asset_pre_fee * (10000 - redemption_fee_bps) / 10000
pub fn calc_withdraw_asset_to_redeem(
    amount_lp_to_burn: u64,
    total_lp_supply_pre_withdraw: u64,
    total_unlocked_asset: u64,
    redemption_fee_bps: u16,
) -> Result<u64> {
    if total_lp_supply_pre_withdraw == 0 {
        return Err(VoltrError::DivisionByZero.into());
    }

    let asset_pre_fee = (amount_lp_to_burn as u128)
        .checked_mul(total_unlocked_asset as u128)
        .and_then(|v| v.checked_div(total_lp_supply_pre_withdraw as u128))
        .ok_or(VoltrError::MathOverflow)?;

    let fee_adjusted = MAX_FEE_BPS
        .checked_sub(redemption_fee_bps)
        .ok_or(VoltrError::MathOverflow)? as u128;

    let asset_post_fee = asset_pre_fee
        .checked_mul(fee_adjusted)
        .and_then(|v| v.checked_div(MAX_FEE_BPS as u128))
        .ok_or(VoltrError::MathOverflow)?;

    Ok(u64::try_from(asset_post_fee)?)
}

/// Calculate LP tokens to mint for accumulated fees.
///
/// `lp_to_mint = (fee_amount * total_lp_supply) / (total_assets - fee_amount)`
pub fn calc_fee_lp_to_mint(
    fee_amount: u64,
    total_lp_supply_pre_fee: u64,
    total_asset_post_fee: u64,
) -> Result<u64> {
    let denominator = (total_asset_post_fee as u128)
        .checked_sub(fee_amount as u128)
        .ok_or(VoltrError::MathOverflow)?;

    if denominator == 0 {
        return Err(VoltrError::DivisionByZero.into());
    }

    let numerator = (fee_amount as u128)
        .checked_mul(total_lp_supply_pre_fee as u128)
        .ok_or(VoltrError::MathOverflow)?;

    let lp_to_mint = numerator
        .checked_add(denominator.saturating_sub(1))
        .and_then(|v| v.checked_div(denominator))
        .ok_or(VoltrError::DivisionByZero)?;

    Ok(u64::try_from(lp_to_mint)?)
}
