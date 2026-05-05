# Cache Crate

This crate provides host function implementations for the FastEdge cache interface.

## Overview

The cache crate implements the WIT-defined cache interfaces:
- `gcore:fastedge/cache-sync` - Synchronous-style cache interface
- `gcore:fastedge/cache-types` - Shared types (`payload`, `error`)

## Features

### Cache Operations

- **get** - Retrieve a value by key
- **set** - Store a value with optional TTL (time-to-live in milliseconds)
- **delete** - Remove a key-value pair
- **exists** - Check if a key exists
- **incr** - Atomically increment/decrement an integer value (`i64` delta)
- **expire** - Update the TTL for a key

### Error Types

- `AccessDenied` - Component lacks permission to access the cache
- `InternalError` - Unexpected internal error
- `Other(String)` - Implementation-specific error

## Usage

### Implementing a Cache Backend

```rust
use cache::{CacheBackend, Error, Payload};
use async_trait::async_trait;

struct MyCache {
    // Your cache implementation
}

#[async_trait]
impl CacheBackend for MyCache {
    async fn get(&self, key: &str) -> Result<Option<Payload>, Error> {
        // Implementation
    }

    async fn set(&self, key: &str, value: Payload, ttl_ms: Option<u64>) -> Result<(), Error> {
        // Implementation
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        // Implementation
    }

    async fn exists(&self, key: &str) -> Result<bool, Error> {
        // Implementation
    }

    async fn incr(&self, key: &str, delta: i64) -> Result<i64, Error> {
        // Implementation
    }

    async fn expire(&self, key: &str, ttl_ms: u64) -> Result<bool, Error> {
        // Implementation
    }
}
```

### Creating and Using CacheImpl

```rust
use cache::CacheImpl;
use std::sync::Arc;

// Create a cache implementation with your backend
let backend = Arc::new(MyCache::new());
let cache_impl = CacheImpl::new(backend);
```

### Adding to the Wasmtime Linker

`CacheImpl` implements `cache_sync::Host`, so it can be registered with a
wasmtime component linker via the generated `add_to_linker` helper:

```rust
use reactor::gcore::fastedge::cache_sync;
use wasmtime::component::HasSelf;

cache_sync::add_to_linker::<T, HasSelf<CacheImpl>>(linker, |state: &mut T| {
    &mut state.cache_impl
})?;
```

## Architecture

```
┌─────────────────────────────────────────┐
│         WASM Component (Guest)          │
│  Uses: gcore:fastedge/cache-sync        │
└──────────────────┬──────────────────────┘
                   │ WIT Interface
┌──────────────────▼──────────────────────┐
│          CacheImpl (Host)               │
│  - Implements cache_sync::Host          │
│  - Direct delegation to backend         │
└──────────────────┬──────────────────────┘
                   │ Direct call
┌──────────────────▼──────────────────────┐
│        CacheBackend Implementation      │
│  - Memory, Redis, moka, etc.            │
│  - Thread-safe async operations         │
└─────────────────────────────────────────┘
```

### Components

- **`CacheBackend`** - Async trait defining the backend interface (`Send + Sync`)
- **`CacheImpl`** - Host implementation wrapping an `Arc<dyn CacheBackend>`
- **`NoCacheBackend`** - No-op backend that returns `AccessDenied` for every
  operation; useful as a default when caching is disabled

## Testing

The crate includes unit tests with a mock in-memory backend:

```bash
cargo test -p cache
```

Tests cover:
- Get/set/delete operations
- Existence checks
- Increment operations (including negative deltas and missing keys)
- TTL and expiry management
- `AccessDenied` propagation from `NoCacheBackend`
- Linker registration compatibility (compile-only)

## Thread Safety

- The `CacheBackend` trait requires `Send + Sync`
- All operations are `async` for non-blocking I/O
- Backends are shared via `Arc<dyn CacheBackend>` and may be invoked concurrently

## Example Backends

`CacheBackend` can be implemented on top of any storage engine:

- **In-memory** - `HashMap` behind a `tokio::Mutex`/`RwLock`
- **Moka** - High-performance concurrent cache
- **Redis** - Distributed cache
- **Custom** - Any storage that fits your needs

The FastEdge binary ships with an in-memory `MemoryCacheBackend` that supports
TTL-based expiration; see `src/cache.rs` at the workspace root for a reference
implementation.

## Future Work

- Ship ready-to-use backend implementations from this crate (Redis, Moka, …)
- Add metrics and observability hooks
- Support for batch operations
- Cache warming and preloading strategies