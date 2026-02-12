# JOULE Smart Contracts

Soroban smart contracts for the JOULE prepaid AI compute credit system on Stellar.

## Contracts

### joule-token

SEP-41 compliant fungible token — prepaid AI inference credits.

- **Mainnet**: `CABREAOOPRZIIF6NKDOGPRHEMOYZIOJGTNPYPPSHA2EH7P2GW5JZMEKT`
- Fee-free transfers (SEP-41 + Soroswap/SDEX compatible)
- Oracle-controlled price feed with circuit breaker
- Owner + oracle dual-auth model
- Built on OpenZeppelin stellar-contracts v0.6.0

### rebalancer

Automated market maker peg maintenance via SushiSwap V3 pool.

- **Mainnet**: `CCC2V3YONPLSMUSUMVJHQ5ASVEGNKKFZR6CKJCM5OLTJNZIMU45VFRG7`
- Mint-and-sell when pool price > oracle + 5%
- Buyback-and-burn when pool price < oracle - 5%
- Configurable bands, cooldown, slippage protection
- Self-funding: mint rebalances earn USDC for future buybacks

## On-chain WASM Hashes

| Contract | On-chain SHA256 |
|----------|----------------|
| joule-token | `dd83430703a4ff25047b46f621033a1633cd37abc7e8ebafd4a811bb9e287435` |
| rebalancer | `0ab013dd9f4373f3052ad39dbb4f60def9495b77d301a3aab08405ac2b1052d8` |

You can verify these hashes by fetching the on-chain WASM:

```bash
stellar contract fetch --id CABREAOOPRZIIF6NKDOGPRHEMOYZIOJGTNPYPPSHA2EH7P2GW5JZMEKT --network mainnet -o joule_token.wasm
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
