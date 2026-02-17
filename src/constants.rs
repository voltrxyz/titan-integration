use solana_pubkey::Pubkey;

pub const VOLTR_VAULT_PROGRAM: Pubkey =
    Pubkey::from_str_const("vVoLTRjQmtFpiYoegx285Ze4gsLJ8ZxgFKVcuvmG1a8");

pub const TOKEN_PROGRAM: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_22_PROGRAM: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

pub const PROTOCOL_SEED: &[u8] = b"protocol";
pub const VAULT_LP_MINT_SEED: &[u8] = b"vault_lp_mint";
pub const VAULT_LP_MINT_AUTH_SEED: &[u8] = b"vault_lp_mint_auth";
pub const VAULT_ASSET_IDLE_AUTH_SEED: &[u8] = b"vault_asset_idle_auth";
pub const REQUEST_WITHDRAW_VAULT_RECEIPT_SEED: &[u8] = b"request_withdraw_vault_receipt";

pub const ATA_PROGRAM: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

pub const MAX_FEE_BPS: u16 = 10_000;
pub const ONE_YEAR_U64: u64 = 365 * 24 * 60 * 60;
pub const DEAD_WEIGHT: u64 = 1_000;
