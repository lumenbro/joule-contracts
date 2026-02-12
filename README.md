# JOULE Smart Contracts

Soroban smart contracts for the JOULE prepaid AI compute credit system on Stellar.

1 JOULE = 1,000 Joules of estimated AI inference energy. Pay-per-query via the [x402 HTTP payment protocol](https://www.x402.org/).

## Contracts

### joule-token

SEP-41 compliant fungible token — prepaid AI inference credits.

- **Mainnet**: `CB3PT4TU4LAGEWTCPBIPQVD3S2V5NOMQVUNFYGYBDO64YVKYYJJGVXNF`
- Fee-free transfers (SEP-41 + Soroswap/SDEX compatible)
- Oracle-controlled price feed with circuit breaker (20% max swing, price bounds)
- Owner + oracle dual-auth model
- Upgradeable via `upgrade()` (owner-gated)
- Built on [OpenZeppelin stellar-contracts v0.6.0](https://github.com/OpenZeppelin/stellar-contracts)

### rebalancer

Automated market maker peg maintenance via SushiSwap V3 pool.

- **Mainnet**: `CB4UL4XX4YENBHM2U6PQV7I3YS7YEBNNTHLZAJ4OC7L7R7XSH2D3G4DU`
- Mint-and-sell when pool price > oracle + 5%
- Buyback-and-burn when pool price < oracle - 5%
- Configurable bands, cooldown, slippage protection
- Self-funding: mint rebalances earn USDC for future buybacks
- Upgradeable via `upgrade()` (owner-gated)

### Auth Chain

```
Oracle Service -> Rebalancer -> JOULE Token
```

The rebalancer is the oracle on the JOULE token. It forwards `set_price()` and `oracle_mint()`.

## On-chain WASM Hashes

| Contract | On-chain SHA256 |
|----------|----------------|
| joule-token v0.2.0 | `23715363ff5f399eab3c57b671270d193b6452616aafef32df8d44caeeca2509` |
| rebalancer v0.2.0 | `88fb527ce0f55d2b68ce74c2edbf7e9f7b938f67acb3268a8363e103f7fee7f6` |

You can verify these hashes by fetching the on-chain WASM:

```bash
stellar contract fetch --id CB3PT4TU4LAGEWTCPBIPQVD3S2V5NOMQVUNFYGYBDO64YVKYYJJGVXNF --network mainnet -o joule_token.wasm
sha256sum joule_token.wasm
```

## Build

Requires [Stellar CLI](https://github.com/stellar/stellar-cli) v23.4.1 and Rust 1.92.0.

```bash
stellar contract build --optimize
```

## Test

```bash
cargo test
```

## Verification

Tagged releases trigger a GitHub Actions workflow that:
1. Builds optimized WASMs with pinned Stellar CLI + Rust versions
2. Computes and publishes SHA256 hashes
3. Submits to [StellarExpert](https://stellar.expert) for contract validation
4. Attests build provenance via GitHub Attestations

## License

MIT
