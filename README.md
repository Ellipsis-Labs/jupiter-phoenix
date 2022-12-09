# Jupiter Phoenix Integration

This module implements the `Amm` trait defined [here](https://github.com/jup-ag/rust-amm-implementation).

To test, simply run:

```
cargo test -- --nocapture
```

This will print out a quote for selling 1000 USDC against the Phoenix devnet SOL/USDC market. Sample output:
```
Getting quote for selling 1000 SOL
Quote result: 13792.60522
```
