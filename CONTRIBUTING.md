# Contributing

Report vulnerabilities as [SECURITY.md](SECURITY.md) says, not in issues.

## Rust version

The crates build with Rust 1.89 and later, except `mercuriod`, which
follows wasmtime and needs Rust 1.97. CI checks both:

```bash
cargo +1.89 check --workspace --all-features --exclude mercuriod
cargo +1.97 check -p mercuriod --all-features
```

## Building and testing

```bash
cargo test --workspace --all-features
```

The tests run the broker and the client over real connections.

The `mercuriod` plugin tests build the example plugins in
`examples/plugins/` and `mercuriod/test-plugins/` for WebAssembly, which
needs the target:

```bash
rustup target add wasm32-wasip2
```

To build the example plugins on their own, as CI does before the tests:

```bash
for p in examples/plugins/*/; do cargo build --release --target wasm32-wasip2 --manifest-path "$p/Cargo.toml" --target-dir target/plugins; done
```

### PostgreSQL

The PostgreSQL storage tests skip unless `MERCURIO_TEST_POSTGRES_URL`
names a database. They take the database over and empty it, so give them
one of their own. CI runs them against the `postgres:17-alpine` image
with `POSTGRES_PASSWORD=mercurio`:

```bash
MERCURIO_TEST_POSTGRES_URL=postgres://postgres:mercurio@localhost:5432/postgres \
  cargo test --workspace --all-features
```

## Formatting and lints

CI fails on unformatted code, clippy warnings and rustdoc warnings:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

The example plugins are not workspace members, so they are checked on
their own:

```bash
for p in examples/plugins/*/ mercuriod/test-plugins/*/; do cargo fmt --manifest-path "$p/Cargo.toml" -- --check; done
for p in examples/plugins/*/ mercuriod/test-plugins/*/; do cargo clippy --release --target wasm32-wasip2 --manifest-path "$p/Cargo.toml" --target-dir target/plugins -- -D warnings; done
```

`mercurio-core` and `mercurio-packets` are `no_std`. Check them for a
target without `std` after changing them:

```bash
rustup target add thumbv7em-none-eabihf
cargo check -p mercurio-core -p mercurio-packets --no-default-features --target thumbv7em-none-eabihf
```

Dependencies must pass `cargo deny check`, configured in `deny.toml`.

## Commits

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/),
such as `fix(server): ...` or `feat(client)!: ...` for a breaking change.
release-plz builds the version bumps and changelogs from them. The body
says why the change is needed.

Sign off every commit (`git commit -s`), certifying the
[Developer Certificate of Origin](https://developercertificate.org/).

## Licence

Contributions are dual-licensed under MIT OR Apache-2.0, as the
[README](README.md#license) says.
