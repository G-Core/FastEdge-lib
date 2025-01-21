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
* run with `cargo run --bin fastedge-run -- --help` flag to list run commands and options
