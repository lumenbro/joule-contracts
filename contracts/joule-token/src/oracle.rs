use soroban_sdk::{contracttype, Env};

use crate::{DataKey, JouleError};

/// Price data stored by the oracle
#[contracttype]
#[derive(Clone, Debug)]
pub struct PriceData {
    /// JOULE_USD price in 7-decimal fixed-point (e.g., 7630 = $0.000763)
    pub price: i128,
    /// Strictly increasing nonce to prevent replay
    pub nonce: u64,
    /// Ledger sequence when price was last updated
    pub ledger: u32,
}

// ─── Constants ──────────────────────────────────────────────────

/// Default price floor: 1,000 in 7-decimal = $0.0001
pub const DEFAULT_PRICE_FLOOR: i128 = 1_000;

/// Default price ceiling: 100,000 in 7-decimal = $0.01
pub const DEFAULT_PRICE_CEILING: i128 = 100_000;

/// Default mint cap per oracle_mint call: 10,000 JOULE (7 decimals)
pub const DEFAULT_MINT_CAP: i128 = 100_000_000_000;

/// Max price swing per update: 2,000 basis points = 20%
pub const MAX_SWING_BPS: i128 = 2_000;

// ─── Helpers ────────────────────────────────────────────────────

pub fn get_price_data(env: &Env) -> Option<PriceData> {
    env.storage().instance().get(&DataKey::OraclePrice)
}

pub fn set_price_data(env: &Env, data: &PriceData) {
    env.storage().instance().set(&DataKey::OraclePrice, data);
    env.storage()
        .instance()
        .set(&DataKey::OracleNonce, &data.nonce);
    env.storage()
        .instance()
        .set(&DataKey::OracleLedger, &data.ledger);
}

pub fn get_nonce(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&DataKey::OracleNonce)
        .unwrap_or(0u64)
}

pub fn get_mint_cap(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::OracleMintCap)
        .unwrap_or(DEFAULT_MINT_CAP)
}

pub fn get_price_floor(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::OraclePriceFloor)
        .unwrap_or(DEFAULT_PRICE_FLOOR)
}

pub fn get_price_ceiling(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::OraclePriceCeiling)
        .unwrap_or(DEFAULT_PRICE_CEILING)
}

/// Check if price is within floor/ceiling bounds
pub fn check_bounds(env: &Env, price: i128) -> Result<(), JouleError> {
    let floor = get_price_floor(env);
    let ceiling = get_price_ceiling(env);
    if price < floor || price > ceiling {
        return Err(JouleError::PriceOutOfBounds);
    }
    Ok(())
}

/// Circuit breaker: rejects >20% swing from previous price.
/// Uses multiplication to avoid division: |new - old| * 10000 <= MAX_SWING_BPS * old
pub fn check_circuit_breaker(old_price: i128, new_price: i128) -> Result<(), JouleError> {
    let diff = if new_price > old_price {
        new_price - old_price
    } else {
        old_price - new_price
    };
    // diff * 10_000 <= MAX_SWING_BPS * old_price
    if diff * 10_000 > MAX_SWING_BPS * old_price {
        return Err(JouleError::CircuitBreakerTripped);
    }
    Ok(())
}
