FastEdge run tool and common runtime libraries

# Setting up

## Install Rust with WASM compilation target

Run following commands:

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

## Pull submodules

Run `git submodule update --init --recursive -f`

## Building

Run `cargo build --release` to build fastedge app run tool and all required dependencies.

## Releasing

Fastedge run tool and FastEdge lib are released with GitHub using [cargo-release](https://github.com/crate-ci/cargo-release) tool. 
The release process includes next steps:
* increment crate version in Cargo.toml
* generate CHANGELOG.md file
* push version tag 
* create GitHub release and build artefacts

### Prerequisites

Install cargo release:
``` 
cargo install cargo-release
```
Install [git-cliff](https://git-cliff.org) (tool to generate changelog from Git history):
```
cargo install git-cliff
```
### Creating new release

We are using GitFlow strategy. That means that everything in `main` branch should be ready to be released. 
To create a new release it is necessary to checkout a new release branch with next naming convention: `releases/vX.Y.Z`.
Where the `vX.Y.Z` is the next version number.

```
    cargo release <LEVEL> --execute
```
This command will commit and push to remote changed Cargo.toml and CHANGELOG.md files. And also add a tag for current release. 
Once the release branch is pushed on remote it triggers the release process as GitHub Action.

Note: It also creates a PR for `releases/**` branch to merge it to `main` as soon as release is ready.

#### Release bump level

* `release`: Remove the pre-release extension; if any (0.1.0-alpha.1 -> 0.1.0, 0.1.0 -> 0.1.0).
* `patch`:
    * If version has a pre-release, then the pre-release extension is removed (0.1.0-alpha.1 -> 0.1.0).
    * Otherwise, bump the patch field (0.1.0 -> 0.1.1)
* `minor`: Bump minor version (0.1.0-pre -> 0.2.0)
* `major`: Bump major version (0.1.0-pre -> 1.0.0)
* `alpha`, `beta`, and `rc`: Add/increment pre-release to your version
  (1.0.0 -> 1.0.1-rc.1, 1.0.1-alpha -> 1.0.1-rc.1, 1.0.1-rc.1 ->
  1.0.1-rc.2)


# Running

## Fastedge Run Tool

`fastedge-run` is a local test tool for running FastEdge Wasm applications over HTTP.

```
fastedge-run <COMMAND>

Commands:
  http    Execute http handler
  help    Print this message or the help of the given subcommand(s)
```

Run `cargo run --bin fastedge-run -- --help` to list commands, or
`cargo run --bin fastedge-run -- http --help` for full flag reference.

### `http` subcommand

Starts a local HTTP server that runs a Wasm application on every request.

```
fastedge-run http [OPTIONS] --port <PORT> --wasm <WASM>
```

#### Required flags

| Flag | Description |
|------|-------------|
| `-p, --port <PORT>` | TCP port the server listens on (`127.0.0.1` only) |
| `-w, --wasm <WASM>` | Path to the compiled `.wasm` file |

#### Optional flags

| Flag | Description |
|------|-------------|
| `-e, --env <KEY=VALUE>` | Environment variable passed to the Wasm app (repeatable) |
| `-s, --secret <KEY=VALUE>` | Secret variable passed to the Wasm app (repeatable) |
| `--headers <KEY=VALUE>` | Request headers injected before execution (repeatable) |
| `--rsp-headers <KEY=VALUE>` | Extra headers added to every response (repeatable) |
| `--propagate-header <NAME>` | Forward this header from the incoming request as-is (repeatable) |
| `--kv-stores <NAME=URL>` | Key-value store: map a store name to a Redis URL (repeatable) |
| `--geo` | Inject sample Gcore PoP geo headers into each request |
| `-m <BYTES>` | Memory limit for the Wasm instance (default: 128 MB) |
| `--max-duration <MS>` | Max execution time in milliseconds (default: 60 000 ms) |
| `--wasi-http <BOOL>` | Enable the WASI HTTP interface |
| `--dotenv [PATH]` | Load variables from dotenv files (see [Dotenv support](#dotenv-support)) |

#### Basic example

```bash
fastedge-run http --port 8080 --wasm ./my_app.wasm
```

#### Passing environment variables and secrets

```bash
fastedge-run http \
  --port 8080 \
  --wasm ./my_app.wasm \
  --env HOST=localhost \
  --env PORT=5432 \
  --secret API_KEY=supersecret
```

#### Injecting request headers

Use `--headers` to add fixed headers to every request before the Wasm handler sees it, and
`--propagate-header` to pass through a header from the real incoming request unchanged:

```bash
fastedge-run http \
  --port 8080 \
  --wasm ./my_app.wasm \
  --headers X-Client-ID=test-client \
  --propagate-header Authorization
```

#### Key-value stores

Map a store name (as used by the Wasm app) to a Redis URL with `--kv-stores <NAME=URL>`.
Multiple stores can be specified by repeating the flag.

```bash
fastedge-run http \
  --port 8080 \
  --wasm ./my_app.wasm \
  --kv-stores sessions=redis://localhost:6379 \
  --kv-stores cache=redis://cache-host:6379
```

The store name on the left (`sessions`, `cache`) is the identifier the Wasm app uses to open
the store. The value on the right is the Redis connection URL passed to the Redis client.

#### Geo headers

`--geo` injects a set of sample Gcore PoP location headers so the app can be tested locally
without a real CDN:

```
pop-lat, pop-long, pop-reg, pop-city, pop-continent, pop-country-code, pop-country-name
```

```bash
fastedge-run http --port 8080 --wasm ./my_app.wasm --geo
```

#### Resource limits

```bash
fastedge-run http \
  --port 8080 \
  --wasm ./my_app.wasm \
  -m 67108864 \        # 64 MB memory limit
  --max-duration 5000  # 5-second execution timeout
```

### Dotenv support

Pass `--dotenv` (optionally with a directory path; defaults to the current directory) to load
variables from dotenv files. CLI flags take precedence over file values.

| File | Populated into |
|------|----------------|
| `.env` | All categories via `FASTEDGE_VAR_<TYPE>_` prefixed keys, and bare keys into env vars |
| `.env.variables` | Environment variables (`--env`) |
| `.env.secrets` | Secrets (`--secret`) |
| `.env.req_headers` | Request headers (`--headers`) |
| `.env.rsp_headers` | Response headers (`--rsp-headers`) |
| `.env.kv_stores` | Key-value store mappings (`--kv-stores`) |

Prefix keys in `.env` with `FASTEDGE_VAR_<TYPE>_` to target a specific category:

| Prefix | Category |
|--------|----------|
| `FASTEDGE_VAR_ENV_` | Environment variables |
| `FASTEDGE_VAR_SECRET_` | Secrets |
| `FASTEDGE_VAR_REQ_HEADER_` | Request headers |
| `FASTEDGE_VAR_RSP_HEADER_` | Response headers |
| `FASTEDGE_VAR_KV_STORE` | Key-value stores |

Bare keys in `.env` (without a `FASTEDGE_VAR_` prefix) are added as environment variables.

**Example `.env`:**
```
DB_HOST=localhost
FASTEDGE_VAR_SECRET_API_KEY=s3cr3t
FASTEDGE_VAR_RSP_HEADER_Cache-Control=no-store
FASTEDGE_VAR_KV_STORE=sessions=redis://localhost:6379
```

```bash
fastedge-run http --port 8080 --wasm ./my_app.wasm --dotenv
# or point to a specific directory:
fastedge-run http --port 8080 --wasm ./my_app.wasm --dotenv ./config/
```
