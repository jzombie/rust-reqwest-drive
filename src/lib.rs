#[cfg(doctest)]
doc_comment::doctest!("../README.md");

use std::path::Path;

pub use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};

mod cache_middleware;
pub use cache_middleware::{CachePolicy, DriveCache};

mod throttle_middleware;
pub use throttle_middleware::{DriveThrottleBackoff, ThrottlePolicy};

use simd_r_drive::DataStore;
use std::sync::Arc;

// TODO: Add usage examples here

/// Initializes only the cache middleware with a file-based data store.
///
/// This function creates a new `DriveCache` instance backed by a `DataStore` file.
///
/// # Arguments
///
/// * `cache_storage_file` - Path to the file where cached responses are stored.
/// * `policy` - The cache expiration policy.
///
/// # Returns
///
/// An `Arc<DriveCache>` instance managing the cache.
pub fn init_cache(cache_storage_file: &Path, policy: CachePolicy) -> Arc<DriveCache> {
    Arc::new(DriveCache::new(cache_storage_file, policy))
}

/// Initializes both cache and throttle middleware with a file-based data store.
///
/// This function creates:
/// - A `DriveCache` instance for response caching.
/// - A `DriveThrottleBackoff` instance for rate-limiting and retrying failed requests.
///
/// # Arguments
///
/// * `cache_storage_file` - Path to the file where cached responses are stored.
/// * `cache_policy` - The cache expiration policy.
/// * `throttle_policy` - The throttling and backoff policy.
///
/// # Returns
///
/// A tuple containing:
/// - `Arc<DriveCache>` for caching.
/// - `Arc<DriveThrottleBackoff>` for throttling.
pub fn init_cache_with_throttle(
    cache_storage_file: &Path,
    cache_policy: CachePolicy,
    throttle_policy: ThrottlePolicy,
) -> (Arc<DriveCache>, Arc<DriveThrottleBackoff>) {
    let cache = Arc::new(DriveCache::new(cache_storage_file, cache_policy));
    let throttle = Arc::new(DriveThrottleBackoff::new(
        throttle_policy,
        Arc::clone(&cache),
    ));
    (cache, throttle)
}

/// Initializes only the cache middleware using an **existing** `Arc<DataStore>`.
///
/// This function is useful if a shared `DataStore` instance already exists
/// and should be reused instead of creating a new one.
///
/// # Arguments
///
/// * `store` - A shared `Arc<DataStore>` instance.
/// * `policy` - The cache expiration policy.
///
/// # Returns
///
/// An `Arc<DriveCache>` instance managing the cache.
pub fn init_cache_with_drive(store: Arc<DataStore>, policy: CachePolicy) -> Arc<DriveCache> {
    Arc::new(DriveCache::with_drive_arc(store, policy))
}

/// Initializes both cache and throttle middleware using an **existing** `Arc<DataStore>`.
///
/// This function is useful if a shared `DataStore` instance already exists
/// and should be reused instead of creating a new one.
///
/// # Arguments
///
/// * `store` - A shared `Arc<DataStore>` instance.
/// * `cache_policy` - The cache expiration policy.
/// * `throttle_policy` - The throttling and backoff policy.
///
/// # Returns
///
/// A tuple containing:
/// - `Arc<DriveCache>` for caching.
/// - `Arc<DriveThrottleBackoff>` for throttling.
pub fn init_cache_with_drive_and_throttle(
    store: Arc<DataStore>,
    cache_policy: CachePolicy,
    throttle_policy: ThrottlePolicy,
) -> (Arc<DriveCache>, Arc<DriveThrottleBackoff>) {
    let cache = Arc::new(DriveCache::with_drive_arc(store, cache_policy));
    let throttle = Arc::new(DriveThrottleBackoff::new(
        throttle_policy,
        Arc::clone(&cache),
    ));
    (cache, throttle)
}

/// Initializes a `ClientWithMiddleware` using both cache and throttle middleware.
///
/// This function sets up:
/// - A `DriveCache` instance for response caching.
/// - A `DriveThrottleBackoff` instance for request throttling and backoff.
/// - A `reqwest` HTTP client wrapped with middleware for caching and throttling.
///
/// # Arguments
///
/// * `cache_storage_file` - Path to the file where cached responses are stored.
/// * `cache_policy` - The cache expiration policy.
/// * `throttle_policy` - The throttling and backoff policy.
///
/// # Returns
///
/// A `ClientWithMiddleware` instance, which is a `reqwest` HTTP client
/// enhanced with caching and throttling.
///
/// # Example
///
/// ```
/// use std::path::Path;
/// use reqwest_drive::{init_client_with_cache_and_throttle, CachePolicy, ThrottlePolicy};
///
/// let client = init_client_with_cache_and_throttle(
///     Path::new("http_storage_cache.bin"),
///     CachePolicy::default(),
///     ThrottlePolicy::default(),
/// );
/// ```
///
/// This client can now be used to make HTTP requests with automatic caching and rate limiting.
pub fn init_client_with_cache_and_throttle(
    cache_storage_file: &Path,
    cache_policy: CachePolicy,
    throttle_policy: ThrottlePolicy,
) -> ClientWithMiddleware {
    let (cache, throttle) =
        init_cache_with_throttle(cache_storage_file, cache_policy, throttle_policy);

    ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache)
        .with_arc(throttle)
        .build()
}
