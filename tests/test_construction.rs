#[cfg(test)]
mod test_construction {
    //! Integration test ensuring that a Voltr vault venue:
    //! - can be constructed from on-chain account data,
    //! - can load its required state via the AccountsCache,
    //! - returns valid token info,
    //! - supports quoting for both swap directions,
    //! - and exposes sane quoting boundaries.

    use std::{env, str::FromStr};

    use rstest::rstest;
    use solana_pubkey::Pubkey;
    use titan_integration_template::account_caching::rpc_cache::RpcClientCache;
    use titan_integration_template::trading_venue::{QuoteRequest, SwapType};
    use titan_integration_template::trading_venue::{FromAccount, TradingVenue};

    use titan_voltr_integration::voltr_venue::VoltrVaultVenue;

    use solana_client::nonblocking::rpc_client::RpcClient;

    use assert_no_alloc::*;

    #[cfg(debug_assertions)] // required when disable_release is set (default)
    #[global_allocator]
    static A: AllocDisabler = AllocDisabler;

    /// Initialize logging for test output.
    fn init_test_logger() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    /// Ensure that the venue can:
    /// - Build from a raw on-chain account,
    /// - Perform a state update using the caching layer,
    /// - Report valid token metadata,
    /// - Calculate valid quoting boundaries for both directions,
    /// - Return nonzero, liquidity-supported quotes at both boundary edges.
    #[rstest]
    #[tokio::test]
    #[case("GqoypwVGG35JSR1AwCm2jeqJPUPvA4cWE45rSbfxHgdK")]
    async fn test_construction(#[case] vault_key: String) {
        init_test_logger();

        //
        // Prepare inputs
        //
        let vault_key = Pubkey::from_str(&vault_key).expect("Invalid test pubkey");

        let rpc_url =
            env::var("SOLANA_RPC_URL").expect("SOLANA_RPC_URL must be set for integration tests");
        let rpc = RpcClient::new(rpc_url);

        //
        // Fetch the vault account and construct the venue
        //
        let vault_account = rpc
            .get_account(&vault_key)
            .await
            .expect("Failed to fetch vault account");

        let mut venue = VoltrVaultVenue::from_account(&vault_key, &vault_account)
            .expect("Failed to construct venue from account");

        //
        // Load on-chain state using the caching layer
        //
        let cache = RpcClientCache::new(rpc);
        venue
            .update_state(&cache)
            .await
            .expect("Venue state update failed");

        //
        // Validate token metadata
        //
        let token_info = venue.get_token_info();
        log::info!("Loaded token info: {:#?}", token_info);
        assert!(token_info.len() > 0);

        // Voltr vaults always have 2 tokens (asset + LP).
        assert_eq!(token_info.len(), 2);

        //
        // For each direction (deposit: asset→LP, redeem: LP→asset)
        // validate quoting boundaries and quote correctness.
        //
        for (input_idx, output_idx) in [(0, 1), (1, 0)] {
            let (lower_bound, upper_bound) =
                assert_no_alloc(|| venue.bounds(input_idx, output_idx))
                    .expect("Boundary search failed");

            assert!(
                lower_bound < upper_bound,
                "Lower bound must be strictly less than upper bound"
            );

            let input_mint = token_info[input_idx as usize].pubkey;
            let output_mint = token_info[output_idx as usize].pubkey;

            let lb_result = assert_no_alloc(|| {
                venue.quote(QuoteRequest {
                    input_mint,
                    output_mint,
                    amount: lower_bound,
                    swap_type: SwapType::ExactIn,
                })
            })
            .expect("Lower-bound quote failed");

            log::info!("Lower-bound quote: {:#?}", lb_result);

            assert!(
                !lb_result.not_enough_liquidity,
                "Lower bound indicates insufficient liquidity"
            );
            assert!(
                lb_result.expected_output > 0,
                "Lower bound produced zero output"
            );

            let ub_result = assert_no_alloc(|| {
                venue.quote(QuoteRequest {
                    input_mint,
                    output_mint,
                    amount: upper_bound,
                    swap_type: SwapType::ExactIn,
                })
            })
            .expect("Upper-bound quote failed");

            log::info!("Upper-bound quote: {:#?}", ub_result);

            assert!(
                !ub_result.not_enough_liquidity,
                "Upper bound indicates insufficient liquidity"
            );
            assert!(
                ub_result.expected_output > 0,
                "Upper bound produced zero output"
            );

            //
            // Zero-input quote (Titan requirement: must not error)
            //
            let zero_result = assert_no_alloc(|| {
                venue.quote(QuoteRequest {
                    input_mint,
                    output_mint,
                    amount: 0,
                    swap_type: SwapType::ExactIn,
                })
            })
            .expect("Zero-input quote should not error");

            assert_eq!(
                zero_result.expected_output, 0,
                "Zero input should produce zero output"
            );
            assert!(
                !zero_result.not_enough_liquidity,
                "Zero input should not flag liquidity issues"
            );
        }
    }
}
