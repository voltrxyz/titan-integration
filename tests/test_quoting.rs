#[cfg(test)]
mod simulations {
    //! Quoting tests for the Voltr vault venue.
    //!
    //! The tests ensure:
    //! - The venue loads on-chain state correctly
    //! - It exposes valid token info
    //! - It establishes valid quoting boundaries for both swap directions
    //! - Its off-chain quote matches on-chain execution on and off the boundaries
    //! - Its quoting function is monotone increasing for both directions
    //! - Its quoting speed is sufficient for integration

    use litesvm::LiteSVM;
    use rand::Rng;
    use rstest::rstest;

    use solana_account::Account;
    use solana_account::ReadableAccount;
    use solana_account::WritableAccount;
    use solana_client::nonblocking::rpc_client::RpcClient;
    use solana_compute_budget::compute_budget::ComputeBudget;
    use solana_program::native_token::LAMPORTS_PER_SOL;
    use solana_program_pack::Pack;
    use solana_pubkey::Pubkey;
    use solana_sdk::signature::Keypair;
    use solana_sdk::signer::Signer;
    use solana_sysvar::clock::{self, Clock};
    use solana_transaction::Transaction;
    use std::env;
    use std::str::FromStr;
    use std::time::Instant;

    use spl_associated_token_account::get_associated_token_address_with_program_id;
    use spl_token::state::{Account as TokenAccount, AccountState};

    use titan_integration_template::account_caching::rpc_cache::RpcClientCache;
    use titan_integration_template::account_caching::AccountsCache;
    use titan_integration_template::trading_venue::error::TradingVenueError;
    use titan_integration_template::trading_venue::{
        FromAccount, QuoteRequest, SwapType, TradingVenue,
    };

    use titan_voltr_integration::constants::VOLTR_VAULT_PROGRAM;
    use titan_voltr_integration::voltr_venue::VoltrVaultVenue;

    /// Initialize logging for test diagnostics.
    fn init_test_logger() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    /// Creates a new LiteSVM instance configured with:
    /// - The Voltr vault program loaded from `programs/voltr_vault.so`
    /// - A funded system account for signing transactions
    fn setup_litesvm() -> (LiteSVM, Keypair) {
        let mut litesvm = LiteSVM::new()
            .with_compute_budget(ComputeBudget {
                compute_unit_limit: 1_400_000,
                ..Default::default()
            })
            .with_blockhash_check(false)
            .with_sigverify(false)
            .with_transaction_history(0);

        // Load the Voltr vault program binary
        litesvm
            .add_program_from_file(VOLTR_VAULT_PROGRAM, "programs/voltr_vault.so")
            .unwrap();

        // Create a funded user wallet
        let keypair = Keypair::new();
        let account = Account {
            lamports: 10_000 * LAMPORTS_PER_SOL,
            data: vec![],
            owner: solana_sdk::system_program::id(),
            executable: false,
            rent_epoch: 0,
        };
        litesvm
            .set_account(keypair.pubkey(), account.into())
            .unwrap();

        (litesvm, keypair)
    }

    /// Simulate a deposit using LiteSVM and return the output amount of LP tokens.
    /// This should give the true on-chain output for that deposit.
    async fn sim_quote_request(
        venue: &dyn TradingVenue,
        cache: &dyn AccountsCache,
        request: QuoteRequest,
        litesvm: &mut LiteSVM,
        keypair: &Keypair,
    ) -> u64 {
        let tradable_mints = venue.get_token_info();

        // Identify input/output tokens
        let idx_0 = tradable_mints
            .iter()
            .position(|x| x.pubkey == request.input_mint)
            .unwrap();
        let idx_1 = (idx_0 + 1) % 2;

        let (token_a, token_a_program) = (
            tradable_mints[idx_0].pubkey,
            tradable_mints[idx_0].get_token_program(),
        );
        let (token_b, token_b_program) = (
            tradable_mints[idx_1].pubkey,
            tradable_mints[idx_1].get_token_program(),
        );

        let token_account_a = get_associated_token_address_with_program_id(
            &keypair.pubkey(),
            &token_a,
            &token_a_program,
        );
        let token_account_b = get_associated_token_address_with_program_id(
            &keypair.pubkey(),
            &token_b,
            &token_b_program,
        );

        //
        // Create synthetic token accounts inside the simulator
        //

        // Token A account (source)
        let mut account_a = Account::new(LAMPORTS_PER_SOL, TokenAccount::LEN, &spl_token::ID);
        let mut account_a_data = TokenAccount::default();
        account_a_data.mint = token_a;
        account_a_data.owner = keypair.pubkey();
        account_a_data.state = AccountState::Initialized;
        account_a_data.amount = u64::MAX; // ensure "infinite" input
        account_a_data.pack_into_slice(account_a.data_as_mut_slice());

        // Token B account (destination)
        let mut account_b = Account::new(LAMPORTS_PER_SOL, TokenAccount::LEN, &spl_token::ID);
        let mut account_b_data = TokenAccount::default();
        account_b_data.mint = token_b;
        account_b_data.owner = keypair.pubkey();
        account_b_data.state = AccountState::Initialized;
        account_b_data.amount = 0;
        account_b_data.pack_into_slice(account_b.data_as_mut_slice());

        // Load token accounts into LiteSVM
        litesvm.set_account(token_account_a, account_a).unwrap();
        litesvm.set_account(token_account_b, account_b).unwrap();

        //
        // Build the swap instruction
        //
        let ix = venue
            .generate_swap_instruction(request, keypair.pubkey())
            .unwrap();

        // Load all instruction accounts into SVM (except executable ones already present)
        let pks: Vec<Pubkey> = ix.accounts.iter().map(|acc| acc.pubkey).collect();
        let accounts_to_load = cache.get_accounts(&pks).await.unwrap();
        for (account, key) in accounts_to_load.iter().zip(pks) {
            if let Some(acc) = account {
                if acc.executable {
                    continue;
                }
                litesvm.set_account(key, acc.clone()).unwrap();
            }
        }

        //
        // Execute swap inside the SIM
        //
        let blockhash = litesvm.latest_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&keypair.pubkey()),
            &[keypair],
            blockhash,
        );

        let simulation_result = litesvm.simulate_transaction(tx).unwrap();

        //
        // Read output account and extract the final token amount
        //
        let account_b = simulation_result
            .post_accounts
            .into_iter()
            .find(|(pk, _)| pk == &token_account_b)
            .map(|(_, acc)| acc)
            .unwrap();
        let post_b = TokenAccount::unpack_from_slice(account_b.data())
            .expect("Failed to unpack token B account");
        post_b.amount
    }

    /// Returns a log-uniformly sampled u64 in `[lo, hi]`.
    fn sample_log_uniform_u64(lo: u64, hi: u64) -> u64 {
        assert!(lo >= 1, "log-uniform sampling requires lo >= 1");
        assert!(lo <= hi);

        let lo_f = lo as f64;
        let hi_f = hi as f64;

        let log_lo = lo_f.ln();
        let log_hi = hi_f.ln();

        let r: f64 = rand::rng().random();
        let log_val = log_lo + r * (log_hi - log_lo);

        (log_val.exp() as u64).clamp(lo, hi)
    }

    // -------------------------------------------------------------------------
    // Test 1: check boundary values in simulation
    // -------------------------------------------------------------------------

    #[rstest]
    #[tokio::test]
    #[case("GqoypwVGG35JSR1AwCm2jeqJPUPvA4cWE45rSbfxHgdK")]
    async fn test_bound_simulation(#[case] vault_key: Pubkey) {
        init_test_logger();

        // Fetch live vault data from RPC
        let rpc_url = env::var("SOLANA_RPC_URL").unwrap();
        let rpc = RpcClient::new(rpc_url);
        let vault_account = rpc.get_account(&vault_key).await.unwrap();

        // Build venue + load state
        let cache = RpcClientCache::new(rpc);
        let mut venue = VoltrVaultVenue::from_account(&vault_key, &vault_account).unwrap();
        venue.update_state(&cache).await.unwrap();

        // Setup simulation VM
        let (mut litesvm, keypair) = setup_litesvm();

        // Sync sysvar clock to real network
        let latest_clock = cache.get_account(&clock::ID).await.unwrap();
        let latest_clock: Clock = latest_clock
            .as_ref()
            .ok_or(TradingVenueError::NoAccountFound(clock::ID.into()))
            .unwrap()
            .deserialize_data()
            .unwrap();
        litesvm.set_sysvar::<Clock>(&latest_clock);

        // Ensure valid token set
        let tradable_mints = venue.get_token_info();
        assert_eq!(tradable_mints.len(), 2);

        // Deposit direction: asset (0) -> LP (1)
        let (in_idx, out_idx) = (0, 1);
        let (lower, upper) = venue.bounds(in_idx as u8, out_idx as u8).unwrap();

        for bound in [lower, upper] {
            let request = QuoteRequest {
                input_mint: venue.get_token(in_idx).unwrap().pubkey,
                output_mint: venue.get_token(out_idx).unwrap().pubkey,
                amount: bound,
                swap_type: SwapType::ExactIn,
            };

            let sim =
                sim_quote_request(&venue, &cache, request.clone(), &mut litesvm, &keypair).await;
            let quote = venue.quote(request).unwrap();

            log::debug!(
                "Boundary = {}\nSimulated = {}\nOff-chain quote = {}\nDelta = {}",
                bound,
                sim,
                quote.expected_output,
                quote.expected_output.abs_diff(sim)
            );

            assert_eq!(quote.expected_output.abs_diff(sim), 0);
        }
    }

    // -------------------------------------------------------------------------
    // Test 2: Random sampling simulation
    // -------------------------------------------------------------------------

    #[rstest]
    #[tokio::test]
    #[case("GqoypwVGG35JSR1AwCm2jeqJPUPvA4cWE45rSbfxHgdK")]
    async fn test_random_samples(#[case] vault_key: Pubkey) {
        init_test_logger();

        // Fetch live vault data from RPC
        let rpc_url = env::var("SOLANA_RPC_URL").unwrap();
        let rpc = RpcClient::new(rpc_url);
        let vault_account = rpc.get_account(&vault_key).await.unwrap();

        let cache = RpcClientCache::new(rpc);
        let mut venue = VoltrVaultVenue::from_account(&vault_key, &vault_account).unwrap();
        venue.update_state(&cache).await.unwrap();

        // Setup simulation VM
        let (mut litesvm, keypair) = setup_litesvm();

        // Sync sysvar clock
        let latest_clock = cache.get_account(&clock::ID).await.unwrap();
        let latest_clock: Clock = latest_clock
            .as_ref()
            .ok_or(TradingVenueError::NoAccountFound(clock::ID.into()))
            .unwrap()
            .deserialize_data()
            .unwrap();
        litesvm.set_sysvar::<Clock>(&latest_clock);

        let tradable_mints = venue.get_token_info();
        assert_eq!(tradable_mints.len(), 2);

        // Deposit direction: asset (0) -> LP (1)
        let (lb, ub) = venue.bounds(0, 1).unwrap();

        for _ in 0..50 {
            let amount = sample_log_uniform_u64(lb, ub);

            let request = QuoteRequest {
                input_mint: venue.get_token(0).unwrap().pubkey,
                output_mint: venue.get_token(1).unwrap().pubkey,
                amount,
                swap_type: SwapType::ExactIn,
            };

            let sim =
                sim_quote_request(&venue, &cache, request.clone(), &mut litesvm, &keypair).await;
            let quote = venue.quote(request).unwrap();

            log::debug!(
                "Random sim: {}\nQuote: {}\nDelta: {}",
                sim,
                quote.expected_output,
                quote.expected_output.abs_diff(sim)
            );

            assert_eq!(quote.expected_output.abs_diff(sim), 0);
        }
    }

    // -------------------------------------------------------------------------
    // Test 3: AMM Monotonicity
    // -------------------------------------------------------------------------

    #[rstest]
    #[tokio::test]
    #[case("GqoypwVGG35JSR1AwCm2jeqJPUPvA4cWE45rSbfxHgdK")]
    async fn test_monotone(#[case] vault_key: String) {
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
        log::debug!("Loaded token info: {:#?}", token_info);

        // Voltr vaults always have 2 tokens (asset + LP).
        assert_eq!(token_info.len(), 2);

        //
        // For each direction (deposit: asset→LP, redeem: LP→asset)
        // verify the swap function is monotone increasing.
        //
        for (in_idx, out_idx) in [(0, 1), (1, 0)] {
            let (lb, ub) = venue.bounds(in_idx, out_idx).unwrap();
            let mut test_amounts = Vec::with_capacity(50);

            for _ in 0..50 {
                test_amounts.push(sample_log_uniform_u64(lb, ub));
            }
            test_amounts.sort();

            let mut prev = 0;
            for amount in test_amounts {
                let input_mint = token_info[in_idx as usize].pubkey;
                let output_mint = token_info[out_idx as usize].pubkey;

                let result = venue
                    .quote(QuoteRequest {
                        input_mint,
                        output_mint,
                        amount,
                        swap_type: SwapType::ExactIn,
                    })
                    .expect("Quote failed");

                log::debug!("quote: {:#?}", result);

                assert!(
                    prev <= result.expected_output,
                    "Swap function is not monotone (prev: {}) > (output: {})",
                    prev,
                    result.expected_output
                );

                prev = result.expected_output;
            }
        }
    }

    // -------------------------------------------------------------------------
    // Test 4: Quoting speed
    // -------------------------------------------------------------------------

    #[rstest]
    #[tokio::test]
    #[case("GqoypwVGG35JSR1AwCm2jeqJPUPvA4cWE45rSbfxHgdK", 10_000)]
    async fn test_quoting_speed(#[case] vault_key: String, #[case] iterations: usize) {
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
        log::debug!("Loaded token info: {:#?}", token_info);

        // Voltr vaults always have 2 tokens (asset + LP).
        assert_eq!(token_info.len(), 2);

        //
        // For each direction (deposit: asset→LP, redeem: LP→asset)
        // verify quoting speed requirements are met.
        //
        for (in_idx, out_idx) in [(0, 1), (1, 0)] {
            let input_mint = token_info[in_idx as usize].pubkey;
            let output_mint = token_info[out_idx as usize].pubkey;

            let (lb, ub) = venue.bounds(in_idx, out_idx).unwrap();
            let mut test_amounts = Vec::with_capacity(iterations);

            for _ in 0..iterations {
                test_amounts.push(sample_log_uniform_u64(lb, ub));
            }

            let start = Instant::now();
            for amount in test_amounts {
                let result = venue
                    .quote(QuoteRequest {
                        input_mint,
                        output_mint,
                        amount,
                        swap_type: SwapType::ExactIn,
                    })
                    .expect("Quote failed");

                log::debug!("quote: {:#?}", result);
            }
            let elapsed = start.elapsed().as_secs_f64();
            let avg_time = elapsed / iterations as f64;

            log::info!("Average quoting speed: {}", avg_time);

            assert!(
                avg_time < 0.0001,
                "Failed quoting speed test swapping ({}) -> ({})",
                input_mint,
                output_mint
            );
        }
    }
}
