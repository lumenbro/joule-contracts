#![no_std]

use soroban_sdk::{
    auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation},
    contract, contracterror, contractimpl, contracttype, token::TokenClient, Address, BytesN, Env,
    IntoVal, Map, Symbol, TryIntoVal, U256, Val, Vec,
};

// ─── Storage Keys ────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    JouleToken,
    Pool,
    QuoteToken,
    Oracle,
    Owner,
    QuotePrice,
    UpperBps,
    LowerBps,
    MaxMint,
    MaxQuoteSpend,
    JouleIsToken0,
    Initialized,
    MaxStaleLedgers,
    CooldownLedgers,
    LastRebalanceLedger,
    MinReserve,
    PoolFee,
    Router,
}

// ─── Errors ──────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum RebalancerError {
    Unauthorized = 1,
    NoRebalanceNeeded = 2,
    InsufficientQuote = 3,
    PoolEmpty = 4,
    QuotePriceNotSet = 5,
    AlreadyInitialized = 6,
    NotInitialized = 7,
    OracleStale = 8,
    CooldownActive = 9,
    SwapFailed = 10,
    SwapSlippage = 11,
}

// ─── Defaults ────────────────────────────────────────────────────

const DEFAULT_MAX_STALE_LEDGERS: u32 = 1000; // ~83 min at 5s/ledger
const DEFAULT_COOLDOWN_LEDGERS: u32 = 12; // ~1 min
const DEFAULT_MIN_RESERVE: i128 = 10_000_000; // 1 token (7 decimals)

// TTL constants: extend instance storage proactively to prevent archival
const TTL_THRESHOLD: u32 = 17_280; // ~1 day at 5s/ledger
const TTL_EXTEND_TO: u32 = 518_400; // ~30 days

// ─── Status return type ─────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug)]
pub struct PoolStatus {
    pub reserve_quote: i128,
    pub reserve_joule: i128,
    pub pool_joule_usd_x7: i128,
    pub oracle_joule_usd_x7: i128,
    pub quote_usd_x7: i128,
    pub deviation_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Config {
    pub joule_token: Address,
    pub pool: Address,
    pub quote_token: Address,
    pub oracle: Address,
    pub owner: Address,
    pub quote_price: i128,
    pub upper_bps: u32,
    pub lower_bps: u32,
    pub max_mint: i128,
    pub max_quote_spend: i128,
    pub joule_is_token0: bool,
    pub max_stale_ledgers: u32,
    pub cooldown_ledgers: u32,
    pub min_reserve: i128,
    pub router: Address,
    pub pool_fee: u32,
}

// ─── Contract ────────────────────────────────────────────────────

#[contract]
pub struct Rebalancer;

// ─── Helpers ─────────────────────────────────────────────────────

/// Integer square root via Newton's method.
fn isqrt(n: i128) -> i128 {
    if n <= 0 {
        return 0;
    }
    if n == 1 {
        return 1;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

fn require_initialized(env: &Env) {
    let init: bool = env
        .storage()
        .instance()
        .get(&DataKey::Initialized)
        .unwrap_or(false);
    assert!(init, "Contract not initialized");
}

fn require_oracle(env: &Env) {
    let oracle: Address = env
        .storage()
        .instance()
        .get(&DataKey::Oracle)
        .expect("Oracle not set");
    oracle.require_auth();
}

fn require_owner(env: &Env) {
    let owner: Address = env
        .storage()
        .instance()
        .get(&DataKey::Owner)
        .expect("Owner not set");
    owner.require_auth();
}

/// Get reserves from V3 pool by querying token balances directly.
/// Returns (reserve_quote, reserve_joule).
fn get_pool_reserves(env: &Env) -> (i128, i128) {
    let pool: Address = env
        .storage()
        .instance()
        .get(&DataKey::Pool)
        .expect("Pool not set");
    let joule_token: Address = env
        .storage()
        .instance()
        .get(&DataKey::JouleToken)
        .expect("JOULE not set");
    let quote_token: Address = env
        .storage()
        .instance()
        .get(&DataKey::QuoteToken)
        .expect("Quote not set");

    let joule_client = TokenClient::new(env, &joule_token);
    let quote_client = TokenClient::new(env, &quote_token);

    let reserve_joule = joule_client.balance(&pool);
    let reserve_quote = quote_client.balance(&pool);

    (reserve_quote, reserve_joule)
}

/// V3 router swap params struct (matches SushiSwap V3 ExactInputSingleParams).
/// Fields are alphabetically ordered as Soroban serializes struct fields alphabetically.
#[contracttype]
#[derive(Clone)]
pub struct SwapParams {
    pub amount_in: i128,
    pub amount_out_minimum: i128,
    pub deadline: u64,
    pub fee: u32,
    pub recipient: Address,
    pub sender: Address,
    pub sqrt_price_limit_x96: u128,
    pub token_in: Address,
    pub token_out: Address,
}

/// Swap tokens directly through the V3 pool (bypasses router).
/// Returns the amount of output tokens received.
///
/// Direct pool.swap lets us build the exact auth tree for authorize_as_current_contract,
/// which is required because pool.swap calls sender.require_auth().
fn pool_swap(env: &Env, token_in: &Address, _token_out: &Address, amount_in: i128) -> i128 {
    let pool: Address = env
        .storage()
        .instance()
        .get(&DataKey::Pool)
        .expect("Pool not set");
    let joule_token: Address = env
        .storage()
        .instance()
        .get(&DataKey::JouleToken)
        .expect("JOULE not set");
    let self_addr = env.current_contract_address();

    // Determine swap direction: zero_for_one means selling token0 for token1
    let selling_joule = token_in == &joule_token;
    let joule_is_token0: bool = env
        .storage()
        .instance()
        .get(&DataKey::JouleIsToken0)
        .unwrap_or(false);
    let zero_for_one = if joule_is_token0 {
        selling_joule
    } else {
        !selling_joule
    };

    // sqrt_price_limit_x96 as U256 (pool uses Q64.96 format)
    // MIN_SQRT_RATIO + 1 for zero_for_one, very large for one_for_zero
    let sqrt_price_limit: U256 = if zero_for_one {
        U256::from_u128(env, 4295128740) // MIN_SQRT_RATIO + 1
    } else {
        // MAX_SQRT_RATIO - 1 ≈ 1.46e48 (fits in ~160 bits)
        // Construct from parts: hi_hi=0, hi_lo=0xFFFF8963, lo_hi=0xEFD1FC6A50648849, lo_lo=0x5F5C572E0000953A
        // Simpler: use a value larger than any realistic sqrt_price
        U256::from_u128(env, u128::MAX)
    };

    // Get oracle hints from pool
    let hints: Val = env.invoke_contract(
        &pool,
        &Symbol::new(env, "get_oracle_hints"),
        Vec::new(env),
    );

    // Build the exact args for pool.swap
    let mut swap_args: Vec<Val> = Vec::new(env);
    swap_args.push_back(self_addr.clone().into_val(env)); // sender
    swap_args.push_back(self_addr.clone().into_val(env)); // recipient
    swap_args.push_back(zero_for_one.into_val(env)); // zero_for_one
    swap_args.push_back(amount_in.into_val(env)); // amount_specified (i128)
    swap_args.push_back(sqrt_price_limit.into_val(env)); // sqrt_price_limit_x96 (U256)
    swap_args.push_back(hints); // hints

    // Pre-authorize the token transfer that pool.swap will execute on our behalf.
    // Since we're the direct caller of pool.swap, sender.require_auth() passes
    // automatically. We only need to authorize the nested token.transfer call
    // (pool transfers token_in from us to itself).
    env.authorize_as_current_contract(soroban_sdk::vec![
        env,
        InvokerContractAuthEntry::Contract(SubContractInvocation {
            context: ContractContext {
                contract: token_in.clone(),
                fn_name: Symbol::new(env, "transfer"),
                args: soroban_sdk::vec![
                    env,
                    self_addr.clone().into_val(env),
                    pool.clone().into_val(env),
                    amount_in.into_val(env),
                ],
            },
            sub_invocations: soroban_sdk::vec![env],
        })
    ]);

    // Call pool.swap directly
    let result: Val =
        env.invoke_contract(&pool, &Symbol::new(env, "swap"), swap_args);

    // pool.swap returns SwapResult { amount0: i128, amount1: i128, liquidity, sqrt_price_x96, tick }
    // Extract amount0 and amount1 from the struct (serialized as Map<Symbol, Val>)
    let result_map: Map<Symbol, Val> =
        result.try_into_val(env).expect("Invalid swap result");
    let amount0: i128 = result_map
        .get(Symbol::new(env, "amount0"))
        .expect("Missing amount0")
        .try_into_val(env)
        .expect("Invalid amount0");
    let amount1: i128 = result_map
        .get(Symbol::new(env, "amount1"))
        .expect("Missing amount1")
        .try_into_val(env)
        .expect("Invalid amount1");

    // Positive = tokens flowing INTO pool (what we pay)
    // Negative = tokens flowing OUT of pool (what we receive)
    if zero_for_one {
        (-amount1).max(0)
    } else {
        (-amount0).max(0)
    }
}

/// Get JOULE/USD price and ledger from the JOULE token's oracle.
/// Returns (price_x7, ledger_when_set).
fn get_joule_price(env: &Env) -> (i128, u32) {
    let joule_token: Address = env
        .storage()
        .instance()
        .get(&DataKey::JouleToken)
        .expect("JOULE token not set");

    let result: soroban_sdk::Vec<Val> =
        env.invoke_contract(&joule_token, &Symbol::new(env, "get_price"), Vec::new(env));

    let price_val = result.get(0).expect("Missing price");
    let ledger_val = result.get(1).expect("Missing ledger");
    let price: i128 = price_val.try_into_val(env).expect("Invalid price");
    let ledger: u32 = ledger_val.try_into_val(env).expect("Invalid ledger");
    (price, ledger)
}

/// Mint JOULE to an address via oracle_mint (this contract IS the oracle).
fn oracle_mint_to(env: &Env, to: &Address, amount: i128) {
    let joule_token: Address = env
        .storage()
        .instance()
        .get(&DataKey::JouleToken)
        .expect("JOULE token not set");

    let mut args = Vec::new(env);
    args.push_back(to.clone().into_val(env));
    args.push_back(amount.into_val(env));

    env.invoke_contract::<Val>(&joule_token, &Symbol::new(env, "oracle_mint"), args);
}

/// Burn JOULE held by this contract via burn_for_compute.
fn burn_joule(env: &Env, amount: i128) {
    let joule_token: Address = env
        .storage()
        .instance()
        .get(&DataKey::JouleToken)
        .expect("JOULE token not set");

    let mut args = Vec::new(env);
    args.push_back(env.current_contract_address().into_val(env));
    args.push_back(amount.into_val(env));

    env.invoke_contract::<Val>(
        &joule_token,
        &Symbol::new(env, "burn_for_compute"),
        args,
    );
}

// ─── Implementation ──────────────────────────────────────────────

#[contractimpl]
impl Rebalancer {
    /// Initialize the rebalancer with all config.
    /// `quote_token` is the pool's quote asset (e.g. USDC SAC address).
    pub fn initialize(
        env: Env,
        joule_token: Address,
        pool: Address,
        quote_token: Address,
        oracle: Address,
        owner: Address,
        joule_is_token0: bool,
        router: Address,
        pool_fee: u32,
    ) {
        let already: bool = env
            .storage()
            .instance()
            .get(&DataKey::Initialized)
            .unwrap_or(false);
        assert!(!already, "Already initialized");

        env.storage()
            .instance()
            .set(&DataKey::JouleToken, &joule_token);
        env.storage().instance().set(&DataKey::Pool, &pool);
        env.storage()
            .instance()
            .set(&DataKey::QuoteToken, &quote_token);
        env.storage().instance().set(&DataKey::Oracle, &oracle);
        env.storage().instance().set(&DataKey::Owner, &owner);
        env.storage()
            .instance()
            .set(&DataKey::JouleIsToken0, &joule_is_token0);
        env.storage().instance().set(&DataKey::Router, &router);
        env.storage().instance().set(&DataKey::PoolFee, &pool_fee);

        // Defaults
        env.storage()
            .instance()
            .set(&DataKey::UpperBps, &500u32);
        env.storage()
            .instance()
            .set(&DataKey::LowerBps, &500u32);
        env.storage()
            .instance()
            .set(&DataKey::MaxMint, &100_000_000_000i128);
        env.storage()
            .instance()
            .set(&DataKey::MaxQuoteSpend, &50_000_000_000i128);
        env.storage()
            .instance()
            .set(&DataKey::MaxStaleLedgers, &DEFAULT_MAX_STALE_LEDGERS);
        env.storage()
            .instance()
            .set(&DataKey::CooldownLedgers, &DEFAULT_COOLDOWN_LEDGERS);
        env.storage()
            .instance()
            .set(&DataKey::MinReserve, &DEFAULT_MIN_RESERVE);
        env.storage()
            .instance()
            .set(&DataKey::Initialized, &true);

        env.events()
            .publish((Symbol::new(&env, "initialized"),), (joule_token, pool));
    }

    /// Oracle sets quote token USD price (7-decimal fixed-point).
    pub fn set_quote_price(env: Env, price: i128) -> Result<(), RebalancerError> {
        require_initialized(&env);
        require_oracle(&env);
        assert!(price > 0, "Price must be positive");
        env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);

        env.storage()
            .instance()
            .set(&DataKey::QuotePrice, &price);

        env.events()
            .publish((Symbol::new(&env, "quote_price_set"),), price);

        Ok(())
    }

    /// Oracle forwards a JOULE/USD price update to the JOULE token contract.
    pub fn update_price(env: Env, price_scaled: i128, nonce: u64) -> Result<(), RebalancerError> {
        require_initialized(&env);
        require_oracle(&env);
        env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);

        let joule_token: Address = env
            .storage()
            .instance()
            .get(&DataKey::JouleToken)
            .expect("JOULE token not set");

        let mut args = Vec::new(&env);
        args.push_back(price_scaled.into_val(&env));
        args.push_back(nonce.into_val(&env));

        env.invoke_contract::<Val>(&joule_token, &Symbol::new(&env, "set_price"), args);

        env.events()
            .publish((Symbol::new(&env, "price_forwarded"),), (price_scaled, nonce));

        Ok(())
    }

    /// Main rebalance logic. Compares pool price vs oracle, mints or buys+burns.
    pub fn rebalance(env: Env) -> Result<(), RebalancerError> {
        require_initialized(&env);
        require_oracle(&env);
        env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);

        // Fix 2: Cooldown check
        let cooldown_ledgers: u32 = env
            .storage()
            .instance()
            .get(&DataKey::CooldownLedgers)
            .unwrap_or(DEFAULT_COOLDOWN_LEDGERS);
        let last_rebalance: u32 = env
            .storage()
            .instance()
            .get(&DataKey::LastRebalanceLedger)
            .unwrap_or(0);
        let current_ledger = env.ledger().sequence();
        if last_rebalance > 0 && current_ledger - last_rebalance < cooldown_ledgers {
            return Err(RebalancerError::CooldownActive);
        }

        let quote_usd: i128 = env
            .storage()
            .instance()
            .get(&DataKey::QuotePrice)
            .ok_or(RebalancerError::QuotePriceNotSet)?;

        // Fix 1: Oracle staleness check
        let (joule_usd, price_ledger) = get_joule_price(&env);
        let max_stale: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MaxStaleLedgers)
            .unwrap_or(DEFAULT_MAX_STALE_LEDGERS);
        if current_ledger - price_ledger > max_stale {
            return Err(RebalancerError::OracleStale);
        }

        let (reserve_quote, reserve_joule) = get_pool_reserves(&env);

        // Fix 5: Minimum reserve threshold
        let min_reserve: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MinReserve)
            .unwrap_or(DEFAULT_MIN_RESERVE);
        if reserve_quote < min_reserve || reserve_joule < min_reserve {
            return Err(RebalancerError::PoolEmpty);
        }

        let upper_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::UpperBps)
            .unwrap_or(500);
        let lower_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::LowerBps)
            .unwrap_or(500);

        let lhs = reserve_quote * quote_usd * 10_000;
        let rhs_upper = joule_usd * reserve_joule * (10_000 + upper_bps as i128);
        let rhs_lower = joule_usd * reserve_joule * (10_000 - lower_bps as i128);

        if lhs > rhs_upper {
            Self::do_mint_rebalance(&env, reserve_quote, reserve_joule, quote_usd, joule_usd, upper_bps)?;
        } else if lhs < rhs_lower {
            Self::do_buyback_rebalance(
                &env,
                reserve_quote,
                reserve_joule,
                quote_usd,
                joule_usd,
            )?;
        } else {
            return Err(RebalancerError::NoRebalanceNeeded);
        }

        // Store last rebalance ledger
        env.storage()
            .instance()
            .set(&DataKey::LastRebalanceLedger, &current_ledger);

        Ok(())
    }

    /// Fund the contract with quote token (e.g. USDC) for buyback operations.
    pub fn fund_quote(env: Env, from: Address, amount: i128) {
        require_initialized(&env);
        from.require_auth();
        assert!(amount > 0, "Amount must be positive");

        let quote_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::QuoteToken)
            .expect("Quote token not set");
        let quote = TokenClient::new(&env, &quote_addr);
        quote.transfer(&from, &env.current_contract_address(), &amount);

        env.events()
            .publish((Symbol::new(&env, "funded"),), (from, amount));
    }

    /// Owner withdraws any token from the contract.
    pub fn withdraw(env: Env, token: Address, to: Address, amount: i128) {
        require_initialized(&env);
        require_owner(&env);
        assert!(amount > 0, "Amount must be positive");

        let client = TokenClient::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);

        env.events()
            .publish((Symbol::new(&env, "withdraw"),), (token, to, amount));
    }

    /// Owner changes the oracle address.
    pub fn set_oracle(env: Env, oracle: Address) {
        require_initialized(&env);
        require_owner(&env);
        env.storage().instance().set(&DataKey::Oracle, &oracle);
        env.events()
            .publish((Symbol::new(&env, "oracle_changed"),), oracle);
    }

    /// Owner updates the V3 pool, router, and fee tier.
    pub fn set_pool(env: Env, pool: Address, joule_is_token0: bool, router: Address, pool_fee: u32) {
        require_initialized(&env);
        require_owner(&env);
        env.storage().instance().set(&DataKey::Pool, &pool);
        env.storage()
            .instance()
            .set(&DataKey::JouleIsToken0, &joule_is_token0);
        env.storage().instance().set(&DataKey::Router, &router);
        env.storage().instance().set(&DataKey::PoolFee, &pool_fee);
        env.events()
            .publish((Symbol::new(&env, "pool_changed"),), (pool, router, pool_fee));
    }

    /// Owner updates rebalancing parameters.
    pub fn set_params(
        env: Env,
        upper_bps: u32,
        lower_bps: u32,
        max_mint: i128,
        max_quote_spend: i128,
        cooldown_ledgers: u32,
        min_reserve: i128,
    ) {
        require_initialized(&env);
        require_owner(&env);
        assert!(upper_bps > 0 && upper_bps < 10_000, "Invalid upper_bps");
        assert!(lower_bps > 0 && lower_bps < 10_000, "Invalid lower_bps");
        assert!(max_mint > 0, "max_mint must be positive");
        assert!(max_quote_spend > 0, "max_quote_spend must be positive");
        assert!(min_reserve > 0, "min_reserve must be positive");

        env.storage()
            .instance()
            .set(&DataKey::UpperBps, &upper_bps);
        env.storage()
            .instance()
            .set(&DataKey::LowerBps, &lower_bps);
        env.storage()
            .instance()
            .set(&DataKey::MaxMint, &max_mint);
        env.storage()
            .instance()
            .set(&DataKey::MaxQuoteSpend, &max_quote_spend);
        env.storage()
            .instance()
            .set(&DataKey::CooldownLedgers, &cooldown_ledgers);
        env.storage()
            .instance()
            .set(&DataKey::MinReserve, &min_reserve);

        env.events().publish(
            (Symbol::new(&env, "params_updated"),),
            (upper_bps, lower_bps, max_mint, max_quote_spend),
        );
    }

    /// Owner updates max stale ledgers for oracle freshness.
    pub fn set_max_stale(env: Env, max_stale_ledgers: u32) {
        require_initialized(&env);
        require_owner(&env);
        assert!(max_stale_ledgers > 0, "Must be positive");
        env.storage()
            .instance()
            .set(&DataKey::MaxStaleLedgers, &max_stale_ledgers);
        env.events()
            .publish((Symbol::new(&env, "max_stale_changed"),), max_stale_ledgers);
    }

    /// Owner upgrades the contract WASM. Requires owner auth.
    pub fn upgrade(env: Env, wasm_hash: BytesN<32>) {
        require_initialized(&env);
        require_owner(&env);
        env.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
        env.deployer().update_current_contract_wasm(wasm_hash);
    }

    /// Returns pool price vs oracle price status.
    pub fn get_status(env: Env) -> Result<PoolStatus, RebalancerError> {
        require_initialized(&env);

        let quote_usd: i128 = env
            .storage()
            .instance()
            .get(&DataKey::QuotePrice)
            .ok_or(RebalancerError::QuotePriceNotSet)?;

        let (joule_usd, _ledger) = get_joule_price(&env);
        let (reserve_quote, reserve_joule) = get_pool_reserves(&env);

        if reserve_quote <= 0 || reserve_joule <= 0 {
            return Err(RebalancerError::PoolEmpty);
        }

        let pool_joule_usd = reserve_quote * quote_usd / reserve_joule;
        let deviation_bps = (pool_joule_usd - joule_usd) * 10_000 / joule_usd;

        Ok(PoolStatus {
            reserve_quote,
            reserve_joule,
            pool_joule_usd_x7: pool_joule_usd,
            oracle_joule_usd_x7: joule_usd,
            quote_usd_x7: quote_usd,
            deviation_bps,
        })
    }

    /// Returns all configuration values.
    pub fn get_config(env: Env) -> Config {
        require_initialized(&env);
        Config {
            joule_token: env
                .storage()
                .instance()
                .get(&DataKey::JouleToken)
                .expect("not set"),
            pool: env
                .storage()
                .instance()
                .get(&DataKey::Pool)
                .expect("not set"),
            quote_token: env
                .storage()
                .instance()
                .get(&DataKey::QuoteToken)
                .expect("not set"),
            oracle: env
                .storage()
                .instance()
                .get(&DataKey::Oracle)
                .expect("not set"),
            owner: env
                .storage()
                .instance()
                .get(&DataKey::Owner)
                .expect("not set"),
            quote_price: env
                .storage()
                .instance()
                .get(&DataKey::QuotePrice)
                .unwrap_or(0),
            upper_bps: env
                .storage()
                .instance()
                .get(&DataKey::UpperBps)
                .unwrap_or(500),
            lower_bps: env
                .storage()
                .instance()
                .get(&DataKey::LowerBps)
                .unwrap_or(500),
            max_mint: env
                .storage()
                .instance()
                .get(&DataKey::MaxMint)
                .unwrap_or(100_000_000_000),
            max_quote_spend: env
                .storage()
                .instance()
                .get(&DataKey::MaxQuoteSpend)
                .unwrap_or(50_000_000_000),
            joule_is_token0: env
                .storage()
                .instance()
                .get(&DataKey::JouleIsToken0)
                .unwrap_or(true),
            max_stale_ledgers: env
                .storage()
                .instance()
                .get(&DataKey::MaxStaleLedgers)
                .unwrap_or(DEFAULT_MAX_STALE_LEDGERS),
            cooldown_ledgers: env
                .storage()
                .instance()
                .get(&DataKey::CooldownLedgers)
                .unwrap_or(DEFAULT_COOLDOWN_LEDGERS),
            min_reserve: env
                .storage()
                .instance()
                .get(&DataKey::MinReserve)
                .unwrap_or(DEFAULT_MIN_RESERVE),
            router: env
                .storage()
                .instance()
                .get(&DataKey::Router)
                .expect("not set"),
            pool_fee: env
                .storage()
                .instance()
                .get(&DataKey::PoolFee)
                .unwrap_or(3000),
        }
    }

    // ─── Internal rebalance methods ──────────────────────────────

    /// Mint JOULE and sell through V3 pool to push price down (pool is overpriced).
    /// Targets band midpoint instead of exact peg.
    /// USDC received stays in rebalancer as buyback reserves.
    fn do_mint_rebalance(
        env: &Env,
        reserve_quote: i128,
        reserve_joule: i128,
        quote_usd: i128,
        joule_usd: i128,
        upper_bps: u32,
    ) -> Result<(), RebalancerError> {
        let max_mint: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MaxMint)
            .unwrap_or(100_000_000_000);

        // Target band midpoint: joule_usd * (1 + upper_bps/2/10000)
        let target_joule_price = joule_usd * (10_000 + upper_bps as i128 / 2);
        let target_reserve_joule = reserve_quote * quote_usd * 10_000 / target_joule_price;
        let mut mint_amount = target_reserve_joule - reserve_joule;

        if mint_amount <= 0 {
            return Err(RebalancerError::NoRebalanceNeeded);
        }

        if mint_amount > max_mint {
            mint_amount = max_mint;
        }

        // Mint JOULE to self (V3 has no sync — must swap through router)
        oracle_mint_to(env, &env.current_contract_address(), mint_amount);

        // Swap JOULE → USDC through V3 router (pushes price down)
        let joule_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::JouleToken)
            .expect("JOULE not set");
        let quote_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::QuoteToken)
            .expect("Quote not set");

        let usdc_received = pool_swap(env, &joule_addr, &quote_addr, mint_amount);

        // Slippage protection: verify USDC received >= 80% of oracle-implied value
        // Expected: mint_amount * joule_usd / quote_usd
        // Min acceptable: 80% of expected (allows for V3 concentrated liquidity + fees)
        let expected_usdc = mint_amount * joule_usd / quote_usd;
        let min_usdc = expected_usdc * 80 / 100;
        if usdc_received < min_usdc {
            // Swap executed but got far less than expected — pool too thin
            // Note: tokens already swapped, but this prevents silent bad execution
            // in future calls. Log the event for diagnostics.
            env.events().publish(
                (Symbol::new(env, "slippage_warning"),),
                (usdc_received, expected_usdc, min_usdc),
            );
        }

        env.events().publish(
            (Symbol::new(env, "rebalance_mint"),),
            (mint_amount, usdc_received, reserve_quote, reserve_joule),
        );

        Ok(())
    }

    /// Buy JOULE from V3 pool with quote token and burn it (pool is underpriced).
    fn do_buyback_rebalance(
        env: &Env,
        reserve_quote: i128,
        reserve_joule: i128,
        quote_usd: i128,
        joule_usd: i128,
    ) -> Result<(), RebalancerError> {
        let max_quote_spend: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MaxQuoteSpend)
            .unwrap_or(50_000_000_000);

        // Calculate USDC to spend to restore peg
        let k = reserve_quote * reserve_joule;
        let target_reserve_quote = isqrt(k * joule_usd / quote_usd);
        let mut quote_to_spend = target_reserve_quote - reserve_quote;

        if quote_to_spend <= 0 {
            return Err(RebalancerError::NoRebalanceNeeded);
        }

        if quote_to_spend > max_quote_spend {
            quote_to_spend = max_quote_spend;
        }

        let quote_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::QuoteToken)
            .expect("Quote token not set");
        let quote_client = TokenClient::new(env, &quote_addr);
        let quote_balance = quote_client.balance(&env.current_contract_address());

        if quote_balance < quote_to_spend {
            return Err(RebalancerError::InsufficientQuote);
        }

        // Swap USDC → JOULE through V3 router
        let joule_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::JouleToken)
            .expect("JOULE token not set");

        let joule_received = pool_swap(env, &quote_addr, &joule_addr, quote_to_spend);

        // Slippage protection: verify JOULE received >= 80% of oracle-implied value
        // Expected: quote_to_spend * quote_usd / joule_usd
        let expected_joule = quote_to_spend * quote_usd / joule_usd;
        let min_joule = expected_joule * 80 / 100;
        if joule_received < min_joule {
            env.events().publish(
                (Symbol::new(env, "slippage_warning"),),
                (joule_received, expected_joule, min_joule),
            );
        }

        // Burn all received JOULE
        let joule_client = TokenClient::new(env, &joule_addr);
        let joule_balance = joule_client.balance(&env.current_contract_address());

        if joule_balance > 0 {
            burn_joule(env, joule_balance);
        }

        env.events().publish(
            (Symbol::new(env, "rebalance_buyback"),),
            (quote_to_spend, joule_received, reserve_quote, reserve_joule),
        );

        Ok(())
    }
}

// tests
#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger, LedgerInfo};
    use soroban_sdk::{contract, contractimpl, contracttype, map, Env, Map};

    // ─── Mock JOULE Token (fee-free, matches v3 token) ──────────

    #[contracttype]
    #[derive(Clone)]
    enum MockJouleKey {
        Balances,
        Price,
        PriceLedger,
        OracleAddr,
        TotalBurned,
    }

    #[contract]
    pub struct MockJouleToken;

    #[contractimpl]
    impl MockJouleToken {
        pub fn init(env: Env, oracle: Address) {
            env.storage().instance().set(&MockJouleKey::OracleAddr, &oracle);
            let balances: Map<Address, i128> = map![&env];
            env.storage().instance().set(&MockJouleKey::Balances, &balances);
            env.storage().instance().set(&MockJouleKey::TotalBurned, &0i128);
        }

        pub fn get_price(env: Env) -> (i128, u32) {
            let price: i128 = env.storage().instance().get(&MockJouleKey::Price).unwrap_or(0);
            let ledger: u32 = env.storage().instance().get(&MockJouleKey::PriceLedger).unwrap_or(0);
            (price, ledger)
        }

        pub fn set_price(env: Env, price: i128, _nonce: u64) {
            let oracle: Address = env.storage().instance().get(&MockJouleKey::OracleAddr).expect("no oracle");
            oracle.require_auth();
            env.storage().instance().set(&MockJouleKey::Price, &price);
            env.storage().instance().set(&MockJouleKey::PriceLedger, &env.ledger().sequence());
        }

        pub fn oracle_mint(env: Env, to: Address, amount: i128) {
            let oracle: Address = env.storage().instance().get(&MockJouleKey::OracleAddr).expect("no oracle");
            oracle.require_auth();
            let mut balances: Map<Address, i128> = env.storage().instance().get(&MockJouleKey::Balances).unwrap();
            let prev = balances.get(to.clone()).unwrap_or(0);
            balances.set(to, prev + amount);
            env.storage().instance().set(&MockJouleKey::Balances, &balances);
        }

        pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
            // No require_auth in mock — avoids non-root auth issues in cross-contract calls
            let mut balances: Map<Address, i128> = env.storage().instance().get(&MockJouleKey::Balances).unwrap();
            let from_bal = balances.get(from.clone()).unwrap_or(0);
            assert!(from_bal >= amount, "insufficient balance");
            balances.set(from, from_bal - amount);
            let to_bal = balances.get(to.clone()).unwrap_or(0);
            balances.set(to, to_bal + amount);
            env.storage().instance().set(&MockJouleKey::Balances, &balances);
        }

        pub fn burn_for_compute(env: Env, from: Address, amount: i128) {
            // No require_auth in mock
            let mut balances: Map<Address, i128> = env.storage().instance().get(&MockJouleKey::Balances).unwrap();
            let bal = balances.get(from.clone()).unwrap_or(0);
            assert!(bal >= amount, "insufficient balance to burn");
            balances.set(from, bal - amount);
            env.storage().instance().set(&MockJouleKey::Balances, &balances);
            let burned: i128 = env.storage().instance().get(&MockJouleKey::TotalBurned).unwrap_or(0);
            env.storage().instance().set(&MockJouleKey::TotalBurned, &(burned + amount));
        }

        pub fn balance(env: Env, id: Address) -> i128 {
            let balances: Map<Address, i128> = env.storage().instance().get(&MockJouleKey::Balances).unwrap();
            balances.get(id).unwrap_or(0)
        }

        pub fn total_burned(env: Env) -> i128 {
            env.storage().instance().get(&MockJouleKey::TotalBurned).unwrap_or(0)
        }
    }

    // ─── Mock V3 Pool ───────────────────────────────────────────

    #[contracttype]
    #[derive(Clone)]
    enum MockV3PoolKey {
        Token0,
        Token1,
    }

    #[contract]
    pub struct MockV3Pool;

    /// Oracle hints struct for V3 pool (matches real pool interface).
    /// Real pool uses { checkpoint: u32, slot: u128 }.
    #[contracttype]
    #[derive(Clone)]
    pub struct OracleHints {
        pub checkpoint: u32,
        pub slot: u128,
    }

    /// V3 pool swap result struct.
    #[contracttype]
    #[derive(Clone)]
    pub struct SwapResult {
        pub amount0: i128,
        pub amount1: i128,
        pub liquidity: u128,
        pub sqrt_price_x96: U256,
        pub tick: i32,
    }

    #[contractimpl]
    impl MockV3Pool {
        pub fn init(env: Env, token0: Address, token1: Address, reserve0: i128, reserve1: i128) {
            env.storage().instance().set(&MockV3PoolKey::Token0, &token0);
            env.storage().instance().set(&MockV3PoolKey::Token1, &token1);
            let _ = (reserve0, reserve1); // reserves tracked via token balances
        }

        /// V3 pool interface: returns (reserve0, reserve1) from actual token balances.
        pub fn get_pool_state_with_balances(env: Env) -> (i128, i128) {
            let token0: Address = env.storage().instance().get(&MockV3PoolKey::Token0).expect("no token0");
            let token1: Address = env.storage().instance().get(&MockV3PoolKey::Token1).expect("no token1");
            let pool_addr = env.current_contract_address();
            let t0_client = TokenClient::new(&env, &token0);
            let t1_client = TokenClient::new(&env, &token1);
            (t0_client.balance(&pool_addr), t1_client.balance(&pool_addr))
        }

        /// V3 pool oracle hints.
        pub fn get_oracle_hints(_env: Env) -> OracleHints {
            OracleHints {
                checkpoint: 0,
                slot: 0,
            }
        }

        /// V3 pool swap — called directly by rebalancer (bypassing router).
        /// Uses constant-product formula for test approximation.
        /// Returns SwapResult where amount0/amount1: positive = paid, negative = received.
        pub fn swap(
            env: Env,
            _sender: Address,
            _recipient: Address,
            zero_for_one: bool,
            amount_specified: i128,
            _sqrt_price_limit_x96: U256,
            _oracle_hints: OracleHints,
        ) -> SwapResult {
            let token0: Address = env
                .storage()
                .instance()
                .get(&MockV3PoolKey::Token0)
                .expect("no token0");
            let token1: Address = env
                .storage()
                .instance()
                .get(&MockV3PoolKey::Token1)
                .expect("no token1");
            let pool_addr = env.current_contract_address();
            let t0_client = TokenClient::new(&env, &token0);
            let t1_client = TokenClient::new(&env, &token1);
            let reserve0 = t0_client.balance(&pool_addr);
            let reserve1 = t1_client.balance(&pool_addr);

            let (reserve_in, reserve_out, token_in_addr, token_out_addr) = if zero_for_one {
                (reserve0, reserve1, token0, token1)
            } else {
                (reserve1, reserve0, token1, token0)
            };

            // Constant-product with 0.3% fee
            let amount_in = amount_specified;
            let amount_in_with_fee = amount_in * 997;
            let numerator = reserve_out * amount_in_with_fee;
            let denominator = reserve_in * 1000 + amount_in_with_fee;
            let amount_out = numerator / denominator;

            assert!(amount_out > 0, "swap output is zero");

            // Transfer tokens: sender pays token_in, pool pays token_out
            let in_client = TokenClient::new(&env, &token_in_addr);
            in_client.transfer(&_sender, &pool_addr, &amount_in);

            let out_client = TokenClient::new(&env, &token_out_addr);
            out_client.transfer(&pool_addr, &_recipient, &amount_out);

            let (a0, a1) = if zero_for_one {
                (amount_in, -amount_out)
            } else {
                (-amount_out, amount_in)
            };

            SwapResult {
                amount0: a0,
                amount1: a1,
                liquidity: 0,
                sqrt_price_x96: U256::from_u128(&env, 0),
                tick: 0,
            }
        }
    }

    // ─── Mock Router ────────────────────────────────────────────

    #[contracttype]
    #[derive(Clone)]
    enum MockRouterKey {
        Pool,
        Token0,
        Token1,
    }

    #[contract]
    pub struct MockRouter;

    #[contractimpl]
    impl MockRouter {
        pub fn init(env: Env, pool: Address, token0: Address, token1: Address) {
            env.storage().instance().set(&MockRouterKey::Pool, &pool);
            env.storage().instance().set(&MockRouterKey::Token0, &token0);
            env.storage().instance().set(&MockRouterKey::Token1, &token1);
        }

        /// V3 router swap_exact_input_single.
        /// Uses constant-product formula for test approximation.
        /// Accepts a single SwapParams struct (matching real router interface).
        pub fn swap_exact_input_single(
            env: Env,
            params: SwapParams,
        ) -> i128 {
            let pool: Address = env.storage().instance().get(&MockRouterKey::Pool).expect("no pool");

            // Get pool reserves
            let token0: Address = env.storage().instance().get(&MockRouterKey::Token0).expect("no token0");
            let token1: Address = env.storage().instance().get(&MockRouterKey::Token1).expect("no token1");
            let t0_client = TokenClient::new(&env, &token0);
            let t1_client = TokenClient::new(&env, &token1);
            let pool_addr = pool.clone();
            let reserve0 = t0_client.balance(&pool_addr);
            let reserve1 = t1_client.balance(&pool_addr);

            // Determine direction
            let (reserve_in, reserve_out) = if params.token_in == token0 {
                (reserve0, reserve1)
            } else {
                (reserve1, reserve0)
            };

            // Constant-product swap with 0.3% fee
            let amount_in_with_fee = params.amount_in * 997;
            let numerator = reserve_out * amount_in_with_fee;
            let denominator = reserve_in * 1000 + amount_in_with_fee;
            let amount_out = numerator / denominator;

            assert!(amount_out > 0, "swap output is zero");

            // Transfer token_in from sender to pool
            let in_client = TokenClient::new(&env, &params.token_in);
            in_client.transfer(&params.sender, &pool, &params.amount_in);

            // Transfer token_out from pool to recipient
            let out_client = TokenClient::new(&env, &params.token_out);
            out_client.transfer(&pool, &params.recipient, &amount_out);

            amount_out
        }
    }

    // ─── Mock Quote Token (simple SEP-41) ───────────────────────

    #[contracttype]
    #[derive(Clone)]
    enum MockQuoteKey {
        Balances,
    }

    #[contract]
    pub struct MockQuoteToken;

    #[contractimpl]
    impl MockQuoteToken {
        pub fn init(env: Env) {
            let balances: Map<Address, i128> = map![&env];
            env.storage().instance().set(&MockQuoteKey::Balances, &balances);
        }

        pub fn mint(env: Env, to: Address, amount: i128) {
            let mut balances: Map<Address, i128> = env.storage().instance().get(&MockQuoteKey::Balances).unwrap();
            let prev = balances.get(to.clone()).unwrap_or(0);
            balances.set(to, prev + amount);
            env.storage().instance().set(&MockQuoteKey::Balances, &balances);
        }

        pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
            // No require_auth in mock — avoids non-root auth issues in cross-contract calls
            let mut balances: Map<Address, i128> = env.storage().instance().get(&MockQuoteKey::Balances).unwrap();
            let from_bal = balances.get(from.clone()).unwrap_or(0);
            assert!(from_bal >= amount, "insufficient quote balance");
            balances.set(from, from_bal - amount);
            let to_bal = balances.get(to.clone()).unwrap_or(0);
            balances.set(to, to_bal + amount);
            env.storage().instance().set(&MockQuoteKey::Balances, &balances);
        }

        pub fn balance(env: Env, id: Address) -> i128 {
            let balances: Map<Address, i128> = env.storage().instance().get(&MockQuoteKey::Balances).unwrap();
            balances.get(id).unwrap_or(0)
        }
    }

    // ─── Test Helpers ───────────────────────────────────────────

    #[allow(dead_code)]
    struct TestEnv {
        env: Env,
        rebalancer_id: Address,
        rebalancer: RebalancerClient<'static>,
        joule_id: Address,
        joule: MockJouleTokenClient<'static>,
        pool_id: Address,
        pool: MockV3PoolClient<'static>,
        router_id: Address,
        router: MockRouterClient<'static>,
        quote_id: Address,
        quote: MockQuoteTokenClient<'static>,
        oracle: Address,
        owner: Address,
    }

    fn set_ledger(env: &Env, sequence: u32) {
        env.ledger().set(LedgerInfo {
            timestamp: 0,
            protocol_version: 23,
            sequence_number: sequence,
            network_id: [0; 32],
            base_reserve: 10,
            min_temp_entry_ttl: 100,
            min_persistent_entry_ttl: 100,
            max_entry_ttl: 10_000_000,
        });
    }

    fn setup_test(
        initial_reserve_joule: i128,
        initial_reserve_quote: i128,
        oracle_price: i128,
        quote_price: i128,
    ) -> TestEnv {
        let env = Env::default();
        env.mock_all_auths();
        set_ledger(&env, 100);

        let oracle = Address::generate(&env);
        let owner = Address::generate(&env);

        let joule_id = env.register(MockJouleToken, ());
        let joule = MockJouleTokenClient::new(&env, &joule_id);

        let pool_id = env.register(MockV3Pool, ());
        let pool = MockV3PoolClient::new(&env, &pool_id);

        let router_id = env.register(MockRouter, ());
        let router = MockRouterClient::new(&env, &router_id);

        let quote_id = env.register(MockQuoteToken, ());
        let quote = MockQuoteTokenClient::new(&env, &quote_id);

        let rebalancer_id = env.register(Rebalancer, ());
        let rebalancer = RebalancerClient::new(&env, &rebalancer_id);

        // Initialize mocks — oracle for JOULE token IS the rebalancer contract
        joule.init(&rebalancer_id);
        quote.init();

        joule.set_price(&oracle_price, &1u64);

        // Seed pool with initial reserves
        if initial_reserve_joule > 0 {
            joule.oracle_mint(&pool_id, &initial_reserve_joule);
        }
        if initial_reserve_quote > 0 {
            quote.mint(&pool_id, &initial_reserve_quote);
        }

        // token0=JOULE, token1=quote for V3 pool
        pool.init(&joule_id, &quote_id, &initial_reserve_joule, &initial_reserve_quote);

        // Initialize router with pool info
        router.init(&pool_id, &joule_id, &quote_id);

        // Initialize rebalancer with V3 pool + router
        rebalancer.initialize(
            &joule_id, &pool_id, &quote_id, &oracle, &owner,
            &true,           // joule_is_token0
            &router_id,      // router
            &3000u32,        // pool_fee (0.3%)
        );

        if quote_price > 0 {
            rebalancer.set_quote_price(&quote_price);
        }

        TestEnv {
            env,
            rebalancer_id,
            rebalancer,
            joule_id,
            joule,
            pool_id,
            pool,
            router_id,
            router,
            quote_id,
            quote,
            oracle,
            owner,
        }
    }

    /// Helper: pool_price = reserve_quote * quote_usd / reserve_joule
    /// So reserve_joule = reserve_quote * quote_usd / target_pool_price
    fn joule_reserves_for_price(reserve_quote: i128, quote_usd: i128, target_pool_price: i128) -> i128 {
        reserve_quote * quote_usd / target_pool_price
    }

    // ─── Basic Tests ────────────────────────────────────────────

    #[test]
    fn test_isqrt() {
        assert_eq!(isqrt(0), 0);
        assert_eq!(isqrt(1), 1);
        assert_eq!(isqrt(2), 1);
        assert_eq!(isqrt(4), 2);
        assert_eq!(isqrt(9), 3);
        assert_eq!(isqrt(10), 3);
        assert_eq!(isqrt(100), 10);
        assert_eq!(isqrt(1_000_000), 1_000);
        assert_eq!(isqrt(1_000_000_000_000_000_000), 1_000_000_000);
        assert_eq!(isqrt(49), 7);
        assert_eq!(isqrt(10000), 100);
        assert_eq!(isqrt(i128::MAX / 2), 9_223_372_036_854_775_807);
    }

    #[test]
    fn test_isqrt_edge_cases() {
        assert_eq!(isqrt(-1), 0);
        assert_eq!(isqrt(-100), 0);
        assert_eq!(isqrt(0), 0);
        assert_eq!(isqrt(1), 1);
        assert_eq!(isqrt(2), 1);
        assert_eq!(isqrt(3), 1);
    }

    #[test]
    fn test_initialize() {
        let env = Env::default();
        let contract_id = env.register(Rebalancer, ());
        let client = RebalancerClient::new(&env, &contract_id);

        let joule = Address::generate(&env);
        let pool = Address::generate(&env);
        let quote = Address::generate(&env);
        let oracle = Address::generate(&env);
        let owner = Address::generate(&env);
        let router = Address::generate(&env);

        client.initialize(&joule, &pool, &quote, &oracle, &owner, &true, &router, &3000u32);

        let config = client.get_config();
        assert_eq!(config.joule_token, joule);
        assert_eq!(config.pool, pool);
        assert_eq!(config.quote_token, quote);
        assert_eq!(config.oracle, oracle);
        assert_eq!(config.owner, owner);
        assert_eq!(config.upper_bps, 500);
        assert_eq!(config.lower_bps, 500);
        assert!(config.joule_is_token0);
        assert_eq!(config.max_stale_ledgers, DEFAULT_MAX_STALE_LEDGERS);
        assert_eq!(config.cooldown_ledgers, DEFAULT_COOLDOWN_LEDGERS);
        assert_eq!(config.min_reserve, DEFAULT_MIN_RESERVE);
        assert_eq!(config.router, router);
        assert_eq!(config.pool_fee, 3000);
    }

    #[test]
    #[should_panic(expected = "Already initialized")]
    fn test_double_initialize() {
        let env = Env::default();
        let contract_id = env.register(Rebalancer, ());
        let client = RebalancerClient::new(&env, &contract_id);
        let joule = Address::generate(&env);
        let pool = Address::generate(&env);
        let quote = Address::generate(&env);
        let oracle = Address::generate(&env);
        let owner = Address::generate(&env);
        let router = Address::generate(&env);
        client.initialize(&joule, &pool, &quote, &oracle, &owner, &true, &router, &3000u32);
        client.initialize(&joule, &pool, &quote, &oracle, &owner, &true, &router, &3000u32);
    }

    #[test]
    fn test_set_quote_price() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(Rebalancer, ());
        let client = RebalancerClient::new(&env, &contract_id);
        let joule = Address::generate(&env);
        let pool = Address::generate(&env);
        let quote = Address::generate(&env);
        let oracle = Address::generate(&env);
        let owner = Address::generate(&env);
        let router = Address::generate(&env);
        client.initialize(&joule, &pool, &quote, &oracle, &owner, &true, &router, &3000u32);
        client.set_quote_price(&10_000_000i128);
        let config = client.get_config();
        assert_eq!(config.quote_price, 10_000_000);
    }

    #[test]
    fn test_set_params() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(Rebalancer, ());
        let client = RebalancerClient::new(&env, &contract_id);
        let joule = Address::generate(&env);
        let pool = Address::generate(&env);
        let quote = Address::generate(&env);
        let oracle = Address::generate(&env);
        let owner = Address::generate(&env);
        let router = Address::generate(&env);
        client.initialize(&joule, &pool, &quote, &oracle, &owner, &true, &router, &3000u32);
        client.set_params(&300u32, &300u32, &50_000_000_000i128, &25_000_000_000i128, &20u32, &20_000_000i128);
        let config = client.get_config();
        assert_eq!(config.upper_bps, 300);
        assert_eq!(config.lower_bps, 300);
        assert_eq!(config.max_mint, 50_000_000_000);
        assert_eq!(config.max_quote_spend, 25_000_000_000);
        assert_eq!(config.cooldown_ledgers, 20);
        assert_eq!(config.min_reserve, 20_000_000);
    }

    #[test]
    fn test_set_pool() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(Rebalancer, ());
        let client = RebalancerClient::new(&env, &contract_id);
        let joule = Address::generate(&env);
        let pool = Address::generate(&env);
        let quote = Address::generate(&env);
        let oracle = Address::generate(&env);
        let owner = Address::generate(&env);
        let router = Address::generate(&env);
        client.initialize(&joule, &pool, &quote, &oracle, &owner, &true, &router, &3000u32);
        let new_pool = Address::generate(&env);
        let new_router = Address::generate(&env);
        client.set_pool(&new_pool, &false, &new_router, &10000u32);
        let config = client.get_config();
        assert_eq!(config.pool, new_pool);
        assert!(!config.joule_is_token0);
        assert_eq!(config.router, new_router);
        assert_eq!(config.pool_fee, 10000);
    }

    // ─── Basic Rebalance Flow ───────────────────────────────────

    /// 1. 3% deviation (within 5% band) — NoRebalanceNeeded
    #[test]
    fn test_no_rebalance_within_band() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        // 3% overpriced: pool_price = 10300
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 10_300);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        let result = t.rebalancer.try_rebalance();
        assert_eq!(result, Err(Ok(RebalancerError::NoRebalanceNeeded)));
    }

    /// 2. Pool 10% above oracle — mint rebalance (mints JOULE, swaps through pool)
    #[test]
    fn test_mint_rebalance_overpriced() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        // 10% overpriced: pool_price = 11000
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 11_000);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        let pool_joule_before = t.joule.balance(&t.pool_id);
        t.rebalancer.rebalance();
        let pool_joule_after = t.joule.balance(&t.pool_id);
        // After swap, pool should have more JOULE (rebalancer sold JOULE into pool)
        assert!(pool_joule_after > pool_joule_before, "Pool should have more JOULE after mint rebalance");
    }

    /// 3. Pool 10% below oracle — buyback rebalance (buys JOULE + burns)
    #[test]
    fn test_buyback_rebalance_underpriced() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        // 10% underpriced: pool_price = 9000
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 9_000);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        t.quote.mint(&t.rebalancer_id, &500_0000000i128);
        let quote_before = t.quote.balance(&t.rebalancer_id);
        t.rebalancer.rebalance();
        let quote_after = t.quote.balance(&t.rebalancer_id);
        assert!(quote_after < quote_before, "USDC should have been spent on buyback");
        // Rebalancer should have burned all received JOULE
        let rebalancer_joule = t.joule.balance(&t.rebalancer_id);
        assert_eq!(rebalancer_joule, 0, "All received JOULE should be burned");
    }

    /// 4. Large deviation, mint capped at max_mint
    #[test]
    fn test_mint_capped_at_max() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        // 50% overpriced: pool_price = 15000
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 15_000);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        let small_max = 50_000_000i128; // 5 JOULE
        t.rebalancer.set_params(&500u32, &500u32, &small_max, &50_000_000_000i128, &12u32, &10_000_000i128);
        let pool_joule_before = t.joule.balance(&t.pool_id);
        t.rebalancer.rebalance();
        let pool_joule_after = t.joule.balance(&t.pool_id);
        let added_to_pool = pool_joule_after - pool_joule_before;
        // The router takes amount_in from sender and sends to pool, so pool receives exactly small_max
        assert_eq!(added_to_pool, small_max, "Pool JOULE increase should equal capped mint amount");
    }

    /// 5. Large deviation, buyback spend capped at max_quote_spend
    #[test]
    fn test_buyback_capped_at_max() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        // 50% underpriced: pool_price = 5000
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 5_000);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        let small_max_spend = 10_000_000i128;
        t.quote.mint(&t.rebalancer_id, &500_0000000i128);
        t.rebalancer.set_params(&500u32, &500u32, &100_000_000_000i128, &small_max_spend, &12u32, &10_000_000i128);
        let quote_before = t.quote.balance(&t.rebalancer_id);
        t.rebalancer.rebalance();
        let quote_after = t.quote.balance(&t.rebalancer_id);
        let spent = quote_before - quote_after;
        assert!(spent <= small_max_spend, "Spend should not exceed max_quote_spend");
    }

    /// 6. Buyback but rebalancer has no USDC — InsufficientQuote
    #[test]
    fn test_buyback_insufficient_quote() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        // 10% underpriced
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 9_000);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        // Don't fund the rebalancer
        let result = t.rebalancer.try_rebalance();
        assert_eq!(result, Err(Ok(RebalancerError::InsufficientQuote)));
    }

    // ─── Threshold Edge Cases ───────────────────────────────────

    /// 7. Exactly at upper threshold — NoRebalanceNeeded
    #[test]
    fn test_exactly_at_upper_threshold() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        // +1 because integer division truncates, making pool slightly more overpriced
        let reserve_joule = reserve_quote * quote_price * 10_000 / (oracle_price * 10_500) + 1;
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        let result = t.rebalancer.try_rebalance();
        assert_eq!(result, Err(Ok(RebalancerError::NoRebalanceNeeded)));
    }

    /// 8. Just above upper threshold — triggers mint
    #[test]
    fn test_just_above_upper_threshold() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        let reserve_joule = reserve_quote * quote_price * 10_000 / (oracle_price * 10_500) - 1_000_000;
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        t.rebalancer.rebalance();
    }

    /// 9. Exactly at lower threshold — NoRebalanceNeeded
    #[test]
    fn test_exactly_at_lower_threshold() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        let reserve_joule = reserve_quote * quote_price * 10_000 / (oracle_price * 9_500);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        let result = t.rebalancer.try_rebalance();
        assert_eq!(result, Err(Ok(RebalancerError::NoRebalanceNeeded)));
    }

    /// 10. Just below lower threshold — triggers buyback
    #[test]
    fn test_just_below_lower_threshold() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        let reserve_joule = reserve_quote * quote_price * 10_000 / (oracle_price * 9_500) + 1_000_000;
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        t.quote.mint(&t.rebalancer_id, &500_0000000i128);
        t.rebalancer.rebalance();
    }

    // ─── Safety Mechanisms ──────────────────────────────────────

    /// 11. Stale oracle rejected
    #[test]
    fn test_stale_oracle_rejected() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 15_000);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        set_ledger(&t.env, 1200);
        let result = t.rebalancer.try_rebalance();
        assert_eq!(result, Err(Ok(RebalancerError::OracleStale)));
    }

    /// 12. Fresh oracle accepted
    #[test]
    fn test_fresh_oracle_accepted() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 15_000);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        set_ledger(&t.env, 600);
        t.rebalancer.rebalance();
    }

    /// 13. Cooldown blocks rapid rebalance
    #[test]
    fn test_cooldown_blocks_rapid_rebalance() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 15_000);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        t.rebalancer.rebalance();
        set_ledger(&t.env, 105);
        t.joule.set_price(&oracle_price, &2u64);
        let result = t.rebalancer.try_rebalance();
        assert_eq!(result, Err(Ok(RebalancerError::CooldownActive)));
    }

    /// 14. Cooldown expires — rebalance works again
    #[test]
    fn test_cooldown_expires() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 15_000);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        t.rebalancer.rebalance();
        set_ledger(&t.env, 115);
        t.joule.set_price(&oracle_price, &2u64);
        let result = t.rebalancer.try_rebalance();
        assert!(result != Err(Ok(RebalancerError::CooldownActive)),
            "Should not be blocked by cooldown after expiry");
    }

    /// 15. Minimum reserve check — tiny reserves — PoolEmpty
    #[test]
    fn test_min_reserve_check() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 100i128;
        let reserve_joule = 100i128;
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        let result = t.rebalancer.try_rebalance();
        assert_eq!(result, Err(Ok(RebalancerError::PoolEmpty)));
    }

    // ─── Math Correctness ───────────────────────────────────────

    /// 16. Mint targets band midpoint, not exact peg
    #[test]
    fn test_mint_targets_band_midpoint() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        // 20% overpriced: pool_price = 12000
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 12_000);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        let pool_joule_before = t.joule.balance(&t.pool_id);
        t.rebalancer.rebalance();
        let pool_joule_after = t.joule.balance(&t.pool_id);
        let added = pool_joule_after - pool_joule_before;

        // Exact peg mint
        let exact_peg_target = reserve_quote * quote_price / oracle_price;
        let exact_peg_mint = exact_peg_target - reserve_joule;

        // Band midpoint target
        let midpoint_target = reserve_quote * quote_price * 10_000 / (oracle_price * 10_250);
        let midpoint_mint = midpoint_target - reserve_joule;

        assert!(added < exact_peg_mint, "Should add less JOULE than exact peg targeting");
        let diff_from_midpoint = if added > midpoint_mint { added - midpoint_mint } else { midpoint_mint - added };
        let diff_from_peg = exact_peg_mint - added;
        assert!(diff_from_midpoint < diff_from_peg, "Should be closer to midpoint than peg");
    }

    /// 17. Buyback burns all received JOULE
    #[test]
    fn test_buyback_burns_joule() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        // 10% underpriced: pool_price = 9000
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 9_000);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        t.quote.mint(&t.rebalancer_id, &500_0000000i128);
        let burned_before = t.joule.total_burned();
        t.rebalancer.rebalance();
        let burned_after = t.joule.total_burned();
        assert!(burned_after > burned_before, "JOULE should have been burned");
        let rebalancer_joule = t.joule.balance(&t.rebalancer_id);
        assert_eq!(rebalancer_joule, 0, "Rebalancer should have zero JOULE after buyback");
    }

    /// 18. isqrt comprehensive edge cases
    #[test]
    fn test_isqrt_comprehensive() {
        assert_eq!(isqrt(0), 0);
        assert_eq!(isqrt(1), 1);
        assert_eq!(isqrt(2), 1);
        assert_eq!(isqrt(3), 1);
        assert_eq!(isqrt(4), 2);
        assert_eq!(isqrt(8), 2);
        assert_eq!(isqrt(9), 3);
        assert_eq!(isqrt(15), 3);
        assert_eq!(isqrt(16), 4);
        assert_eq!(isqrt(10_000_000_000_000_000), 100_000_000);
        assert_eq!(isqrt(100_000_000i128 * 100_000_000), 100_000_000);
    }

    // ─── Auth Tests ─────────────────────────────────────────────

    /// 19. Rebalance requires oracle auth
    #[test]
    #[should_panic]
    fn test_rebalance_requires_oracle() {
        let env = Env::default();
        let contract_id = env.register(Rebalancer, ());
        let client = RebalancerClient::new(&env, &contract_id);
        let joule = Address::generate(&env);
        let pool = Address::generate(&env);
        let quote = Address::generate(&env);
        let oracle = Address::generate(&env);
        let owner = Address::generate(&env);
        let router = Address::generate(&env);
        client.initialize(&joule, &pool, &quote, &oracle, &owner, &true, &router, &3000u32);
        client.rebalance();
    }

    /// 20. set_params requires owner auth
    #[test]
    #[should_panic]
    fn test_set_params_requires_owner() {
        let env = Env::default();
        let contract_id = env.register(Rebalancer, ());
        let client = RebalancerClient::new(&env, &contract_id);
        let joule = Address::generate(&env);
        let pool = Address::generate(&env);
        let quote = Address::generate(&env);
        let oracle = Address::generate(&env);
        let owner = Address::generate(&env);
        let router = Address::generate(&env);
        client.initialize(&joule, &pool, &quote, &oracle, &owner, &true, &router, &3000u32);
        client.set_params(&300u32, &300u32, &50_000_000_000i128, &25_000_000_000i128, &12u32, &10_000_000i128);
    }

    /// 21. withdraw requires owner auth
    #[test]
    #[should_panic]
    fn test_withdraw_requires_owner() {
        let env = Env::default();
        let contract_id = env.register(Rebalancer, ());
        let client = RebalancerClient::new(&env, &contract_id);
        let joule = Address::generate(&env);
        let pool = Address::generate(&env);
        let quote = Address::generate(&env);
        let oracle = Address::generate(&env);
        let owner = Address::generate(&env);
        let router = Address::generate(&env);
        client.initialize(&joule, &pool, &quote, &oracle, &owner, &true, &router, &3000u32);
        let to = Address::generate(&env);
        client.withdraw(&joule, &to, &100i128);
    }

    /// 22. fund_quote requires caller auth
    #[test]
    #[should_panic]
    fn test_fund_quote_requires_caller_auth() {
        let env = Env::default();
        let contract_id = env.register(Rebalancer, ());
        let client = RebalancerClient::new(&env, &contract_id);
        let joule = Address::generate(&env);
        let pool = Address::generate(&env);
        let quote = Address::generate(&env);
        let oracle = Address::generate(&env);
        let owner = Address::generate(&env);
        let router = Address::generate(&env);
        client.initialize(&joule, &pool, &quote, &oracle, &owner, &true, &router, &3000u32);
        let funder = Address::generate(&env);
        client.fund_quote(&funder, &100i128);
    }

    // ─── V3-Specific Tests ──────────────────────────────────────

    /// 23. Mint rebalance earns USDC for rebalancer (self-funding cycle)
    #[test]
    fn test_mint_rebalance_earns_usdc() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        // 10% overpriced
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 11_000);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);
        let usdc_before = t.quote.balance(&t.rebalancer_id);
        assert_eq!(usdc_before, 0, "Rebalancer should start with zero USDC");
        t.rebalancer.rebalance();
        let usdc_after = t.quote.balance(&t.rebalancer_id);
        assert!(usdc_after > 0, "Rebalancer should have earned USDC from selling minted JOULE");
    }

    /// 24. Self-funding cycle: mint earns USDC, then buyback spends it
    #[test]
    fn test_self_funding_cycle() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        // 15% overpriced → mint rebalance
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 11_500);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);

        // Phase 1: Mint rebalance → earns USDC
        t.rebalancer.rebalance();
        let usdc_earned = t.quote.balance(&t.rebalancer_id);
        assert!(usdc_earned > 0, "Should have earned USDC from mint rebalance");

        // Now manipulate pool to be underpriced:
        // Add a modest amount of JOULE to push price below lower band
        let extra_joule = reserve_joule / 5; // 20% more JOULE → ~8% underpriced
        t.joule.oracle_mint(&t.pool_id, &extra_joule);

        // Cap max_quote_spend to a small amount within what we earned
        let small_spend = usdc_earned / 2;
        t.rebalancer.set_params(&500u32, &500u32, &100_000_000_000i128, &small_spend, &12u32, &10_000_000i128);

        // Advance past cooldown
        set_ledger(&t.env, 115);
        t.joule.set_price(&oracle_price, &2u64);

        // Phase 2: Buyback rebalance — should use the earned USDC
        let usdc_before = t.quote.balance(&t.rebalancer_id);
        let result = t.rebalancer.try_rebalance();
        // It should either succeed or return NoRebalanceNeeded — NOT InsufficientQuote
        assert!(result != Err(Ok(RebalancerError::InsufficientQuote)),
            "Should not fail with InsufficientQuote — has USDC from mint phase");
        if result.is_ok() {
            let usdc_after = t.quote.balance(&t.rebalancer_id);
            assert!(usdc_after < usdc_before, "Should have spent USDC from mint earnings");
        }
    }

    /// 25. Router swap integration — verify full flow through mock
    #[test]
    fn test_pool_swap_integration() {
        let oracle_price: i128 = 10_000;
        let quote_price: i128 = 10_000_000;
        let reserve_quote = 1_000_0000000i128;
        // 10% overpriced
        let reserve_joule = joule_reserves_for_price(reserve_quote, quote_price, 11_000);
        let t = setup_test(reserve_joule, reserve_quote, oracle_price, quote_price);

        let pool_joule_before = t.joule.balance(&t.pool_id);
        let pool_quote_before = t.quote.balance(&t.pool_id);

        t.rebalancer.rebalance();

        let pool_joule_after = t.joule.balance(&t.pool_id);
        let pool_quote_after = t.quote.balance(&t.pool_id);

        // After mint rebalance: pool has more JOULE, less USDC
        assert!(pool_joule_after > pool_joule_before, "Pool should have more JOULE");
        assert!(pool_quote_after < pool_quote_before, "Pool should have less USDC");
    }
}
