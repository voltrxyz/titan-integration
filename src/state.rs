use anyhow::Result;
use solana_pubkey::Pubkey;

use crate::errors::VoltrError;

const DISCRIMINATOR_SIZE: usize = 8;

#[derive(Clone, Debug)]
pub struct Vault {
    pub asset: VaultAsset,
    pub lp: VaultLp,
    pub vault_configuration: VaultConfiguration,
    pub fee_configuration: FeeConfiguration,
    pub fee_update: FeeUpdate,
    pub fee_state: FeeState,
    pub dead_weight: u64,
    pub high_water_mark: HighWaterMark,
    pub last_updated_ts: u64,
    pub locked_profit_state: LockedProfitState,
}

impl Vault {
    pub fn load(account_data: &[u8]) -> Result<Self> {
        let d = DISCRIMINATOR_SIZE;

        let asset = VaultAsset::load(&account_data[d + 96..d + 264])?;
        let lp = VaultLp::load(&account_data[d + 264..d + 360])?;
        let vault_configuration =
            VaultConfiguration::load(&account_data[d + 424..d + 504])?;
        let fee_configuration =
            FeeConfiguration::load(&account_data[d + 504..d + 552])?;
        let fee_update = FeeUpdate::load(&account_data[d + 552..d + 568])?;
        let fee_state = FeeState::load(&account_data[d + 568..d + 608])?;
        let dead_weight =
            u64::from_le_bytes(account_data[d + 608..d + 616].try_into()?);
        let high_water_mark =
            HighWaterMark::load(&account_data[d + 616..d + 648])?;
        let last_updated_ts =
            u64::from_le_bytes(account_data[d + 648..d + 656].try_into()?);
        let locked_profit_state =
            LockedProfitState::load(&account_data[d + 664..d + 680])?;

        Ok(Vault {
            asset,
            lp,
            vault_configuration,
            fee_configuration,
            fee_update,
            fee_state,
            dead_weight,
            high_water_mark,
            last_updated_ts,
            locked_profit_state,
        })
    }

    pub fn get_total_asset_value(&self) -> u64 {
        self.asset.total_value
    }

    pub fn get_total_accumulated_lp_fees(&self) -> Result<u64> {
        self.fee_state
            .accumulated_lp_admin_fees
            .checked_add(self.fee_state.accumulated_lp_manager_fees)
            .and_then(|s| s.checked_add(self.fee_state.accumulated_lp_protocol_fees))
            .ok_or_else(|| VoltrError::MathOverflow.into())
    }

    pub fn get_total_lp_supply_incl_fees(&self, total_lp_supply_excl_fees: u64) -> Result<u64> {
        self.get_total_accumulated_lp_fees()?
            .checked_add(total_lp_supply_excl_fees)
            .and_then(|s: u64| s.checked_add(self.dead_weight))
            .ok_or_else(|| VoltrError::MathOverflow.into())
    }

    pub fn get_total_fee_configuration_management_fee(&self) -> Result<u16> {
        self.fee_configuration
            .admin_management_fee
            .checked_add(self.fee_configuration.manager_management_fee)
            .and_then(|s| s.checked_add(self.fee_configuration.protocol_management_fee))
            .ok_or_else(|| VoltrError::MathOverflow.into())
    }

    pub fn get_unlocked_asset_value(&self, current_ts: u64) -> Result<u64> {
        let locked_profit = self.locked_profit_state.calculate_locked_profit(
            self.vault_configuration.locked_profit_degradation_duration,
            current_ts,
        )?;
        self.asset
            .total_value
            .checked_sub(locked_profit)
            .ok_or_else(|| VoltrError::MathOverflow.into())
    }

    pub fn get_total_fee_configuration_performance_fee(&self) -> Result<u16> {
        self.fee_configuration
            .admin_performance_fee
            .checked_add(self.fee_configuration.manager_performance_fee)
            .and_then(|s| s.checked_add(self.fee_configuration.protocol_performance_fee))
            .ok_or_else(|| VoltrError::MathOverflow.into())
    }
}

#[derive(Clone, Debug)]
pub struct VaultAsset {
    pub mint: Pubkey,
    pub idle_ata: Pubkey,
    pub total_value: u64,
    pub idle_ata_auth_bump: u8,
}

impl VaultAsset {
    pub fn load(data: &[u8]) -> Result<Self> {
        Ok(VaultAsset {
            mint: Pubkey::new_from_array(data[0..32].try_into()?),
            idle_ata: Pubkey::new_from_array(data[32..64].try_into()?),
            total_value: u64::from_le_bytes(data[64..72].try_into()?),
            idle_ata_auth_bump: data[72],
        })
    }
}

#[derive(Clone, Debug)]
pub struct VaultLp {
    pub mint: Pubkey,
    pub mint_bump: u8,
    pub mint_auth_bump: u8,
}

impl VaultLp {
    pub fn load(data: &[u8]) -> Result<Self> {
        Ok(VaultLp {
            mint: Pubkey::new_from_array(data[0..32].try_into()?),
            mint_bump: data[32],
            mint_auth_bump: data[33],
        })
    }
}

#[derive(Clone, Debug)]
pub struct VaultConfiguration {
    pub max_cap: u64,
    pub start_at_ts: u64,
    pub locked_profit_degradation_duration: u64,
    pub withdrawal_waiting_period: u64,
    pub disabled_operations: u16,
}

impl VaultConfiguration {
    pub fn load(data: &[u8]) -> Result<Self> {
        Ok(VaultConfiguration {
            max_cap: u64::from_le_bytes(data[0..8].try_into()?),
            start_at_ts: u64::from_le_bytes(data[8..16].try_into()?),
            locked_profit_degradation_duration: u64::from_le_bytes(data[16..24].try_into()?),
            withdrawal_waiting_period: u64::from_le_bytes(data[24..32].try_into()?),
            disabled_operations: u16::from_le_bytes(data[32..34].try_into()?),
        })
    }
}

#[derive(Clone, Debug)]
pub struct FeeConfiguration {
    pub manager_performance_fee: u16,
    pub admin_performance_fee: u16,
    pub manager_management_fee: u16,
    pub admin_management_fee: u16,
    pub redemption_fee: u16,
    pub issuance_fee: u16,
    pub protocol_performance_fee: u16,
    pub protocol_management_fee: u16,
}

impl FeeConfiguration {
    pub fn load(data: &[u8]) -> Result<Self> {
        Ok(FeeConfiguration {
            manager_performance_fee: u16::from_le_bytes(data[0..2].try_into()?),
            admin_performance_fee: u16::from_le_bytes(data[2..4].try_into()?),
            manager_management_fee: u16::from_le_bytes(data[4..6].try_into()?),
            admin_management_fee: u16::from_le_bytes(data[6..8].try_into()?),
            redemption_fee: u16::from_le_bytes(data[8..10].try_into()?),
            issuance_fee: u16::from_le_bytes(data[10..12].try_into()?),
            protocol_performance_fee: u16::from_le_bytes(data[12..14].try_into()?),
            protocol_management_fee: u16::from_le_bytes(data[14..16].try_into()?),
        })
    }
}

#[derive(Clone, Debug)]
pub struct FeeUpdate {
    pub last_performance_fee_update_ts: u64,
    pub last_management_fee_update_ts: u64,
}

impl FeeUpdate {
    pub fn load(data: &[u8]) -> Result<Self> {
        Ok(FeeUpdate {
            last_performance_fee_update_ts: u64::from_le_bytes(data[0..8].try_into()?),
            last_management_fee_update_ts: u64::from_le_bytes(data[8..16].try_into()?),
        })
    }
}

#[derive(Clone, Debug)]
pub struct FeeState {
    pub accumulated_lp_manager_fees: u64,
    pub accumulated_lp_admin_fees: u64,
    pub accumulated_lp_protocol_fees: u64,
}

impl FeeState {
    pub fn load(data: &[u8]) -> Result<Self> {
        Ok(FeeState {
            accumulated_lp_manager_fees: u64::from_le_bytes(data[0..8].try_into()?),
            accumulated_lp_admin_fees: u64::from_le_bytes(data[8..16].try_into()?),
            accumulated_lp_protocol_fees: u64::from_le_bytes(data[16..24].try_into()?),
        })
    }
}

#[derive(Clone, Debug)]
pub struct HighWaterMark {
    pub highest_asset_per_lp_decimal_bits: u128,
    pub last_updated_ts: u64,
}

impl HighWaterMark {
    pub fn load(data: &[u8]) -> Result<Self> {
        Ok(HighWaterMark {
            highest_asset_per_lp_decimal_bits: u128::from_le_bytes(data[0..16].try_into()?),
            last_updated_ts: u64::from_le_bytes(data[16..24].try_into()?),
        })
    }
}

#[derive(Clone, Debug)]
pub struct LockedProfitState {
    pub last_updated_locked_profit: u64,
    pub last_report: u64,
}

impl LockedProfitState {
    pub fn load(data: &[u8]) -> Result<Self> {
        Ok(LockedProfitState {
            last_updated_locked_profit: u64::from_le_bytes(data[0..8].try_into()?),
            last_report: u64::from_le_bytes(data[8..16].try_into()?),
        })
    }

    pub fn calculate_locked_profit(
        &self,
        locked_profit_degradation_duration: u64,
        current_time: u64,
    ) -> Result<u64> {
        let duration = current_time.saturating_sub(self.last_report) as u128;
        let degradation_duration = locked_profit_degradation_duration as u128;

        if duration > degradation_duration || degradation_duration == 0 {
            return Ok(0);
        }

        let locked_profit = (self.last_updated_locked_profit as u128)
            .checked_mul(degradation_duration.saturating_sub(duration))
            .and_then(|v| v.checked_div(degradation_duration))
            .ok_or(VoltrError::MathOverflow)?;

        Ok(u64::try_from(locked_profit)?)
    }
}
