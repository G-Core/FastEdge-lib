# Cache Crate

This crate provides host function implementations for the FastEdge cache interface, supporting both synchronous and asynchronous cache operations.

## Overview

The cache crate implements the WIT-defined cache interfaces:
- `gcore:fastedge/cache` - Async cache interface (placeholder) 
- `gcore:fastedge/cache-sync` - Sync cache interface (fully implemented)
- `gcore:fastedge/cache-types` - Shared types (Payload, Error)

## Features

### Cache Operations

- **get** - Retrieve a value by key
- **set** - Store a value with optional TTL (time-to-live)
- **delete** - Remove a key-value pair
- **exists** - Check if a key exists
- **incr** - Atomically increment/decrement an integer value
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

// Use the cache
cache_impl.set("key", b"value".to_vec(), Some(5000)).await?;
let value = cache_impl.get("key").await?;
```

### Adding to Linker

The cache implementation integrates with wasmtime's component model:

```rust
use reactor::gcore::fastedge::cache_sync;

// Add to linker (actual integration example)
cache_sync::add_to_linker::<T, D>(linker, host_getter)?;
```

## Architecture

### Simple and Direct

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
│  - Redis, memory, moka, etc.            │
│  - Thread-safe async operations         │
└─────────────────────────────────────────┘
```

### Components

- **`CacheBackend`** - Trait defining backend interface
- **`CacheImpl`** - Host implementation wrapping a backend
- **`NoCacheBackend`** - No-op backend that returns `AccessDenied`

## Testing

The crate includes comprehensive unit tests with mock implementations:

```bash
cargo test -p cache
```

Tests cover:
- Get/set/delete operations
- Existence checks
- Increment operations (including negative deltas)
- TTL and expiry management
- Error conditions
- Sync trait implementation

## Thread Safety

All implementations must be `Send + Sync`:
- The `CacheBackend` trait requires `Send + Sync`
- Operations use async functions for non-blocking I/O
- Safe for concurrent access across tasks

## Example Backends

You can implement `CacheBackend` with various storage engines:

- **In-memory** - HashMap with Mutex/RwLock
- **Moka** - High-performance concurrent cache
- **Redis** - Distributed cache
- **Custom** - Any storage that fits your needs

## Future Work

- Implement `cache::HostWithStore` for the async variant with Accessor pattern
- Add ready-to-use backend implementations (Redis, Moka, etc.)
- Add metrics and observability hooks
- Support for batch operations
- Cache warming and preloading strategies


