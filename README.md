FastEdge CLI and common runtime libraries

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

Run `cargo build --release` to build CLI tool and all required dependencies.

# Running

## CLI
* run with `cargo run --bin cli -- --help` flag to list CLI commands and options
