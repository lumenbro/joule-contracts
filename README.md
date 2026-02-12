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

## Build

```bash
stellar contract build
```

Optimized WASM output in `target/wasm32-unknown-unknown/release/`.

## Test

```bash
cargo test
```

## Verification

Contract WASMs are verified via [StellarExpert](https://stellar.expert/explorer/public/contract/validation) using the [soroban-build-workflow](https://github.com/stellar-expert/soroban-build-workflow). Tag a release to trigger automated build + verification.

## License

MIT
