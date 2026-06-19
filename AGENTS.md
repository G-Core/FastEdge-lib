# AGENTS.md

## Fast start (before touching code)
- WIT files are vendored directly at `crates/reactor/wit/` (git subtree), so no submodule init is required after clone.
- Use CI-equivalent checks: `cargo build --all-features`, `cargo test --all`, `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features`.
- Assume warnings fail CI (`RUSTFLAGS="-Dwarnings"` behavior).

## WIT subtree maintenance
- Upstream WIT remote alias is `fastedge-wit` (`https://github.com/G-Core/FastEdge-wit.git`).
- If alias is missing in a fresh clone: `git remote add fastedge-wit https://github.com/G-Core/FastEdge-wit.git`.
- Pull upstream updates into this repo: `git subtree pull --prefix=crates/reactor/wit fastedge-wit main --squash`.
- Push local WIT changes upstream: `git subtree push --prefix=crates/reactor/wit fastedge-wit main`.
- Keep WIT contract and generated bindings aligned: edit `crates/reactor/wit/*.wit` and verify `crates/reactor/src/lib.rs` still compiles.

## Big picture architecture (top -> bottom)
- `src/main.rs`: `fastedge-run` CLI; builds `runtime::App` from flags/dotenv, then runs `HttpService`.
- `crates/http-service/src/lib.rs`: Hyper server and request pipeline; app lookup, status gating, executor invocation, error/status mapping.
- `crates/runtime/src/lib.rs`: Wasmtime engine + shared host state (`Data<T>`), component/module loading, componentization fallback.
- `crates/reactor/` + `crates/reactor/wit/`: WIT world and generated host bindings for FastEdge interfaces.
- Host capability crates (`cache`, `key-value-store`, `secret`, `utils`, `http-backend`) are linked into `Data<T>` and exposed via linker registration.

## Core data flow to keep in mind
- HTTP request enters `HttpService::handle_request_inner` -> app ID/name resolution (`fastedge_app_id`, `server_name`, then URL path) in `crates/http-service/src/lib.rs`.
- Context asks `ExecutorFactory::get_executor` for either `HttpExecutorImpl` or `WasiHttpExecutorImpl` (`crates/http-service/src/executor/mod.rs`).
- Executor runs Wasm with store data from `runtime::Data<T>`; outbound host HTTP uses `WasiHttpView::send_request` in `crates/runtime/src/lib.rs`.
- 5xx responses include `X-CDN-Internal-Status`; constants in `crates/http-service/src/lib.rs` must stay aligned with `README.md` table.

## Project-specific patterns (important)
- Two WASI worlds exist simultaneously (`WasiVersion::{Preview1, Preview2}`); wrong context accessor intentionally panics (`preview1_wasi_ctx_mut` / `preview2_wasi_ctx_mut`).
- Core wasm modules are auto-componentized by `componentize_if_necessary` using bundled Preview1 adapter (`crates/runtime/src/adapters/wasi_snapshot_preview1.reactor.wasm`).
- New host interface pattern is explicit: add WIT -> add field to `runtime::Data<T>` -> register in linker (`HttpService::add_fastedge_imports`) -> expose constructor option via `ContextT`/`App` path.
- Cache integration is runtime-injected (`Arc<dyn CacheBackend>`); CLI default is `MemoryCacheBackend` in `src/cache.rs`, deny-by-default backend is `NoCacheBackend`.

## External integration and dependency constraints
- Wasmtime crates are pinned to custom fork (`G-Core/wasmtime` release-36.0.0); local override lives commented in `.cargo/config.toml`.
- Redis-backed KV is feature-gated in `key-value-store`; runtime already enables it.
- `fastedge-run http` exposes integration knobs used by tests/dev: `--wasi-http`, `--kv-stores`, `--dotenv`, `--propagate-header`, `--geo` (see `README.md` usage examples).

## Useful local workflows
- Focused tests: `cargo test -p http-service <test_name>` and `cargo test -p cache`.
- Run local app quickly: `cargo run --bin fastedge-run -- http --port 8080 --wasm ./my_app.wasm`.
- If changing error mapping, verify both behavior and docs: `crates/http-service/src/lib.rs` + root `README.md` Internal status codes section.

## Where to read first for common tasks
- Add/modify request routing or app resolution: `crates/http-service/src/lib.rs` (`app_name_from_request`, `handle_request_inner`).
- Add host state/capability: `crates/runtime/src/lib.rs` (`Data<T>`, `WasmEngineBuilder`) and `crates/http-service/src/lib.rs` linker wiring.
- Modify CLI/runtime wiring: `src/main.rs`, `src/context.rs`, `crates/runtime/src/app.rs`.
- Touch WIT contracts: `crates/reactor/wit/*.wit` and `crates/reactor/src/lib.rs`.

