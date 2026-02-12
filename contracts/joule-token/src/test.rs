#![cfg(test)]

use soroban_sdk::{testutils::Address as _, Address, Env, String};

use crate::JouleTokenClient;

fn setup() -> (Env, JouleTokenClient<'static>, Address, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(crate::JouleToken, ());
    let client = JouleTokenClient::new(&env, &contract_id);

    let owner = Address::generate(&env);
    let oracle = Address::generate(&env);
    let agent = Address::generate(&env);

    client.initialize(&owner, &oracle);

    (env, client, owner, oracle, agent)
}

// ─── Basic Token Tests ──────────────────────────────────────────

#[test]
fn test_initialize_and_metadata() {
    let (_env, client, _owner, _oracle, _agent) = setup();
    assert_eq!(client.decimals(), 7);
    assert_eq!(client.name(), String::from_str(&_env, "Joule Compute Credit"));
    assert_eq!(client.symbol(), String::from_str(&_env, "JOULE"));
    assert_eq!(client.total_supply(), 0);
}

#[test]
fn test_mint_and_balance() {
    let (_env, client, _owner, _oracle, agent) = setup();
    client.mint(&agent, &1_000_000_000); // 100 JOULE
    assert_eq!(client.balance(&agent), 1_000_000_000);
    assert_eq!(client.total_minted(), 1_000_000_000);
    assert_eq!(client.total_supply(), 1_000_000_000);
}

#[test]
fn test_transfer_no_fee() {
    let (env, client, _owner, _oracle, agent) = setup();
    let recipient = Address::generate(&env);

    client.mint(&agent, &1_000_000_000); // 100 JOULE
    client.transfer(&agent, &recipient, &1_000_000_000);

    // No fee — recipient gets full amount
    assert_eq!(client.balance(&recipient), 1_000_000_000);
    assert_eq!(client.balance(&agent), 0);
}

// ─── Oracle Price Feed Tests ────────────────────────────────────

#[test]
fn test_set_price_basic() {
    let (_env, client, _owner, _oracle, _agent) = setup();

    // Set initial price: $0.000763 = 7630
    client.set_price(&7_630, &1_u64);
    let (price, _ledger) = client.get_price();
    assert_eq!(price, 7_630);
}

#[test]
fn test_set_price_within_swing() {
    let (_env, client, _owner, _oracle, _agent) = setup();

    client.set_price(&10_000, &1_u64);

    // 15% increase (within 20% limit)
    client.set_price(&11_500, &2_u64);
    let (price, _) = client.get_price();
    assert_eq!(price, 11_500);

    // 15% decrease (within 20% limit)
    client.set_price(&9_775, &3_u64);
    let (price, _) = client.get_price();
    assert_eq!(price, 9_775);
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_set_price_stale_nonce() {
    let (_env, client, _owner, _oracle, _agent) = setup();
    client.set_price(&7_630, &5_u64);
    // Same nonce should fail
    client.set_price(&7_700, &5_u64);
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_set_price_old_nonce() {
    let (_env, client, _owner, _oracle, _agent) = setup();
    client.set_price(&7_630, &5_u64);
    // Lower nonce should fail
    client.set_price(&7_700, &3_u64);
}

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn test_set_price_below_floor() {
    let (_env, client, _owner, _oracle, _agent) = setup();
    // Default floor is 1,000. Price of 500 should fail.
    client.set_price(&500, &1_u64);
}

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn test_set_price_above_ceiling() {
    let (_env, client, _owner, _oracle, _agent) = setup();
    // Default ceiling is 100,000. Price of 200,000 should fail.
    client.set_price(&200_000, &1_u64);
}

#[test]
#[should_panic(expected = "Error(Contract, #9)")]
fn test_set_price_circuit_breaker() {
    let (_env, client, _owner, _oracle, _agent) = setup();
    client.set_price(&10_000, &1_u64);
    // 25% swing should trigger circuit breaker (>20%)
    client.set_price(&12_500, &2_u64);
}

#[test]
fn test_set_price_exact_20_percent_allowed() {
    let (_env, client, _owner, _oracle, _agent) = setup();
    client.set_price(&10_000, &1_u64);
    // Exactly 20% swing should be allowed
    client.set_price(&12_000, &2_u64);
    let (price, _) = client.get_price();
    assert_eq!(price, 12_000);
}

// ─── Oracle Mint Tests ──────────────────────────────────────────

#[test]
fn test_oracle_mint() {
    let (_env, client, _owner, _oracle, agent) = setup();
    client.oracle_mint(&agent, &500_000_000); // 50 JOULE
    assert_eq!(client.balance(&agent), 500_000_000);
    assert_eq!(client.total_minted(), 500_000_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")]
fn test_oracle_mint_exceeds_cap() {
    let (_env, client, _owner, _oracle, agent) = setup();
    // Default cap is 100_000_000_000 (10,000 JOULE)
    // Try to mint 20,000 JOULE
    client.oracle_mint(&agent, &200_000_000_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #2)")]
fn test_oracle_mint_zero() {
    let (_env, client, _owner, _oracle, agent) = setup();
    client.oracle_mint(&agent, &0);
}

// ─── Owner Override Tests ───────────────────────────────────────

#[test]
fn test_owner_set_price_skips_circuit_breaker() {
    let (_env, client, _owner, _oracle, _agent) = setup();
    client.set_price(&10_000, &1_u64);
    // 50% swing — would fail set_price but owner override skips circuit breaker
    client.owner_set_price(&15_000, &2_u64);
    let (price, _) = client.get_price();
    assert_eq!(price, 15_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_owner_set_price_stale_nonce() {
    let (_env, client, _owner, _oracle, _agent) = setup();
    client.set_price(&10_000, &5_u64);
    client.owner_set_price(&15_000, &3_u64); // stale
}

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn test_owner_set_price_out_of_bounds() {
    let (_env, client, _owner, _oracle, _agent) = setup();
    // Still respects bounds
    client.owner_set_price(&500_000, &1_u64);
}

// ─── Configuration Tests ────────────────────────────────────────

#[test]
fn test_set_mint_cap() {
    let (_env, client, _owner, _oracle, agent) = setup();
    assert_eq!(client.mint_cap(), 100_000_000_000); // default

    client.set_mint_cap(&50_000_000_000); // 5,000 JOULE
    assert_eq!(client.mint_cap(), 50_000_000_000);

    // Now mint at new cap limit
    client.oracle_mint(&agent, &50_000_000_000);
    assert_eq!(client.balance(&agent), 50_000_000_000);
}

#[test]
fn test_set_price_bounds() {
    let (_env, client, _owner, _oracle, _agent) = setup();
    let (floor, ceiling) = client.price_bounds();
    assert_eq!(floor, 1_000);
    assert_eq!(ceiling, 100_000);

    client.set_price_bounds(&5_000, &50_000);
    let (floor, ceiling) = client.price_bounds();
    assert_eq!(floor, 5_000);
    assert_eq!(ceiling, 50_000);
}

#[test]
fn test_custom_bounds_enforced() {
    let (_env, client, _owner, _oracle, _agent) = setup();
    client.set_price_bounds(&5_000, &50_000);

    // Price within new bounds works
    client.set_price(&7_630, &1_u64);
    let (price, _) = client.get_price();
    assert_eq!(price, 7_630);
}

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn test_custom_bounds_reject_below() {
    let (_env, client, _owner, _oracle, _agent) = setup();
    client.set_price_bounds(&5_000, &50_000);
    client.set_price(&3_000, &1_u64); // below new floor
}

// ─── Burn for Compute Tests ─────────────────────────────────────

#[test]
fn test_burn_for_compute() {
    let (_env, client, _owner, _oracle, agent) = setup();
    client.mint(&agent, &1_000_000_000);
    client.burn_for_compute(&agent, &500_000_000);
    assert_eq!(client.balance(&agent), 500_000_000);
    assert_eq!(client.total_burned(), 500_000_000);
    assert_eq!(client.circulating_supply(), 500_000_000);
}
