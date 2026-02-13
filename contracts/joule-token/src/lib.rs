#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token::TokenInterface, Address, BytesN,
    Env, MuxedAddress, String, Symbol,
};
use stellar_access::ownable::{self, Ownable};
use stellar_contract_utils::pausable::{self, Pausable};
use stellar_macros::{only_owner, when_not_paused};
use stellar_tokens::fungible::Base;

mod oracle;
#[cfg(test)]
mod test;

pub use oracle::PriceData;

// TTL constants: extend instance storage proactively to prevent archival
const TTL_THRESHOLD: u32 = 17_280; // ~1 day at 5s/ledger
const TTL_EXTEND_TO: u32 = 518_400; // ~30 days

// ─── Storage Keys ────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    OracleAddress,
    TotalMinted,
    TotalBurned,
    OraclePrice,
    OracleNonce,
    OracleLedger,
    OraclePriceFloor,
    OraclePriceCeiling,
    OracleMintCap,
}

// ─── Errors ──────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum JouleError {
    InsufficientBalance = 1,
    InvalidAmount = 2,
    Unauthorized = 3,
    OracleOnly = 5,
    AlreadyProcessed = 6,
    StaleNonce = 7,
    PriceOutOfBounds = 8,
    CircuitBreakerTripped = 9,
    MintCapExceeded = 10,
    PriceNotSet = 11,
}

// ─── Contract ────────────────────────────────────────────────────

#[contract]
pub struct JouleToken;

// ─── SEP-41 Token Interface (canonical trait for indexer detection) ──

#[contractimpl]
impl TokenInterface for JouleToken {
    fn allowance(env: Env, from: Address, spender: Address) -> i128 {
        Base::allowance(&env, &from, &spender)
    }

    fn approve(env: Env, from: Address, spender: Address, amount: i128, expiration_ledger: u32) {
        Base::approve(&env, &from, &spender, amount, expiration_ledger);
    }

    fn balance(env: Env, id: Address) -> i128 {
        Base::balance(&env, &id)
    }

    fn transfer(env: Env, from: Address, to: MuxedAddress, amount: i128) {
        Base::transfer(&env, &from, &to, amount);
    }

    fn transfer_from(env: Env, spender: Address, from: Address, to: Address, amount: i128) {
        Base::transfer_from(&env, &spender, &from, &to, amount);
    }

    fn burn(env: Env, from: Address, amount: i128) {
        Base::burn(&env, &from, amount);
    }

    fn burn_from(env: Env, spender: Address, from: Address, amount: i128) {
        Base::burn_from(&env, &spender, &from, amount);
    }

    fn decimals(env: Env) -> u32 {
        Base::decimals(&env)
    }

    fn name(env: Env) -> String {
        Base::name(&env)
    }

    fn symbol(env: Env) -> String {
        Base::symbol(&env)
    }
}

// Ownable (2-step transfer)
#[contractimpl]
impl Ownable for JouleToken {}

// Pausable (owner-only)
#[contractimpl]
impl Pausable for JouleToken {
    fn pause(e: &Env, _caller: Address) {
        ownable::enforce_owner_auth(e);
        pausable::pause(e);
    }

    fn unpause(e: &Env, _caller: Address) {
        ownable::enforce_owner_auth(e);
        pausable::unpause(e);
    }
}

// ─── JOULE-Specific Functions ────────────────────────────────────

#[contractimpl]
impl JouleToken {
    /// Total token supply (not part of TokenInterface but commonly expected).
    pub fn total_supply(env: Env) -> i128 {
        Base::total_supply(&env)
    }

    pub fn initialize(env: Env, owner: Address, oracle: Address) {
        ownable::set_owner(&env, &owner);
        Base::set_metadata(
            &env,
            7,
            String::from_str(&env, "Joule Compute Credit"),
            String::from_str(&env, "JOULE"),
        );

        env.storage()
            .instance()
            .set(&DataKey::OracleAddress, &oracle);
        env.storage()
            .instance()
            .set(&DataKey::TotalMinted, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::TotalBurned, &0i128);
    }

    #[only_owner]
    #[when_not_paused]
    pub fn mint(env: Env, to: Address, amount: i128) {
        assert!(amount > 0, "Amount must be positive");
        Base::update(&env, None, Some(&to), amount);

        let total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalMinted)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalMinted, &(total + amount));

        env.events()
            .publish((Symbol::new(&env, "mint"),), (to, amount));
    }

    #[when_not_paused]
    pub fn burn_for_compute(env: Env, from: Address, amount: i128) {
        from.require_auth();
        assert!(amount > 0, "Amount must be positive");
        Base::update(&env, Some(&from), None, amount);

        let total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalBurned)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalBurned, &(total + amount));

        env.events()
            .publish((Symbol::new(&env, "burn_for_compute"),), (from, amount));
    }

    #[only_owner]
    pub fn set_oracle(env: Env, oracle: Address) {
        env.storage()
            .instance()
            .set(&DataKey::OracleAddress, &oracle);
    }

    pub fn oracle(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::OracleAddress)
            .expect("Oracle not set")
    }

    pub fn total_minted(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalMinted)
            .unwrap_or(0)
    }

    pub fn total_burned(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalBurned)
            .unwrap_or(0)
    }

    pub fn circulating_supply(env: Env) -> i128 {
        let minted: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalMinted)
            .unwrap_or(0);
        let burned: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalBurned)
            .unwrap_or(0);
        minted - burned
    }

    // ─── Oracle Price Feed ──────────────────────────────────────

    /// Oracle posts JOULE_USD price. Validates nonce, bounds, circuit breaker.
    pub fn set_price(env: Env, price_scaled: i128, nonce: u64) -> Result<(), JouleError> {
        let oracle_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::OracleAddress)
            .expect("Oracle not set");
        oracle_addr.require_auth();
        env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);

        // Nonce must be strictly increasing
        let current_nonce = oracle::get_nonce(&env);
        if nonce <= current_nonce {
            return Err(JouleError::StaleNonce);
        }

        // Price must be within bounds
        oracle::check_bounds(&env, price_scaled)?;

        // Circuit breaker: if there's an existing price, check swing
        if let Some(existing) = oracle::get_price_data(&env) {
            oracle::check_circuit_breaker(existing.price, price_scaled)?;
        }

        let data = oracle::PriceData {
            price: price_scaled,
            nonce,
            ledger: env.ledger().sequence(),
        };
        oracle::set_price_data(&env, &data);

        env.events().publish(
            (Symbol::new(&env, "price_updated"),),
            (price_scaled, nonce, env.ledger().sequence()),
        );

        Ok(())
    }

    /// Returns (price_scaled, last_updated_ledger). Panics if no price set.
    pub fn get_price(env: Env) -> (i128, u32) {
        let data = oracle::get_price_data(&env).expect("Price not set");
        (data.price, data.ledger)
    }

    /// Oracle mints JOULE up to mint_cap. Respects pause.
    #[when_not_paused]
    pub fn oracle_mint(env: Env, to: Address, amount: i128) -> Result<(), JouleError> {
        let oracle_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::OracleAddress)
            .expect("Oracle not set");
        oracle_addr.require_auth();
        env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);

        if amount <= 0 {
            return Err(JouleError::InvalidAmount);
        }

        let cap = oracle::get_mint_cap(&env);
        if amount > cap {
            return Err(JouleError::MintCapExceeded);
        }

        Base::update(&env, None, Some(&to), amount);

        let total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalMinted)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalMinted, &(total + amount));

        env.events()
            .publish((Symbol::new(&env, "oracle_mint"),), (to, amount));

        Ok(())
    }

    /// Owner emergency price override — skips circuit breaker.
    #[only_owner]
    pub fn owner_set_price(env: Env, price_scaled: i128, nonce: u64) -> Result<(), JouleError> {
        let current_nonce = oracle::get_nonce(&env);
        if nonce <= current_nonce {
            return Err(JouleError::StaleNonce);
        }

        oracle::check_bounds(&env, price_scaled)?;

        let data = oracle::PriceData {
            price: price_scaled,
            nonce,
            ledger: env.ledger().sequence(),
        };
        oracle::set_price_data(&env, &data);

        env.events().publish(
            (Symbol::new(&env, "price_override"),),
            (price_scaled, nonce, env.ledger().sequence()),
        );

        Ok(())
    }

    /// Owner sets max JOULE per oracle_mint call.
    #[only_owner]
    pub fn set_mint_cap(env: Env, cap: i128) {
        assert!(cap > 0, "Mint cap must be positive");
        env.storage()
            .instance()
            .set(&DataKey::OracleMintCap, &cap);
    }

    /// Read current mint cap.
    pub fn mint_cap(env: Env) -> i128 {
        oracle::get_mint_cap(&env)
    }

    /// Owner sets price floor and ceiling.
    #[only_owner]
    pub fn set_price_bounds(env: Env, floor: i128, ceiling: i128) {
        assert!(floor > 0, "Floor must be positive");
        assert!(ceiling > floor, "Ceiling must exceed floor");
        env.storage()
            .instance()
            .set(&DataKey::OraclePriceFloor, &floor);
        env.storage()
            .instance()
            .set(&DataKey::OraclePriceCeiling, &ceiling);
    }

    /// Read price floor and ceiling.
    pub fn price_bounds(env: Env) -> (i128, i128) {
        (
            oracle::get_price_floor(&env),
            oracle::get_price_ceiling(&env),
        )
    }

    /// Owner upgrades the contract WASM. Requires owner auth.
    #[only_owner]
    pub fn upgrade(env: Env, wasm_hash: BytesN<32>) {
        env.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
        env.deployer().update_current_contract_wasm(wasm_hash);
    }
}
