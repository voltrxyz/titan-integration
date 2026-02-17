use async_trait::async_trait;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_program::system_program::ID as SYSTEM_PROGRAM_ID;
use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use spl_token_2022::{
    extension::StateWithExtensions,
    state::Mint as Mint22,
};

use titan_integration_template::{
    account_caching::AccountsCache,
    trading_venue::{
        error::TradingVenueError, protocol::PoolProtocol, token_info::TokenInfo,
        AddressLookupTableTrait, FromAccount, QuoteRequest, QuoteResult, TradingVenue,
    },
};

use crate::{
    constants::*,
    math::*,
    state::Vault,
};

/// Compute Anchor's 8-byte instruction discriminator for a given method name.
fn anchor_discriminator(name: &str) -> [u8; 8] {
    let preimage = format!("global:{}", name);
    let mut sighash = [0u8; 8];
    sighash.copy_from_slice(&solana_sdk::hash::hash(preimage.as_bytes()).to_bytes()[..8]);
    sighash
}

/// Number of accounts in the first instruction (`request_withdraw_vault`)
/// when the returned redeem instruction is split into two.
pub const REDEEM_SPLIT_INDEX: usize = 11;

/// Titan-compatible trading venue for Voltr yield vaults.
///
/// Voltr vaults accept deposits of an underlying asset and issue LP tokens
/// representing a share of the vault's total value. This venue supports:
///
/// - **Deposits** (asset -> LP): `generate_swap_instruction()` returns a real
///   `deposit_vault` instruction that can be submitted directly.
/// - **Redeems** (LP -> asset): `generate_swap_instruction()` returns a **dummy**
///   instruction whose `accounts` field contains the account metas for **two**
///   on-chain instructions that must be executed atomically in the same transaction:
///
///   1. `request_withdraw_vault` — accounts `[0..REDEEM_SPLIT_INDEX]` (first 11)
///   2. `withdraw_vault`         — accounts `[REDEEM_SPLIT_INDEX..]`  (remaining 13)
///
///   The instruction `data` field is empty for the dummy; see the per-instruction
///   data formats documented on `build_redeem_dummy_instruction()`.
#[derive(Clone)]
pub struct VoltrVaultVenue {
    pub vault_key: Pubkey,
    pub vault_state: Vault,
    pub lp_mint_supply: u64,
    pub lp_mint_decimals: u8,
    pub asset_mint_decimals: u8,
    pub asset_token_program: Pubkey,
    pub asset_idle_balance: u64,
    token_info: Vec<TokenInfo>,
    initialized: bool,
}

impl VoltrVaultVenue {
    pub fn new(vault_key: Pubkey, vault_state: Vault) -> Self {
        Self {
            vault_key,
            vault_state,
            lp_mint_supply: 0,
            lp_mint_decimals: 9, // Voltr LP is always 9 decimals
            asset_mint_decimals: 0,
            asset_token_program: TOKEN_PROGRAM,
            asset_idle_balance: 0,
            token_info: Vec::new(),
            initialized: false,
        }
    }

    /// Estimate management-fee LP tokens that would be minted at `current_ts`.
    fn estimate_management_fee_lp(
        &self,
        current_ts: u64,
        total_asset_value: u64,
        total_lp_supply_incl_fees: u64,
    ) -> Result<u64, TradingVenueError> {
        let management_fee_bps = self
            .vault_state
            .get_total_fee_configuration_management_fee()
            .map_err(|e: anyhow::Error| TradingVenueError::CheckedMathError(e.to_string().into()))?;

        if self.vault_state.fee_update.last_management_fee_update_ts == 0
            || total_asset_value == 0
            || management_fee_bps == 0
        {
            return Ok(0);
        }

        let time_elapsed = current_ts
            .saturating_sub(self.vault_state.fee_update.last_management_fee_update_ts);
        if time_elapsed == 0 {
            return Ok(0);
        }

        let fee_amount_in_asset =
            calc_management_fee_amount_in_asset(time_elapsed, total_asset_value, management_fee_bps)
                .map_err(|e: anyhow::Error| TradingVenueError::CheckedMathError(e.to_string().into()))?;

        if fee_amount_in_asset == 0 || fee_amount_in_asset >= total_asset_value {
            return Ok(0);
        }

        calc_fee_lp_to_mint(fee_amount_in_asset, total_lp_supply_incl_fees, total_asset_value)
            .map_err(|e: anyhow::Error| TradingVenueError::CheckedMathError(e.to_string().into()))
    }

    /// Compute a redeem quote (LP -> asset).
    fn quote_redeem(
        &self,
        request: &QuoteRequest,
        current_ts: u64,
        total_lp_supply_after_mgmt_fee: u64,
    ) -> Result<QuoteResult, TradingVenueError> {
        if self
            .vault_state
            .vault_configuration
            .withdrawal_waiting_period
            != 0
        {
            return Err(TradingVenueError::AmmMethodError(
                "Withdrawal waiting period must be zero for instant redeems".into(),
            ));
        }

        let amount = request.amount;
        let redemption_fee_bps = self.vault_state.fee_configuration.redemption_fee;

        let total_unlocked_asset = self
            .vault_state
            .get_unlocked_asset_value(current_ts)
            .map_err(|e: anyhow::Error| TradingVenueError::CheckedMathError(e.to_string().into()))?;

        let asset_to_redeem = calc_withdraw_asset_to_redeem(
            amount,
            total_lp_supply_after_mgmt_fee,
            total_unlocked_asset,
            redemption_fee_bps,
        )
        .map_err(|e: anyhow::Error| TradingVenueError::CheckedMathError(e.to_string().into()))?;

        if self.asset_idle_balance < asset_to_redeem {
            return Ok(QuoteResult {
                input_mint: request.input_mint,
                output_mint: request.output_mint,
                amount,
                expected_output: 0,
                not_enough_liquidity: true,
            });
        }

        Ok(QuoteResult {
            input_mint: request.input_mint,
            output_mint: request.output_mint,
            amount,
            expected_output: asset_to_redeem,
            not_enough_liquidity: false,
        })
    }

    /// Build the `deposit_vault` instruction for a deposit (asset -> LP).
    fn build_deposit_instruction(
        &self,
        deposit_amount: u64,
        user: &Pubkey,
    ) -> Result<Instruction, TradingVenueError> {
        let (protocol_pda, _) =
            Pubkey::find_program_address(&[PROTOCOL_SEED], &VOLTR_VAULT_PROGRAM);

        let (vault_lp_mint_pda, _) = Pubkey::find_program_address(
            &[VAULT_LP_MINT_SEED, self.vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        );

        let (vault_asset_idle_auth_pda, _) = Pubkey::find_program_address(
            &[VAULT_ASSET_IDLE_AUTH_SEED, self.vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        );

        let (vault_lp_mint_auth_pda, _) = Pubkey::find_program_address(
            &[VAULT_LP_MINT_AUTH_SEED, self.vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        );

        let user_source_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
            user,
            &self.vault_state.asset.mint,
            &self.asset_token_program,
        );

        let user_dest_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
            user,
            &vault_lp_mint_pda,
            &TOKEN_PROGRAM,
        );

        let accounts = vec![
            AccountMeta::new_readonly(*user, true),
            AccountMeta::new_readonly(protocol_pda, false),
            AccountMeta::new(self.vault_key, false),
            AccountMeta::new_readonly(self.vault_state.asset.mint, false),
            AccountMeta::new(vault_lp_mint_pda, false),
            AccountMeta::new(user_source_ata, false),
            AccountMeta::new(self.vault_state.asset.idle_ata, false),
            AccountMeta::new_readonly(vault_asset_idle_auth_pda, false),
            AccountMeta::new(user_dest_ata, false),
            AccountMeta::new_readonly(vault_lp_mint_auth_pda, false),
            AccountMeta::new_readonly(self.asset_token_program, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ];

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&anchor_discriminator("deposit_vault"));
        data.extend_from_slice(&deposit_amount.to_le_bytes());

        Ok(Instruction {
            program_id: VOLTR_VAULT_PROGRAM,
            accounts,
            data,
        })
    }

    /// Build a dummy `Instruction` for Voltr vault redeems (LP -> asset).
    ///
    /// The returned instruction is **not** executable as-is. Its `accounts` field
    /// contains the account metas for **two** on-chain instructions, concatenated:
    ///
    /// ## Splitting into real instructions
    ///
    /// ```text
    /// accounts[0..REDEEM_SPLIT_INDEX]   → request_withdraw_vault  (first 11 accounts)
    /// accounts[REDEEM_SPLIT_INDEX..]    → withdraw_vault           (remaining 13 accounts)
    /// ```
    ///
    /// Both must be submitted atomically in the same transaction.
    ///
    /// ## Instruction data formats
    ///
    /// **`request_withdraw_vault`** (program_id = `VOLTR_VAULT_PROGRAM`):
    /// ```text
    /// [0..8]   anchor discriminator for "request_withdraw_vault"
    /// [8..16]  lp_amount: u64 (little-endian)
    /// [16]     is_amount_in_lp: u8 = 1
    /// [17]     is_withdraw_all: u8 = 0
    /// ```
    ///
    /// **`withdraw_vault`** (program_id = `VOLTR_VAULT_PROGRAM`):
    /// ```text
    /// [0..8]   anchor discriminator for "withdraw_vault"
    /// ```
    fn build_redeem_dummy_instruction(
        &self,
        user: &Pubkey,
    ) -> Result<Instruction, TradingVenueError> {
        let (protocol_pda, _) =
            Pubkey::find_program_address(&[PROTOCOL_SEED], &VOLTR_VAULT_PROGRAM);

        let (vault_lp_mint_pda, _) = Pubkey::find_program_address(
            &[VAULT_LP_MINT_SEED, self.vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        );

        let (vault_asset_idle_auth_pda, _) = Pubkey::find_program_address(
            &[VAULT_ASSET_IDLE_AUTH_SEED, self.vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        );

        let (receipt_pda, _) = Pubkey::find_program_address(
            &[
                REQUEST_WITHDRAW_VAULT_RECEIPT_SEED,
                self.vault_key.as_ref(),
                user.as_ref(),
            ],
            &VOLTR_VAULT_PROGRAM,
        );

        let receipt_lp_ata = Pubkey::find_program_address(
            &[
                receipt_pda.as_ref(),
                TOKEN_PROGRAM.as_ref(),
                vault_lp_mint_pda.as_ref(),
            ],
            &ATA_PROGRAM,
        )
        .0;

        let user_lp_ata =
            spl_associated_token_account::get_associated_token_address_with_program_id(
                user,
                &vault_lp_mint_pda,
                &TOKEN_PROGRAM,
            );

        let user_asset_ata =
            spl_associated_token_account::get_associated_token_address_with_program_id(
                user,
                &self.vault_state.asset.mint,
                &self.asset_token_program,
            );

        let mut accounts = Vec::with_capacity(24);

        // --- request_withdraw_vault accounts (indices 0..11) ---
        accounts.push(AccountMeta::new(*user, true));              // 0  payer (signer, writable)
        accounts.push(AccountMeta::new_readonly(*user, true));     // 1  user_transfer_authority (signer)
        accounts.push(AccountMeta::new_readonly(protocol_pda, false)); // 2  protocol PDA
        accounts.push(AccountMeta::new_readonly(self.vault_key, false)); // 3  vault
        accounts.push(AccountMeta::new_readonly(vault_lp_mint_pda, false)); // 4  vault LP mint
        accounts.push(AccountMeta::new(user_lp_ata, false));       // 5  user LP ATA (source)
        accounts.push(AccountMeta::new(receipt_lp_ata, false));    // 6  receipt LP ATA (init_if_needed)
        accounts.push(AccountMeta::new(receipt_pda, false));       // 7  receipt PDA
        accounts.push(AccountMeta::new_readonly(ATA_PROGRAM, false)); // 8  associated token program
        accounts.push(AccountMeta::new_readonly(TOKEN_PROGRAM, false)); // 9  lp token program
        accounts.push(AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false)); // 10 system program

        // --- withdraw_vault accounts (indices 11..24) ---
        accounts.push(AccountMeta::new(*user, true));              // 11 user (signer, writable)
        accounts.push(AccountMeta::new_readonly(protocol_pda, false)); // 12 protocol PDA
        accounts.push(AccountMeta::new(self.vault_key, false));    // 13 vault (writable)
        accounts.push(AccountMeta::new_readonly(self.vault_state.asset.mint, false)); // 14 asset mint
        accounts.push(AccountMeta::new(vault_lp_mint_pda, false)); // 15 vault LP mint (writable)
        accounts.push(AccountMeta::new(receipt_lp_ata, false));    // 16 receipt LP ATA (writable)
        accounts.push(AccountMeta::new(self.vault_state.asset.idle_ata, false)); // 17 idle ATA (writable)
        accounts.push(AccountMeta::new(vault_asset_idle_auth_pda, false)); // 18 idle auth PDA (writable)
        accounts.push(AccountMeta::new(user_asset_ata, false));    // 19 user asset ATA (writable)
        accounts.push(AccountMeta::new(receipt_pda, false));       // 20 receipt PDA (writable)
        accounts.push(AccountMeta::new_readonly(self.asset_token_program, false)); // 21 asset token program
        accounts.push(AccountMeta::new_readonly(TOKEN_PROGRAM, false)); // 22 lp token program
        accounts.push(AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false)); // 23 system program

        Ok(Instruction {
            program_id: VOLTR_VAULT_PROGRAM,
            accounts,
            data: vec![], // dummy — see doc comment for per-instruction data formats
        })
    }

    /// Derive the receipt PDA for a given vault and user.
    pub fn derive_receipt_pda(vault_key: &Pubkey, user: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[
                REQUEST_WITHDRAW_VAULT_RECEIPT_SEED,
                vault_key.as_ref(),
                user.as_ref(),
            ],
            &VOLTR_VAULT_PROGRAM,
        )
        .0
    }

    /// Derive the vault LP mint PDA.
    pub fn derive_vault_lp_mint_pda(vault_key: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[VAULT_LP_MINT_SEED, vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        )
        .0
    }
}

impl FromAccount for VoltrVaultVenue {
    fn from_account(pubkey: &Pubkey, account: &Account) -> Result<Self, TradingVenueError> {
        let vault_state = Vault::load(&account.data)
            .map_err(|e: anyhow::Error| TradingVenueError::DeserializationFailed(e.to_string().into()))?;
        Ok(VoltrVaultVenue::new(*pubkey, vault_state))
    }
}

#[async_trait]
impl TradingVenue for VoltrVaultVenue {
    fn initialized(&self) -> bool {
        self.initialized
    }

    fn program_id(&self) -> Pubkey {
        VOLTR_VAULT_PROGRAM
    }

    fn program_dependencies(&self) -> Vec<Pubkey> {
        vec![
            VOLTR_VAULT_PROGRAM,
            TOKEN_PROGRAM,
            TOKEN_22_PROGRAM,
            ATA_PROGRAM,
        ]
    }

    fn market_id(&self) -> Pubkey {
        self.vault_key
    }

    fn protocol(&self) -> PoolProtocol {
        PoolProtocol::VoltrVault
    }

    fn get_token_info(&self) -> &[TokenInfo] {
        &self.token_info
    }

    fn get_required_pubkeys_for_update(&self) -> Result<Vec<Pubkey>, TradingVenueError> {
        Ok(vec![
            self.vault_key,
            self.vault_state.lp.mint,
            self.vault_state.asset.mint,
            self.vault_state.asset.idle_ata,
        ])
    }

    async fn update_state(&mut self, cache: &dyn AccountsCache) -> Result<(), TradingVenueError> {
        let pubkeys = vec![
            self.vault_key,
            self.vault_state.lp.mint,
            self.vault_state.asset.mint,
            self.vault_state.asset.idle_ata,
        ];

        let accounts = cache.get_accounts(&pubkeys).await?;

        // Parse vault state
        let vault_account = accounts[0]
            .as_ref()
            .ok_or(TradingVenueError::NoAccountFound(self.vault_key.into()))?;
        self.vault_state = Vault::load(&vault_account.data)
            .map_err(|e: anyhow::Error| TradingVenueError::DeserializationFailed(e.to_string().into()))?;

        // Parse LP mint
        let lp_mint_account = accounts[1]
            .as_ref()
            .ok_or(TradingVenueError::NoAccountFound(
                self.vault_state.lp.mint.into(),
            ))?;
        let lp_mint = spl_token::state::Mint::unpack(&lp_mint_account.data)
            .map_err(|e| TradingVenueError::DeserializationFailed(e.to_string().into()))?;
        self.lp_mint_supply = lp_mint.supply;
        self.lp_mint_decimals = lp_mint.decimals;

        // Parse asset mint (supports both Token and Token-2022)
        let asset_mint_account = accounts[2]
            .as_ref()
            .ok_or(TradingVenueError::NoAccountFound(
                self.vault_state.asset.mint.into(),
            ))?;
        self.asset_token_program = asset_mint_account.owner;

        if asset_mint_account.owner == TOKEN_PROGRAM {
            let mint = spl_token::state::Mint::unpack(&asset_mint_account.data)
                .map_err(|e| TradingVenueError::DeserializationFailed(e.to_string().into()))?;
            self.asset_mint_decimals = mint.decimals;
        } else {
            let mint = StateWithExtensions::<Mint22>::unpack(&asset_mint_account.data)
                .map_err(|e| TradingVenueError::DeserializationFailed(e.to_string().into()))?;
            self.asset_mint_decimals = mint.base.decimals;
        }

        // Parse idle ATA balance
        let idle_ata_account = accounts[3]
            .as_ref()
            .ok_or(TradingVenueError::NoAccountFound(
                self.vault_state.asset.idle_ata.into(),
            ))?;

        if self.asset_token_program == TOKEN_PROGRAM {
            let idle = spl_token::state::Account::unpack(&idle_ata_account.data)
                .map_err(|e| TradingVenueError::DeserializationFailed(e.to_string().into()))?;
            self.asset_idle_balance = idle.amount;
        } else {
            let idle = StateWithExtensions::<spl_token_2022::state::Account>::unpack(
                &idle_ata_account.data,
            )
            .map_err(|e| TradingVenueError::DeserializationFailed(e.to_string().into()))?;
            self.asset_idle_balance = idle.base.amount;
        }

        // Build token info
        self.token_info = vec![
            TokenInfo::new(
                &self.vault_state.asset.mint,
                asset_mint_account,
                u64::MAX,
            )?,
            TokenInfo::new(&self.vault_state.lp.mint, lp_mint_account, u64::MAX)?,
        ];

        self.initialized = true;
        Ok(())
    }

    fn quote(&self, request: QuoteRequest) -> Result<QuoteResult, TradingVenueError> {
        let asset_mint = self.vault_state.asset.mint;
        let lp_mint = self.vault_state.lp.mint;

        let is_deposit = request.input_mint == asset_mint && request.output_mint == lp_mint;
        let is_redeem = request.input_mint == lp_mint && request.output_mint == asset_mint;

        if !is_deposit && !is_redeem {
            return Err(TradingVenueError::InvalidMint(request.input_mint.into()));
        }

        // Handle zero input without error (required by Titan)
        if request.amount == 0 {
            return Ok(QuoteResult {
                input_mint: request.input_mint,
                output_mint: request.output_mint,
                amount: 0,
                expected_output: 0,
                not_enough_liquidity: false,
            });
        }

        let total_asset_value = self.vault_state.get_total_asset_value();
        let total_lp_supply_incl_fees = self
            .vault_state
            .get_total_lp_supply_incl_fees(self.lp_mint_supply)
            .map_err(|e: anyhow::Error| TradingVenueError::CheckedMathError(e.to_string().into()))?;

        let current_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(self.vault_state.last_updated_ts);

        let mgmt_fee_lp = self.estimate_management_fee_lp(
            current_ts,
            total_asset_value,
            total_lp_supply_incl_fees,
        )?;

        let total_lp_supply_after_mgmt_fee = total_lp_supply_incl_fees
            .checked_add(mgmt_fee_lp)
            .ok_or_else(|| TradingVenueError::CheckedMathError(
                "LP supply overflow after management fee".into(),
            ))?;

        // --- Redeem path (LP -> asset) ---
        if is_redeem {
            return self.quote_redeem(&request, current_ts, total_lp_supply_after_mgmt_fee);
        }

        // --- Deposit path (asset -> LP) ---
        let amount = request.amount;

        // Enforce vault max cap: if max_cap > 0, the deposit must not push
        // total asset value above the configured ceiling.
        let max_cap = self.vault_state.vault_configuration.max_cap;
        if max_cap > 0 {
            let new_total = total_asset_value.saturating_add(amount);
            if new_total > max_cap {
                return Ok(QuoteResult {
                    input_mint: request.input_mint,
                    output_mint: request.output_mint,
                    amount,
                    expected_output: 0,
                    not_enough_liquidity: true,
                });
            }
        }

        let issuance_fee_bps = self.vault_state.fee_configuration.issuance_fee;

        let lp_before_deadweight = if total_lp_supply_incl_fees == 0 {
            calc_init_lp_to_mint(amount, self.asset_mint_decimals, self.lp_mint_decimals)
                .map_err(|e: anyhow::Error| TradingVenueError::CheckedMathError(e.to_string().into()))?
        } else {
            calc_deposit_lp_to_mint(
                amount,
                total_lp_supply_after_mgmt_fee,
                total_asset_value,
                issuance_fee_bps,
            )
            .map_err(|e: anyhow::Error| TradingVenueError::CheckedMathError(e.to_string().into()))?
        };

        let lp_to_mint = if self.vault_state.dead_weight == 0 {
            if lp_before_deadweight < DEAD_WEIGHT {
                return Ok(QuoteResult {
                    input_mint: request.input_mint,
                    output_mint: request.output_mint,
                    amount,
                    expected_output: 0,
                    not_enough_liquidity: true,
                });
            }
            lp_before_deadweight.saturating_sub(DEAD_WEIGHT)
        } else {
            lp_before_deadweight
        };

        Ok(QuoteResult {
            input_mint: request.input_mint,
            output_mint: request.output_mint,
            amount,
            expected_output: lp_to_mint,
            not_enough_liquidity: false,
        })
    }

    fn generate_swap_instruction(
        &self,
        request: QuoteRequest,
        user: Pubkey,
    ) -> Result<Instruction, TradingVenueError> {
        let asset_mint = self.vault_state.asset.mint;
        let lp_mint = self.vault_state.lp.mint;

        let is_deposit = request.input_mint == asset_mint && request.output_mint == lp_mint;
        let is_redeem = request.input_mint == lp_mint && request.output_mint == asset_mint;

        if !is_deposit && !is_redeem {
            return Err(TradingVenueError::InvalidMint(request.input_mint.into()));
        }

        if is_redeem {
            return self.build_redeem_dummy_instruction(&user);
        }

        self.build_deposit_instruction(request.amount, &user)
    }
}

#[async_trait]
impl AddressLookupTableTrait for VoltrVaultVenue {
    async fn get_lookup_table_keys(
        &self,
        _accounts_cache: Option<&dyn AccountsCache>,
    ) -> Result<Vec<Pubkey>, TradingVenueError> {
        let (protocol_pda, _) =
            Pubkey::find_program_address(&[PROTOCOL_SEED], &VOLTR_VAULT_PROGRAM);

        let (vault_lp_mint_pda, _) = Pubkey::find_program_address(
            &[VAULT_LP_MINT_SEED, self.vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        );

        let (vault_asset_idle_auth_pda, _) = Pubkey::find_program_address(
            &[VAULT_ASSET_IDLE_AUTH_SEED, self.vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        );

        let (vault_lp_mint_auth_pda, _) = Pubkey::find_program_address(
            &[VAULT_LP_MINT_AUTH_SEED, self.vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        );

        Ok(vec![
            VOLTR_VAULT_PROGRAM,
            self.vault_key,
            self.vault_state.asset.mint,
            vault_lp_mint_pda,
            self.vault_state.asset.idle_ata,
            vault_asset_idle_auth_pda,
            vault_lp_mint_auth_pda,
            protocol_pda,
            self.asset_token_program,
            TOKEN_PROGRAM,
        ])
    }
}
