# reqwest-drive

[![made-with-rust][rust-logo]][rust-src-page] [![crates.io][crates-badge]][crates-page] [![MIT licensed][mit-license-badge]][mit-license-page] [![Apache 2.0 licensed][apache-2.0-license-badge]][apache-2.0-license-page] [![Coverage][coveralls-badge]][coveralls-page]

High-performance caching, throttling, and backoff middleware for [reqwest](https://crates.io/crates/reqwest), powered by **SIMD-accelerated** single-file storage.

## Overview

`reqwest-drive` is a middleware based on [`reqwest-middleware`](https://crates.io/crates/reqwest-middleware) that provides:
- **High-speed request caching** using [SIMD R Drive](https://crates.io/crates/simd-r-drive), a SIMD-optimized, single-file-container data store.
- **Automatic process-scoped cache storage** via [cache-manager](https://crates.io/crates/cache-manager), with no manual cache path required.
- **Adaptive request throttling** with support for dynamic concurrency limits.
- **Configurable backoff strategies** for handling rate-limiting and transient failures.
- **Throttle-only mode** that requires no persistent store.

Note: This is not **WASM** compatible.

### Cache safety note

- The cache layer is **thread-safe** within a single process.
- The cache layer is **not multi-process safe** when multiple processes target the same cache file concurrently.
- If your deployment has multiple processes/workers, use process-level coordination or separate cache files.

## Features

- **Efficient single-file caching**  
  - Uses **SIMD acceleration** for fast reads/writes.
  - Supports **header-based TTLs** or custom expiration policies.
    - Normalizes query parameter order for stable cache identity.
    - Varies cache entries by key request headers (e.g. `accept`, `accept-language`, `content-type`).
    - Hashes sensitive header values (like `authorization`/`x-api-key`) before key material is constructed.
    - Supports per-request cache controls:
        - **Bypass** cache (`CacheBypass`) for one-off uncached reads.
        - **Bust & refresh** cache (`CacheBust`) to force a fresh value and update stored cache.
- **Customizable throttling & backoff**  
  - Control request concurrency.
  - Define exponential backoff & jitter for retries.
    - Run in **throttle-only** mode without cache persistence.
    - Supports per-request throttle policy overrides via request extensions.
- **Seamless integration with `reqwest`**  
  - Works as a `reqwest-middleware` layer.
  - Easy to configure and extend.

## Install

```sh
cargo add reqwest-drive
```

## Usage

### Basic example with caching:

```rust
use reqwest_drive::{init_cache, CachePolicy};
use reqwest_middleware::ClientBuilder;
use std::time::Duration;
use tempfile::tempdir;

#[tokio::main]
async fn main() {
    let temp_dir = tempdir().unwrap();
    let cache_path = temp_dir.path().join("cache_storage.bin");

    let cache = init_cache(&cache_path, CachePolicy {
        default_ttl: Duration::from_secs(3600), // Cache for 1 hour
        respect_headers: true,
        cache_status_override: None,
    });

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache)
        .build();

    let response = client.get("https://httpbin.org/get").send().await.unwrap();
    println!("Response: {:?}", response.text().await.unwrap());
}
```

### Process-scoped cache (no manual cache path)

Use this mode to automatically place cache storage under discovered `<crate-root>/.cache`,
using cache group `reqwest-drive`, and a process/thread-scoped `cache_storage.bin` location:

```rust
use reqwest_drive::{init_cache_process_scoped, CachePolicy};
use reqwest_middleware::ClientBuilder;
use std::time::Duration;

#[tokio::main]
async fn main() {
    // Optional: keep cache discovery scoped to a temp root for demo/test isolation.
    // In a real app, you can remove this and let discovery use your current project root.
    let temp_root = tempfile::tempdir().unwrap();
    let previous_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(temp_root.path()).unwrap();

    let cache = init_cache_process_scoped(CachePolicy {
        default_ttl: Duration::from_secs(3600),
        respect_headers: true,
        cache_status_override: None,
    })
    .expect("init process-scoped cache");

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache)
        .build();

    let response = client.get("https://httpbin.org/get").send().await.unwrap();
    println!("Response status: {}", response.status());

    std::env::set_current_dir(previous_cwd).unwrap();
}
```

Notes:
- The cache group name is this crate's package name: `reqwest-drive`.
- Process directories are PID-scoped and cleaned up on normal process shutdown.
- Cleanup is best-effort (crashes/forced exits can leave stale directories).

### Throttling & Backoff

To enable request throttling and exponential backoff:
```rust
use reqwest_drive::{init_cache_with_throttle, CachePolicy, ThrottlePolicy};
use reqwest_middleware::ClientBuilder;
use std::time::Duration;
use tempfile::tempdir;

#[tokio::main]
async fn main() {
    let temp_dir = tempdir().unwrap();
    let cache_path = temp_dir.path().join("cache_storage.bin");

    let (cache, throttle) = init_cache_with_throttle(
        &cache_path,
        CachePolicy::default(),
        ThrottlePolicy {
            base_delay_ms: 200,
            adaptive_jitter_ms: 100,
            max_concurrent: 2,
            max_retries: 3,
        }
    );

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache)
        .with_arc(throttle)
        .build();

    let response = client.get("https://httpbin.org/status/429").send().await.unwrap();
    println!("Response status: {}", response.status());
}
```

### Process-scoped cache + throttling

```rust
use reqwest_drive::{
    CachePolicy, ThrottlePolicy, init_cache_process_scoped_with_throttle,
    init_client_with_cache_and_throttle,
};

#[tokio::main]
async fn main() {
    // Optional: keep cache discovery scoped to a temp root for demo/test isolation.
    let temp_root = tempfile::tempdir().unwrap();
    let previous_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(temp_root.path()).unwrap();

    let (cache, throttle) = init_cache_process_scoped_with_throttle(
        CachePolicy::default(),
        ThrottlePolicy {
            base_delay_ms: 200,
            adaptive_jitter_ms: 100,
            max_concurrent: 2,
            max_retries: 3,
        },
    )
    .expect("init process-scoped cache + throttle");

    let client = init_client_with_cache_and_throttle(cache, throttle);

    let response = client.get("https://httpbin.org/status/429").send().await.unwrap();
    println!("Response status: {}", response.status());

    std::env::set_current_dir(previous_cwd).unwrap();
}
```

### Throttle-only (No Data Store)

Use this mode when you want rate limiting and retry/backoff behavior, but no cache layer at all:

```rust
use reqwest_drive::{init_throttle, ThrottlePolicy};
use reqwest_middleware::ClientBuilder;

#[tokio::main]
async fn main() {
    let throttle = init_throttle(ThrottlePolicy {
        base_delay_ms: 200,
        adaptive_jitter_ms: 100,
        max_concurrent: 2,
        max_retries: 3,
    });

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(throttle)
        .build();

    let response = client.get("https://httpbin.org/status/429").send().await.unwrap();
    println!("Response status: {}", response.status());
}
```

### Initializing Client without `with_arc`

Initializing a client with both caching and throttling, without manually attaching middleware via `.with_arc()`:

```rust
use reqwest_drive::{
    init_cache_with_throttle, init_client_with_cache_and_throttle, CachePolicy, ThrottlePolicy,
};
use reqwest_middleware::ClientWithMiddleware;
use std::time::Duration;
use tempfile::tempdir;

#[tokio::main]
async fn main() {
    let cache_policy = CachePolicy {
        default_ttl: Duration::from_secs(300), // Cache responses for 5 minutes
        respect_headers: true,
        cache_status_override: None,
    };

    let throttle_policy = ThrottlePolicy {
        base_delay_ms: 100,     // 100ms delay before retrying
        adaptive_jitter_ms: 50, // Add jitter to avoid request bursts
        max_concurrent: 2,      // Allow 2 concurrent requests
        max_retries: 2,         // Retry up to 2 times on failure
    };

    let temp_dir = tempdir().unwrap();
    let cache_path = temp_dir.path().join("cache_storage.bin");

    // Initialize cache and throttling middleware
    let (cache, throttle) = init_cache_with_throttle(&cache_path, cache_policy, throttle_policy);

    // Initialize a client with cache and throttling middleware
    let client: ClientWithMiddleware = init_client_with_cache_and_throttle(cache, throttle);

    // Perform a request using the client
    let response = client.get("https://httpbin.org/get").send().await.unwrap();
    println!("Response status: {}", response.status());
}
```

### Overriding Throttle Policy (Per Request)

To override the throttle policy for a single request:

```rust
use reqwest_drive::{init_cache_with_throttle, CachePolicy, ThrottlePolicy};
use reqwest_middleware::ClientBuilder;
use std::time::Duration;
use tempfile::tempdir;

#[tokio::main]
async fn main() {
    let temp_dir = tempdir().unwrap();
    let cache_path = temp_dir.path().join("cache_storage.bin");

    let (cache, throttle) = init_cache_with_throttle(
        &cache_path,
        CachePolicy::default(),
        ThrottlePolicy {
            base_delay_ms: 200,      // Default base delay
            adaptive_jitter_ms: 100, // Default jitter
            max_concurrent: 2,       // Default concurrency
            max_retries: 3,          // Default retries
        }
    );

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache)
        .with_arc(throttle)
        .build();

    // Define a custom throttle policy for this request
    let custom_throttle_policy = ThrottlePolicy {
        base_delay_ms: 50,      // Lower base delay for this request
        adaptive_jitter_ms: 25, // Less jitter
        max_concurrent: 1,      // Allow only 1 concurrent request
        max_retries: 1,         // Only retry once
    };

    // Apply the custom throttle policy **only** for this request
    let mut request = client.get("https://httpbin.org/status/429");
    request.extensions().insert(custom_throttle_policy);
    
    let response = request.send().await.unwrap();

    println!("Response status: {}", response.status());
}
```

### Bypassing Cache for a Single Request

When using cache + throttle together, you can bypass cache for an individual request while keeping the same middleware stack:

```rust
use reqwest_drive::{CacheBypass, CachePolicy, ThrottlePolicy, init_cache_with_throttle};
use reqwest_middleware::ClientBuilder;
use tempfile::tempdir;

#[tokio::main]
async fn main() {
    let temp_dir = tempdir().unwrap();
    let cache_path = temp_dir.path().join("cache_storage.bin");

    let (cache, throttle) = init_cache_with_throttle(
        &cache_path,
        CachePolicy::default(),
        ThrottlePolicy::default(),
    );

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache)
        .with_arc(throttle)
        .build();

    let mut request = client.get("https://httpbin.org/get");

    // Bypass this particular request
    request.extensions().insert(CacheBypass(true));

    // This request skips cache read/write, but still uses throttle/backoff
    let response = request.send().await.unwrap();
    println!("Response status: {}", response.status());
}
```

### Busting Cache for a Single Request (Refresh)

Use this when you want to force a fresh fetch now and update the cached entry for later requests:

```rust
use reqwest_drive::{CacheBust, CachePolicy, ThrottlePolicy, init_cache_with_throttle};
use reqwest_middleware::ClientBuilder;
use tempfile::tempdir;

#[tokio::main]
async fn main() {
    let temp_dir = tempdir().unwrap();
    let cache_path = temp_dir.path().join("cache_storage.bin");

    let (cache, throttle) = init_cache_with_throttle(
        &cache_path,
        CachePolicy::default(),
        ThrottlePolicy::default(),
    );

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache)
        .with_arc(throttle)
        .build();

    let mut request = client.get("https://httpbin.org/get");

    // Bust cache for this request: skip cache read, then refresh cache with response
    request.extensions().insert(CacheBust(true));

    let response = request.send().await.unwrap();
    println!("Response status: {}", response.status());
}
```

### Configuration

The middleware can be fine-tuned using the following options:

#### Cache Policy

```rust
use reqwest_middleware::ClientBuilder;
use reqwest_drive::{init_cache, CachePolicy};
use std::time::Duration;
use tempfile::tempdir;

#[tokio::main]
async fn main() {
    let temp_dir = tempdir().unwrap();
    let cache_path = temp_dir.path().join("cache_storage.bin");

    let cache_policy = CachePolicy {
        default_ttl: Duration::from_secs(60 * 60), // Default: 1 hour
        respect_headers: true, // Use HTTP headers for caching decisions
        cache_status_override: Some(vec![200, 404]), // Define which status codes are cacheable
    };

    let cache = init_cache(&cache_path, cache_policy);

    // Configure `reqwest` client with `SIMD R Drive`-powered cache
    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache)
        .build();

    let response = client.get("https://httpbin.org/get").send().await.unwrap();
    println!("Response status: {}", response.status());
}
```

#### Throttle Policy

```rust
use reqwest_middleware::ClientBuilder;
use reqwest_drive::{init_throttle, ThrottlePolicy};

#[tokio::main]
async fn main() {
    let throttle_policy = ThrottlePolicy {
        base_delay_ms: 100,      // Base delay before retries
        adaptive_jitter_ms: 50,  // Add jitter to prevent synchronization issues
        max_concurrent: 1,       // Allow only 1 concurrent request
        max_retries: 2,          // Number of retries before failing
    };

    // Creates throttle middleware only (no cache/data store)
    let throttle = init_throttle(throttle_policy);

    // Configure `reqwest` client with throttle/backoff support
    let client = ClientBuilder::new(reqwest::Client::new())
        // Integrate `throttle` middleware
        .with_arc(throttle)
        .build();

    let response = client.get("https://httpbin.org/get").send().await.unwrap();
    println!("Response status: {}", response.status());
}
```

### Using re-exported `reqwest`

`reqwest-drive` re-exports `reqwest` and generally tracks a compatible upstream
`reqwest` release to reduce version ambiguity, while still allowing independent
updates when needed. Import it as `reqwest_drive::reqwest`.

> Note: This will completely bypass all throttling and caching if the middleware is not attached.

```rust
use reqwest_drive::reqwest;

let client = reqwest::Client::new();
let request = client.get("https://httpbin.org/get").build().unwrap();
assert_eq!(request.url().as_str(), "https://httpbin.org/get");
```

## Why `reqwest-drive`?

✅ **Faster than traditional disk-based caches** (memory-mapped, single-file storage container with SIMD acceleration for queries).  
✅ **More efficient than in-memory caches** (persists data across runs without RAM overhead).  
✅ **Backoff-aware throttling** helps prevent API bans due to excessive requests.

## License

`reqwest-drive` is primarily distributed under the terms of both the MIT license and the Apache License (Version 2.0).

See [LICENSE-APACHE](./LICENSE-APACHE) and [LICENSE-MIT](./LICENSE-MIT) for details.

[rust-src-page]: https://www.rust-lang.org/
[rust-logo]: https://img.shields.io/badge/Made%20with-Rust-black

[crates-page]: https://crates.io/crates/reqwest-drive
[crates-badge]: https://img.shields.io/crates/v/reqwest-drive.svg

[mit-license-page]: ./LICENSE-MIT
[mit-license-badge]: https://img.shields.io/badge/license-MIT-blue.svg

[apache-2.0-license-page]: ./LICENSE-APACHE
[apache-2.0-license-badge]: https://img.shields.io/badge/license-Apache%202.0-blue.svg

[coveralls-page]: https://coveralls.io/github/jzombie/rust-reqwest-drive?branch=main
[coveralls-badge]: https://img.shields.io/coveralls/github/jzombie/rust-reqwest-drive
