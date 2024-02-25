# Ribs

## Testing

### Kani

To install Kani, follow the instructions at [Kani install guide](https://model-checking.github.io/kani/install-guide.html#installing-the-latest-version).

To check the `kani` proofs;

```bash
cargo kani
```

### Loom

```bash
LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo test --release -- loom_verification
```

### Miri

```bash
cargo +nightly miri test
```