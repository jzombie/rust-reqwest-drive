# reqwest-drive

[![Crates.io](https://img.shields.io/crates/v/reqwest-drive.svg)](https://crates.io/crates/reqwest-drive)
[![Docs.rs](https://docs.rs/reqwest-drive/badge.svg)](https://docs.rs/reqwest-drive)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**THIS API IS A WORK IN PROGRESS.**

High-performance caching, throttling, and backoff middleware for [reqwest](https://crates.io/crates/reqwest), powered by **SIMD-accelerated** single-file storage.

## Overview

`reqwest-drive` is a middleware based on [`reqwest-middleware`](https://crates.io/crates/reqwest-middleware) that provides:
- **High-speed request caching** using [SIMD R Drive](https://crates.io/crates/simd-r-drive), a SIMD-optimized, single-file-container data store.
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
use std::path::Path;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let cache = init_cache(Path::new("cache_storage.bin"), CachePolicy {
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

### Throttling & Backoff

To enable request throttling and exponential backoff:
```rust
use reqwest_drive::{init_cache_with_throttle, CachePolicy, ThrottlePolicy};
use reqwest_middleware::ClientBuilder;
use std::path::Path;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let (cache, throttle) = init_cache_with_throttle(
        Path::new("cache_storage.bin"),
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
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

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

    // Initialize cache and throttling middleware
    let (cache, throttle) = init_cache_with_throttle(Path::new("cache_storage.bin"), cache_policy, throttle_policy);

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
use std::path::Path;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let (cache, throttle) = init_cache_with_throttle(
        Path::new("cache_storage.bin"),
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
use std::path::Path;

#[tokio::main]
async fn main() {
    let (cache, throttle) = init_cache_with_throttle(
        Path::new("cache_storage.bin"),
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
use std::path::Path;

#[tokio::main]
async fn main() {
    let (cache, throttle) = init_cache_with_throttle(
        Path::new("cache_storage.bin"),
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
use std::path::Path;
use reqwest_middleware::ClientBuilder;
use reqwest_drive::{init_cache, CachePolicy};
use std::time::Duration;

let cache_policy = CachePolicy {
    default_ttl: Duration::from_secs(60 * 60), // Default: 1 hour
    respect_headers: true, // Use HTTP headers for caching decisions
    cache_status_override: Some(vec![200, 404]), // Define which status codes are cacheable
};

let cache = init_cache(&Path::new("cache_storage.bin"), cache_policy);

// Configure `reqwest` client with `SIMD R Drive`-powered cache
let client = ClientBuilder::new(reqwest::Client::new())
    .with_arc(cache)
    .build();
```

#### Throttle Policy

```rust
use reqwest_middleware::ClientBuilder;
use reqwest_drive::{init_throttle, ThrottlePolicy};

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
```

## Why `reqwest-drive`?

✅ **Faster than traditional disk-based caches** (memory-mapped, single-file storage container with SIMD acceleration for queries).  
✅ **More efficient than in-memory caches** (persists data across runs without RAM overhead).  
✅ **Backoff-aware throttling** helps prevent API bans due to excessive requests.

## License
`reqwest-drive` is licensed under Apache License, Version 2.0 [LICENSE](LICENSE).
