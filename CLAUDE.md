# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Common commands

```bash
# Build everything (CI uses --all-features)
cargo build --all-features

# Run the full unit-test suite (CI command)
cargo test --all
cargo test -p <crate>                       # one crate, e.g. -p cache
cargo test -p http-service <test_name>      # one test by substring

# Lint / format (CI runs both with -Dwarnings)
cargo fmt --all -- --check
cargo clippy --all-targets --all-features

# Run the local CLI server against a wasm app
cargo run --bin fastedge-run -- http --port 8080 --wasm ./my_app.wasm
```

`RUSTFLAGS="-Dwarnings"` is the CI default — fix warnings before pushing.

## Repository setup gotchas

- **WIT is vendored via git subtree.** `crates/reactor/wit/` is committed directly in this repo (no `git submodule update` needed after clone).
- **WIT sync remote.** Use remote alias `fastedge-wit` (`https://github.com/G-Core/FastEdge-wit.git`) for updates:
  - one-time setup (if missing): `git remote add fastedge-wit https://github.com/G-Core/FastEdge-wit.git`
  - pull: `git subtree pull --prefix=crates/reactor/wit fastedge-wit main --squash`
  - push: `git subtree push --prefix=crates/reactor/wit fastedge-wit main`
- **Custom Wasmtime fork.** All `wasmtime-*` deps point at `github.com/G-Core/wasmtime.git#release-36.0.0`. `.cargo/config.toml` contains a commented-out `[patch]` block that redirects those to a local sibling `../wasmtime` checkout — uncomment when developing against a local Wasmtime tree.
- **Wasm components vs core modules.** `componentize_if_necessary` (in `crates/runtime/src/lib.rs`) auto-wraps core modules using the bundled Preview1 adapter (`crates/runtime/src/adapters/wasi_snapshot_preview1.reactor.wasm`). Don't replace that adapter casually — it's pinned to the Wasmtime fork.

## Architecture

The workspace is a layered Wasm execution stack. Top to bottom:

```
src/                 fastedge-run CLI binary (the only consumer of everything below)
crates/http-service/ Hyper server + per-request executor (HttpExecutorImpl, WasiHttpExecutorImpl)
crates/runtime/      Wasmtime engine, WasmConfig, Data<T> store state, ProxyLimiter, pooling alloc
crates/reactor/      `bindgen!` of the FastEdge WIT world (host-trait surface for guests)
crates/http-backend/ Outbound HTTP client used by guest WASI-HTTP requests
crates/cache/        cache-sync host functions, CacheBackend trait
crates/key-value-store/  KV host functions (Redis-backed when feature `redis` is on)
crates/secret/       Secret variable host functions
crates/utils/        Dictionary host functions
```

### The host-state hub: `runtime::Data<T>`

Every Wasmtime `Store` holds a `Data<T>` (in `crates/runtime/src/lib.rs`). It bundles **every** host capability the guest can call: `WasiCtx` (Preview1 *or* Preview2 — the `Wasi` enum), `WasiHttpCtx`, `WasiNnCtx`, `SecretStore`, `key_value_store::StoreImpl`, `Dictionary`, `Utils`, and `cache::CacheImpl`. `T` is whatever per-request payload the embedder threads through (e.g. `HttpState`). Adding a new host interface means: write the WIT, add a field to `Data<T>`, register it in the linker in `runtime::WasmEngineBuilder`, and surface a constructor option on the appropriate trait (`ContextT` or `App`).

### Two parallel WASI worlds

`Data<T>` supports both Preview 1 and Preview 2 via the `Wasi` enum and `WasiVersion`. `preview1_wasi_ctx_mut` / `preview2_wasi_ctx_mut` panic if called against the wrong variant — pick the variant via `StoreBuilder::new(engine, version)`. The CLI selects via the `--wasi-http` flag (Preview 2 path).

### Executor selection

`http-service` exposes two executors behind the same `HttpExecutor` trait:
- `HttpExecutorImpl` — the FastEdge `http-handler` WIT export.
- `WasiHttpExecutorImpl` — standard `wasi:http/incoming-handler`.

`ExecutorFactory::get_executor` decides which one based on the `App` config / wasi_http flag.

### Internal status codes

5xx responses carry an `X-CDN-Internal-Status` header (3000–3999). The mapping (timeouts vs OOM vs trap kinds) is duplicated as `pub(crate) const`s in `crates/http-service/src/lib.rs` and as the table in README.md — keep both in sync when changing.

### Plugging in a cache backend

`cache::CacheBackend` is an `async_trait` (`Send + Sync`). `CacheImpl::new(Arc<dyn CacheBackend>)` wraps it for the linker. The CLI ships `MemoryCacheBackend` (`src/cache.rs`) as the default; production embedders supply their own. `NoCacheBackend` returns `AccessDenied` for every op — use it as the deny-by-default fallback.

## Crate features worth knowing

- `runtime`: `kafka_log`, `victoria_log` (default on); `metrics` opts into Prometheus + lazy_static and propagates through `http-service/metrics`.
- `key-value-store`: `redis` enables the Redis-backed implementation. `runtime` already turns this on; if you depend on `key-value-store` from elsewhere, opt in explicitly.
- `cache`: no features yet — backend choice is at runtime via the `Arc<dyn CacheBackend>` injected into `Data<T>`.

## Releasing

GitFlow with `cargo-release` + `git-cliff`. The workflow lives in `release.toml` and `.github/workflows/release.yaml`; bump levels and the full procedure are in README.md. Don't hand-edit `CHANGELOG.md` — `git-cliff` regenerates it via the `pre-release-hook`.
