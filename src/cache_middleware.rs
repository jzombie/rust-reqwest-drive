use async_trait::async_trait;
// Binary serialization
use bitcode::{Decode, Encode};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http::{Extensions, HeaderMap, HeaderValue, StatusCode};
use reqwest::{Request, Response};
use reqwest_middleware::{Middleware, Next, Result};
use simd_r_drive::traits::{DataStoreReader, DataStoreWriter};
use simd_r_drive::{DataStore, compute_hash};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH}; // For parsing `Expires` headers

/// Per-request control for bypassing cache behavior.
///
/// When set to `CacheBypass(true)` in request extensions, the cache middleware
/// will skip both cache reads and cache writes for that request.
///
/// This is useful when you want a one-off fresh fetch while still reusing the
/// same client, cache store, and throttle middleware stack.
///
/// # Example
///
/// ```rust
/// use reqwest_drive::{CacheBypass, CachePolicy, ThrottlePolicy, init_cache_with_throttle};
/// use reqwest_middleware::ClientBuilder;
/// use std::path::Path;
///
/// # #[tokio::main]
/// # async fn main() {
/// let (cache, throttle) = init_cache_with_throttle(
///     Path::new("cache_storage.bin"),
///     CachePolicy::default(),
///     ThrottlePolicy::default(),
/// );
///
/// let client = ClientBuilder::new(reqwest::Client::new())
///     .with_arc(cache)
///     .with_arc(throttle)
///     .build();
///
/// let mut request = client.get("https://example.com");
/// request.extensions().insert(CacheBypass(true));
/// let _ = request.send().await;
/// # }
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct CacheBypass(pub bool);

/// Per-request control for busting and refreshing cache behavior.
///
/// When set to `CacheBust(true)` in request extensions, the cache middleware
/// skips cache reads for that request, forces a fresh network fetch, and then
/// writes the new response back to cache (subject to `CachePolicy`).
///
/// This is useful when you want to refresh a stale entry and make future
/// non-busted requests use the updated cached response.
///
/// # Example
///
/// ```rust
/// use reqwest_drive::{CacheBust, CachePolicy, ThrottlePolicy, init_cache_with_throttle};
/// use reqwest_middleware::ClientBuilder;
/// use std::path::Path;
///
/// # #[tokio::main]
/// # async fn main() {
/// let (cache, throttle) = init_cache_with_throttle(
///     Path::new("cache_storage.bin"),
///     CachePolicy::default(),
///     ThrottlePolicy::default(),
/// );
///
/// let client = ClientBuilder::new(reqwest::Client::new())
///     .with_arc(cache)
///     .with_arc(throttle)
///     .build();
///
/// let mut request = client.get("https://example.com");
/// request.extensions().insert(CacheBust(true));
/// let _ = request.send().await;
/// # }
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct CacheBust(pub bool);

/// Defines the caching policy for storing and retrieving responses.
#[derive(Clone, Debug)]
pub struct CachePolicy {
    /// Defines the caching policy for storing and retrieving responses.
    pub default_ttl: Duration,
    /// Determines whether cache expiration should respect HTTP headers.
    pub respect_headers: bool,
    /// Optional override for caching specific HTTP status codes.
    /// - If `None`, only success responses (`2xx`) are cached.
    /// - If `Some(Vec<u16>)`, only the specified status codes are cached.
    pub cache_status_override: Option<Vec<u16>>,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            default_ttl: Duration::from_secs(60 * 60 * 24), // Default 1 day TTL
            respect_headers: true,                          // Use headers if available
            cache_status_override: None, // Default behavior: Cache only 2xx responses
        }
    }
}

/// Represents a cached HTTP response.
#[derive(Encode, Decode)]
struct CachedResponse {
    /// HTTP status code of the cached response.
    status: u16,
    /// HTTP headers stored as key-value pairs, where values are raw bytes.
    headers: Vec<(String, Vec<u8>)>,
    /// Response body stored as raw bytes.
    body: Vec<u8>,
    /// Unix timestamp (in milliseconds) indicating when the cache entry expires.
    expiration_timestamp: u64,
}

/// Provides an HTTP cache layer backed by a `SIMD R Drive` data store.
#[derive(Clone)]
pub struct DriveCache {
    store: Arc<DataStore>,
    policy: CachePolicy, // Configurable policy
}

impl DriveCache {
    /// Creates a new cache backed by a file-based data store.
    ///
    /// # Arguments
    ///
    /// * `cache_storage_file` - Path to the file where cached responses are stored.
    /// * `policy` - Configuration specifying cache expiration behavior.
    ///
    /// # Panics
    ///
    /// This function will panic if the `DataStore` fails to initialize.
    pub fn new(cache_storage_file: &Path, policy: CachePolicy) -> Self {
        Self {
            store: Arc::new(DataStore::open(cache_storage_file).unwrap()),
            policy,
        }
    }

    /// Creates a new cache using an existing `Arc<DataStore>`.
    ///
    /// This allows sharing the cache store across multiple components.
    ///
    /// # Arguments
    ///
    /// * `store` - A shared `Arc<DataStore>` instance.
    /// * `policy` - Cache expiration configuration.
    pub fn with_drive_arc(store: Arc<DataStore>, policy: CachePolicy) -> Self {
        Self { store, policy }
    }

    /// Checks whether a request is cached and still valid.
    ///
    /// This method retrieves the cache entry associated with the request
    /// and determines if it is still within its valid TTL.
    ///
    /// # Arguments
    ///
    /// * `req` - The HTTP request to check for a cached response.
    ///
    /// # Returns
    ///
    /// Returns `true` if the request has a valid cached response; otherwise, `false`.
    pub async fn is_cached(&self, req: &Request) -> bool {
        let store = self.store.as_ref();

        let cache_key = self.generate_cache_key(req);
        let cache_key_bytes = cache_key.as_bytes();

        // let store = self.store.read().await;
        if let Ok(Some(entry_handle)) = store.read(cache_key_bytes) {
            eprintln!("Entry handle: {:?}", entry_handle);

            if let Ok(cached) = bitcode::decode::<CachedResponse>(entry_handle.as_slice()) {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_millis() as u64;

                // Extract TTL based on the policy (either from headers or default)
                let ttl = if self.policy.respect_headers {
                    // Convert headers back to HeaderMap to extract TTL
                    let mut headers = HeaderMap::new();
                    for (k, v) in cached.headers.iter() {
                        if let Ok(header_name) = k.parse::<http::HeaderName>()
                            && let Ok(header_value) = HeaderValue::from_bytes(v)
                        {
                            headers.insert(header_name, header_value);
                        }
                    }
                    Self::extract_ttl(&headers, &self.policy)
                } else {
                    self.policy.default_ttl
                };

                let expected_expiration = cached.expiration_timestamp + ttl.as_millis() as u64;

                // If expired, remove from cache
                if now >= expected_expiration {
                    // eprintln!("Determined cache is expired. now - expected_expiration: {:?}", now - expected_expiration);
                    eprintln!(
                        "Cache expires at: {}",
                        chrono::DateTime::from_timestamp_millis(expected_expiration as i64)
                            .unwrap()
                    );
                    eprintln!(
                        "Expiration timestamp: {}",
                        chrono::DateTime::from_timestamp_millis(cached.expiration_timestamp as i64)
                            .unwrap()
                    );
                    eprintln!(
                        "Now: {}",
                        chrono::DateTime::from_timestamp_millis(now as i64).unwrap()
                    );

                    store.delete(cache_key_bytes).ok();
                    return false;
                }

                return true;
            }
        }
        false
    }

    /// Generates a cache key based on request method, canonicalized URL, and relevant headers.
    ///
    /// The generated key is used to uniquely identify cached responses.
    ///
    /// Key strategy:
    /// - Includes request method.
    /// - Canonicalizes URL query parameters by sorting them by key/value.
    /// - Includes selected representation-affecting headers.
    /// - Hashes sensitive header values (e.g. Authorization) before adding them to key material.
    ///
    /// # Arguments
    ///
    /// * `req` - The HTTP request for which to generate a cache key.
    ///
    /// # Returns
    ///
    /// A string representing the cache key.
    fn generate_cache_key(&self, req: &Request) -> String {
        let method = req.method();
        let url = Self::canonicalize_url(req.url());
        let headers = req.headers();

        let relevant_headers = [
            "accept",
            "accept-language",
            "content-type",
            "authorization",
            "x-api-key",
        ];

        let header_string = relevant_headers
            .iter()
            .filter_map(|name| {
                headers.get(*name).map(|value| {
                    let value_str = if Self::is_sensitive_header(name) {
                        format!("h:{:016x}", compute_hash(value.as_bytes()))
                    } else {
                        value.to_str().unwrap_or_default().to_string()
                    };

                    format!("{}={}", name, value_str)
                })
            })
            .collect::<Vec<_>>()
            .join("&");

        format!("{} {} {}", method, url, header_string)
    }

    fn canonicalize_url(url: &reqwest::Url) -> String {
        let mut normalized = url.clone();

        let mut query_pairs = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect::<Vec<_>>();

        if !query_pairs.is_empty() {
            query_pairs.sort_by(|(k1, v1), (k2, v2)| k1.cmp(k2).then_with(|| v1.cmp(v2)));

            {
                let mut serializer = normalized.query_pairs_mut();
                serializer.clear();
                for (key, value) in query_pairs.iter() {
                    serializer.append_pair(key, value);
                }
            }
        }

        normalized.to_string()
    }

    fn is_sensitive_header(name: &str) -> bool {
        matches!(
            name,
            "authorization" | "proxy-authorization" | "cookie" | "x-api-key"
        )
    }

    /// Extracts the TTL from HTTP headers or falls back to the default TTL.
    ///
    /// # Arguments
    ///
    /// * `headers` - The HTTP headers to inspect.
    /// * `policy` - The cache policy specifying TTL behavior.
    ///
    /// # Returns
    ///
    /// A `Duration` indicating the cache expiration time.
    fn extract_ttl(headers: &HeaderMap, policy: &CachePolicy) -> Duration {
        if !policy.respect_headers {
            return policy.default_ttl;
        }

        if let Some(cache_control) = headers.get("cache-control")
            && let Ok(cache_control) = cache_control.to_str()
        {
            for directive in cache_control.split(',') {
                if let Some(max_age) = directive.trim().strip_prefix("max-age=")
                    && let Ok(seconds) = max_age.parse::<u64>()
                {
                    return Duration::from_secs(seconds);
                }
            }
        }

        if let Some(expires) = headers.get("expires")
            && let Ok(expires) = expires.to_str()
            && let Ok(expiry_time) = DateTime::parse_from_rfc2822(expires)
            && let Some(duration) = expiry_time.timestamp().checked_sub(Utc::now().timestamp())
            && duration > 0
        {
            return Duration::from_secs(duration as u64);
        }

        policy.default_ttl
    }
}

#[async_trait]
impl Middleware for DriveCache {
    /// Intercepts HTTP requests to apply caching behavior.
    ///
    /// This method first checks if a valid cached response exists for the incoming request.
    /// - If a cached response is found and still valid, it is returned immediately.
    /// - If no cache entry exists, the request is forwarded to the next middleware or backend.
    /// - If a response is received, it is cached according to the defined `CachePolicy`.
    ///
    /// This middleware **only caches GET and HEAD requests**. Other HTTP methods are passed through without caching.
    ///
    /// # Arguments
    ///
    /// * `req` - The incoming HTTP request.
    /// * `extensions` - A mutable reference to request extensions, which may store metadata.
    /// * `next` - The next middleware in the processing chain.
    ///
    /// # Returns
    ///
    /// A `Result<Response, reqwest_middleware::Error>` that contains either:
    /// - A cached response (if available).
    /// - A fresh response from the backend, which is then cached (if applicable).
    ///
    /// # Behavior
    ///
    /// - If the request is **already cached and valid**, returns the cached response.
    /// - If **no cache is found**, the request is sent to the backend, and the response is cached.
    /// - If **the cache has expired**, the old entry is deleted, and a fresh request is made.
    async fn handle(
        &self,
        req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> Result<Response> {
        let bypass_cache = extensions
            .get::<CacheBypass>()
            .map(|flag| flag.0)
            .unwrap_or(false);
        let bust_cache = extensions
            .get::<CacheBust>()
            .map(|flag| flag.0)
            .unwrap_or(false);

        let cache_key = self.generate_cache_key(&req);

        eprintln!("Handle cache key: {}", cache_key);

        let store = self.store.as_ref();
        let cache_key_bytes = cache_key.as_bytes();

        if req.method() == "GET" || req.method() == "HEAD" {
            if !bypass_cache
                && !bust_cache
                && self.is_cached(&req).await
                && let Ok(Some(entry_handle)) = store.read(cache_key_bytes)
                && let Ok(cached) = bitcode::decode::<CachedResponse>(entry_handle.as_slice())
            {
                let mut headers = HeaderMap::new();
                for (k, v) in cached.headers {
                    if let Ok(header_name) = k.parse::<http::HeaderName>()
                        && let Ok(header_value) = HeaderValue::from_bytes(&v)
                    {
                        headers.insert(header_name, header_value);
                    }
                }
                let status = StatusCode::from_u16(cached.status).unwrap_or(StatusCode::OK);
                return Ok(build_response(status, headers, Bytes::from(cached.body)));
            }

            let response = next.run(req, extensions).await?;
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.bytes().await?.to_vec();

            let ttl = Self::extract_ttl(&headers, &self.policy);
            let expiration_timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_millis() as u64
                + ttl.as_millis() as u64;

            let body_clone = body.clone();

            let should_cache = match &self.policy.cache_status_override {
                Some(status_codes) => status_codes.contains(&status.as_u16()),
                None => status.is_success(),
            };

            if should_cache && !bypass_cache {
                let serialized = bitcode::encode(&CachedResponse {
                    status: status.as_u16(),
                    headers: headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
                        .collect(),
                    body,
                    expiration_timestamp,
                });

                eprintln!("Writing cache with key: {}", cache_key);
                store.write(cache_key_bytes, serialized.as_slice()).ok();
            }

            return Ok(build_response(status, headers, Bytes::from(body_clone)));
        }

        next.run(req, extensions).await
    }
}

/// Constructs a `reqwest::Response` from a given status code, headers, and body.
///
/// This function is used to rebuild an HTTP response from cached data,
/// ensuring that it correctly retains headers and status information.
///
/// # Arguments
///
/// * `status` - The HTTP status code of the response.
/// * `headers` - A `HeaderMap` containing response headers.
/// * `body` - A `Bytes` object containing the response body.
///
/// # Returns
///
/// A `reqwest::Response` representing the reconstructed HTTP response.
///
/// # Panics
///
/// This function will panic if the response body fails to be constructed.
fn build_response(status: StatusCode, headers: HeaderMap, body: Bytes) -> Response {
    let mut response_builder = http::Response::builder().status(status);

    for (key, value) in headers.iter() {
        response_builder = response_builder.header(key, value);
    }

    let http_response = response_builder
        .body(body)
        .expect("Failed to create HTTP response");

    Response::from(http_response)
}
