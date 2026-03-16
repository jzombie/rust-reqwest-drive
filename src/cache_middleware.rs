use async_trait::async_trait;
// Binary serialization
use bitcode::{Decode, Encode};
use bytes::Bytes;
use cache_manager::{CacheRoot, ProcessScopedCacheGroup};
use chrono::{DateTime, Utc};
use http::{Extensions, HeaderMap, HeaderValue, StatusCode};
use reqwest::{Request, Response};
use reqwest_middleware::{Middleware, Next, Result};
use simd_r_drive::traits::{DataStoreReader, DataStoreWriter};
use simd_r_drive::{DataStore, compute_hash};
use std::io;
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
/// use tempfile::tempdir;
///
/// # #[tokio::main]
/// # async fn main() {
/// let temp_dir = tempdir().unwrap();
/// let cache_path = temp_dir.path().join("cache_storage.bin");
///
/// let (cache, throttle) = init_cache_with_throttle(
///     &cache_path,
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
/// use tempfile::tempdir;
///
/// # #[tokio::main]
/// # async fn main() {
/// let temp_dir = tempdir().unwrap();
/// let cache_path = temp_dir.path().join("cache_storage.bin");
///
/// let (cache, throttle) = init_cache_with_throttle(
///     &cache_path,
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
///
/// ## Concurrency model
///
/// - Thread-safe for concurrent access within a single process.
/// - Not multi-process safe for concurrent access to the same backing file.
///
/// If multiple processes need caching, use process-level coordination
/// (e.g., external locking/ownership) or separate cache files per process.
#[derive(Clone)]
pub struct DriveCache {
    store: Arc<DataStore>,
    policy: CachePolicy, // Configurable policy
    _process_scoped_group: Option<Arc<ProcessScopedCacheGroup>>,
}

impl DriveCache {
    /// Creates a new cache backed by a file-based data store.
    ///
    /// # Arguments
    ///
    /// * `cache_storage_file` - Path to the file where cached responses are stored.
    /// * `policy` - Configuration specifying cache expiration behavior.
    ///
    /// # Concurrency
    ///
    /// The cache is thread-safe within a process, but the backing file should
    /// not be shared for concurrent reads/writes across multiple processes.
    ///
    /// # Panics
    ///
    /// This function will panic if the `DataStore` fails to initialize.
    pub fn new(cache_storage_file: &Path, policy: CachePolicy) -> Self {
        Self {
            store: Arc::new(DataStore::open(cache_storage_file).unwrap()),
            policy,
            _process_scoped_group: None,
        }
    }

    /// Creates a new cache using discovered `.cache` root and a process-scoped storage bin.
    ///
    /// The cache group is derived from this crate name (`reqwest-drive`), and the entry
    /// file is created under a process/thread scoped subdirectory so callers do not need
    /// to manually provide a cache path.
    ///
    /// # Errors
    ///
    /// Returns an error if discovery or process-scoped directory/file initialization fails.
    pub fn new_process_scoped(policy: CachePolicy) -> io::Result<Self> {
        let cache_root = CacheRoot::from_discovery()?;
        let scoped_group = Arc::new(ProcessScopedCacheGroup::new(
            &cache_root,
            env!("CARGO_PKG_NAME"),
        )?);
        let cache_storage_file = scoped_group.touch_thread_entry("cache_storage.bin")?;
        let store = DataStore::open(&cache_storage_file).map_err(|err| {
            io::Error::other(format!(
                "failed to open DataStore at {}: {err}",
                cache_storage_file.display()
            ))
        })?;

        Ok(Self {
            store: Arc::new(store),
            policy,
            _process_scoped_group: Some(scoped_group),
        })
    }

    /// Creates a new cache using an existing `Arc<DataStore>`.
    ///
    /// This allows sharing the cache store across multiple components.
    ///
    /// # Arguments
    ///
    /// * `store` - A shared `Arc<DataStore>` instance.
    /// * `policy` - Cache expiration configuration.
    ///
    /// # Concurrency
    ///
    /// This is thread-safe within a process. Avoid concurrent multi-process
    /// access to the same underlying store/file.
    pub fn with_drive_arc(store: Arc<DataStore>, policy: CachePolicy) -> Self {
        Self {
            store,
            policy,
            _process_scoped_group: None,
        }
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
            tracing::debug!("Entry handle: {:?}", entry_handle);

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
                    // tracing::debug!("Determined cache is expired. now - expected_expiration: {:?}", now - expected_expiration);
                    tracing::debug!(
                        "Cache expires at: {}",
                        chrono::DateTime::from_timestamp_millis(expected_expiration as i64)
                            .unwrap()
                    );
                    tracing::debug!(
                        "Expiration timestamp: {}",
                        chrono::DateTime::from_timestamp_millis(cached.expiration_timestamp as i64)
                            .unwrap()
                    );
                    tracing::debug!(
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

        tracing::debug!("Handle cache key: {}", cache_key);

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

                tracing::debug!("Writing cache with key: {}", cache_key);
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

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::{RngExt, SeedableRng};
    use reqwest::Method;
    use std::collections::{HashMap, HashSet};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::TempDir;

    fn build_request(method: Method, url: &str, headers: &[(&str, Option<&str>)]) -> Request {
        // Construct `reqwest::Request` directly rather than building a
        // `reqwest::Client` per-iteration. Creating a `Client` repeatedly in
        // tight loops was the dominant cost on some CI runners; building the
        // `Request` directly avoids that overhead while remaining functionally
        // equivalent for these key-generation tests.
        let mut request = Request::new(
            method,
            reqwest::Url::parse(url).expect("failed to parse request URL"),
        );

        for (name, value) in headers {
            if let Some(value) = value {
                let header_name = http::header::HeaderName::from_bytes(name.as_bytes())
                    .expect("invalid header name");
                let header_value =
                    http::header::HeaderValue::from_str(value).expect("invalid header value");
                request.headers_mut().insert(header_name, header_value);
            }
        }

        request
    }

    fn build_cache_for_tests() -> DriveCache {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let cache_path = temp_dir.path().join("cache_key_matrix.bin");
        DriveCache::new(&cache_path, CachePolicy::default())
    }

    fn random_token(rng: &mut StdRng, min_len: usize, max_len: usize) -> String {
        let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let token_len = rng.random_range(min_len..=max_len);

        (0..token_len)
            .map(|_| {
                let index = rng.random_range(0..alphabet.len());
                alphabet[index] as char
            })
            .collect()
    }

    fn build_random_request(rng: &mut StdRng) -> Request {
        let methods = [
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ];

        let method = methods[rng.random_range(0..methods.len())].clone();
        let mut url = format!(
            "https://example.test/{}/{}",
            random_token(rng, 3, 10),
            random_token(rng, 3, 10)
        );

        let query_pair_count = rng.random_range(0..=6);
        if query_pair_count > 0 {
            url.push('?');
            for query_index in 0..query_pair_count {
                if query_index > 0 {
                    url.push('&');
                }

                let query_key = random_token(rng, 1, 8);
                let query_value = random_token(rng, 0, 12);
                url.push_str(&query_key);
                url.push('=');
                url.push_str(&query_value);
            }
        }

        let mut request = Request::new(
            method,
            reqwest::Url::parse(&url).expect("failed to parse randomized URL"),
        );

        if rng.random::<bool>() {
            let accept_values = ["application/json", "text/plain", "*/*"];
            request.headers_mut().insert(
                http::header::ACCEPT,
                http::header::HeaderValue::from_str(accept_values[rng.random_range(0..3)])
                    .expect("invalid accept header value"),
            );
        }

        if rng.random::<bool>() {
            let language_values = ["en-US", "fr-FR", "es-ES", "de-DE"];
            request.headers_mut().insert(
                http::header::ACCEPT_LANGUAGE,
                http::header::HeaderValue::from_str(language_values[rng.random_range(0..4)])
                    .expect("invalid accept-language header value"),
            );
        }

        if rng.random::<bool>() {
            let content_type_values = ["application/json", "application/xml", "text/plain"];
            request.headers_mut().insert(
                http::header::CONTENT_TYPE,
                http::header::HeaderValue::from_str(content_type_values[rng.random_range(0..3)])
                    .expect("invalid content-type header value"),
            );
        }

        if rng.random::<bool>() {
            let authorization_value = format!("Bearer {}", random_token(rng, 16, 48));
            request.headers_mut().insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(&authorization_value)
                    .expect("invalid authorization header value"),
            );
        }

        if rng.random::<bool>() {
            let api_key_value = random_token(rng, 12, 32);
            request.headers_mut().insert(
                http::header::HeaderName::from_static("x-api-key"),
                http::header::HeaderValue::from_str(&api_key_value)
                    .expect("invalid x-api-key header value"),
            );
        }

        request
    }

    #[test]
    fn fuzz_cache_key_hash_collisions_uses_library_key_generator() {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let cache_path = temp_dir.path().join("cache_key_fuzz.bin");
        let cache = DriveCache::new(&cache_path, CachePolicy::default());

        let mut observed_hash_to_key: HashMap<u64, String> = HashMap::new();
        let mut random_generator = StdRng::seed_from_u64(0xD15EA5E5);

        let sample_count = 50_000;

        let mut distinct_key_count = 0usize;

        // Seed one known key first so the equality-assert branch is exercised
        // deterministically without relying on random hash collisions.
        let duplicate_request = build_request(
            Method::GET,
            "https://example.test/duplicate?a=1&b=2",
            &[("accept", Some("application/json"))],
        );
        let duplicate_key = cache.generate_cache_key(&duplicate_request);
        let duplicate_hash = compute_hash(duplicate_key.as_bytes());
        observed_hash_to_key.insert(duplicate_hash, duplicate_key.clone());
        if let Some(existing_key) = observed_hash_to_key.get(&duplicate_hash) {
            assert_eq!(existing_key, &duplicate_key);
        }

        for sample_index in 0..sample_count {
            let request = if sample_index == 0 {
                // Reuse the exact same request as the seeded entry so this
                // loop deterministically hits the "hash already seen" branch.
                // We are NOT expecting collisions between distinct keys.
                // A distinct-key collision still fails the test via `assert_eq!`.
                build_request(
                    Method::GET,
                    "https://example.test/duplicate?a=1&b=2",
                    &[("accept", Some("application/json"))],
                )
            } else {
                build_random_request(&mut random_generator)
            };

            let cache_key = cache.generate_cache_key(&request);
            let hash = compute_hash(cache_key.as_bytes());

            if let Some(existing_key) = observed_hash_to_key.get(&hash) {
                assert_eq!(
                    existing_key, &cache_key,
                    "hash collision detected for distinct cache keys"
                );
            } else {
                observed_hash_to_key.insert(hash, cache_key);
                distinct_key_count += 1;
            }
        }

        assert!(
            distinct_key_count > sample_count / 2,
            "random generation produced too few distinct keys"
        );
    }

    #[tokio::test]
    async fn is_cached_uses_default_ttl_when_respect_headers_is_disabled() {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let cache_path = temp_dir.path().join("cache_default_ttl.bin");
        let cache = DriveCache::new(
            &cache_path,
            CachePolicy {
                default_ttl: Duration::from_secs(60),
                respect_headers: false,
                cache_status_override: None,
            },
        );

        let request = build_request(
            Method::GET,
            "https://example.test/default-ttl",
            &[("accept", Some("application/json"))],
        );
        let cache_key = cache.generate_cache_key(&request);
        let cache_key_bytes = cache_key.as_bytes();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_millis() as u64;

        let cached = CachedResponse {
            status: 200,
            headers: vec![("cache-control".to_string(), b"max-age=0".to_vec())],
            body: b"ok".to_vec(),
            expiration_timestamp: now,
        };

        let serialized = bitcode::encode(&cached);
        cache
            .store
            .as_ref()
            .write(cache_key_bytes, serialized.as_slice())
            .expect("write cached entry");

        assert!(cache.is_cached(&request).await);
    }

    #[tokio::test]
    async fn is_cached_evicts_entry_when_expired() {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let cache_path = temp_dir.path().join("cache_expired_evict.bin");
        let cache = DriveCache::new(
            &cache_path,
            CachePolicy {
                default_ttl: Duration::from_millis(0),
                respect_headers: false,
                cache_status_override: None,
            },
        );

        let request = build_request(
            Method::GET,
            "https://example.test/expired-entry",
            &[("accept", Some("application/json"))],
        );
        let cache_key = cache.generate_cache_key(&request);
        let cache_key_bytes = cache_key.as_bytes();

        let cached = CachedResponse {
            status: 200,
            headers: Vec::new(),
            body: b"stale".to_vec(),
            expiration_timestamp: 0,
        };

        let serialized = bitcode::encode(&cached);
        cache
            .store
            .as_ref()
            .write(cache_key_bytes, serialized.as_slice())
            .expect("write cached entry");

        assert!(!cache.is_cached(&request).await);
        let stored = cache
            .store
            .as_ref()
            .read(cache_key_bytes)
            .expect("read cache key after eviction");
        assert!(stored.is_none(), "expired key should be evicted");
    }

    #[test]
    fn extract_ttl_returns_default_when_header_respect_is_disabled() {
        let policy = CachePolicy {
            default_ttl: Duration::from_secs(321),
            respect_headers: false,
            cache_status_override: None,
        };

        let mut headers = HeaderMap::new();
        headers.insert("cache-control", HeaderValue::from_static("max-age=1"));

        assert_eq!(
            DriveCache::extract_ttl(&headers, &policy),
            policy.default_ttl
        );
    }

    #[test]
    fn extract_ttl_uses_cache_control_max_age_when_present() {
        let policy = CachePolicy {
            default_ttl: Duration::from_secs(321),
            respect_headers: true,
            cache_status_override: None,
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            "cache-control",
            HeaderValue::from_static("public, max-age=42"),
        );

        assert_eq!(
            DriveCache::extract_ttl(&headers, &policy),
            Duration::from_secs(42)
        );
    }

    #[test]
    fn extract_ttl_uses_expires_header_when_cache_control_missing() {
        let policy = CachePolicy {
            default_ttl: Duration::from_secs(600),
            respect_headers: true,
            cache_status_override: None,
        };

        let mut headers = HeaderMap::new();
        let future = (Utc::now() + chrono::Duration::seconds(120)).to_rfc2822();
        headers.insert(
            "expires",
            HeaderValue::from_str(&future).expect("expires header should be valid"),
        );

        let ttl = DriveCache::extract_ttl(&headers, &policy);
        assert!(ttl > Duration::from_secs(0));
        assert!(ttl < policy.default_ttl);
    }

    #[test]
    fn exhaustive_cache_key_matrix_no_hash_collisions_for_distinct_keys() {
        let cache = build_cache_for_tests();

        let (methods, paths, queries) = (
            vec![
                Method::GET,
                Method::HEAD,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
            ],
            vec!["/resource", "/resource/v2", "/resource/deep/path"],
            vec![
                "", "?a=1", "?a=2", "?a=1&b=2", "?b=2&a=1", "?a=1&a=2", "?a=1&a=3", "?z=9",
            ],
        );

        let accept_values = [None, Some("application/json"), Some("text/plain")];
        let language_values = [None, Some("en-US"), Some("fr-FR")];
        let content_type_values = [None, Some("application/json"), Some("application/xml")];
        let authorization_values = [None, Some("Bearer alpha-token"), Some("Bearer beta-token")];
        let api_key_values = [None, Some("alpha-api-key"), Some("beta-api-key")];

        let mut hash_to_key: HashMap<u64, String> = HashMap::new();
        let mut distinct_keys: HashSet<String> = HashSet::new();
        let mut sample_count = 0usize;

        for method in &methods {
            for path in &paths {
                for query in &queries {
                    for accept in accept_values {
                        for accept_language in language_values {
                            for content_type in content_type_values {
                                for authorization in authorization_values {
                                    for api_key in api_key_values {
                                        sample_count += 1;

                                        let url = format!("https://example.test{}{}", path, query);
                                        let request = build_request(
                                            method.clone(),
                                            &url,
                                            &[
                                                ("accept", accept),
                                                ("accept-language", accept_language),
                                                ("content-type", content_type),
                                                ("authorization", authorization),
                                                ("x-api-key", api_key),
                                            ],
                                        );

                                        let cache_key = cache.generate_cache_key(&request);
                                        let hash = compute_hash(cache_key.as_bytes());

                                        if let Some(existing_key) = hash_to_key.get(&hash) {
                                            assert_eq!(
                                                existing_key, &cache_key,
                                                "hash collision detected for distinct cache keys"
                                            );
                                        } else {
                                            hash_to_key.insert(hash, cache_key.clone());
                                        }

                                        distinct_keys.insert(cache_key);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let expected_sample_count = methods.len()
            * paths.len()
            * queries.len()
            * accept_values.len()
            * language_values.len()
            * content_type_values.len()
            * authorization_values.len()
            * api_key_values.len();
        assert_eq!(sample_count, expected_sample_count);
        assert!(
            distinct_keys.len() > sample_count / 2,
            "matrix generation produced too few distinct keys"
        );
    }

    #[test]
    fn cache_key_query_reordering_is_canonical_and_hash_stable() {
        let cache = build_cache_for_tests();

        let request_a = build_request(
            Method::GET,
            "https://example.test/resource?a=1&b=2",
            &[("accept", Some("application/json"))],
        );
        let request_b = build_request(
            Method::GET,
            "https://example.test/resource?b=2&a=1",
            &[("accept", Some("application/json"))],
        );

        let key_a = cache.generate_cache_key(&request_a);
        let key_b = cache.generate_cache_key(&request_b);

        assert_eq!(key_a, key_b);
        assert_eq!(
            compute_hash(key_a.as_bytes()),
            compute_hash(key_b.as_bytes())
        );
    }

    #[test]
    fn cache_key_changes_for_each_response_affecting_dimension() {
        let cache = build_cache_for_tests();

        let base_request = build_request(
            Method::GET,
            "https://example.test/resource?a=1&b=2",
            &[
                ("accept", Some("application/json")),
                ("accept-language", Some("en-US")),
                ("content-type", Some("application/json")),
                ("authorization", Some("Bearer alpha-token")),
                ("x-api-key", Some("alpha-api-key")),
            ],
        );
        let base_key = cache.generate_cache_key(&base_request);
        let base_hash = compute_hash(base_key.as_bytes());

        let variants = vec![
            build_request(
                Method::POST,
                "https://example.test/resource?a=1&b=2",
                &[
                    ("accept", Some("application/json")),
                    ("accept-language", Some("en-US")),
                    ("content-type", Some("application/json")),
                    ("authorization", Some("Bearer alpha-token")),
                    ("x-api-key", Some("alpha-api-key")),
                ],
            ),
            build_request(
                Method::GET,
                "https://example.test/resource/v2?a=1&b=2",
                &[
                    ("accept", Some("application/json")),
                    ("accept-language", Some("en-US")),
                    ("content-type", Some("application/json")),
                    ("authorization", Some("Bearer alpha-token")),
                    ("x-api-key", Some("alpha-api-key")),
                ],
            ),
            build_request(
                Method::GET,
                "https://example.test/resource?a=99&b=2",
                &[
                    ("accept", Some("application/json")),
                    ("accept-language", Some("en-US")),
                    ("content-type", Some("application/json")),
                    ("authorization", Some("Bearer alpha-token")),
                    ("x-api-key", Some("alpha-api-key")),
                ],
            ),
            build_request(
                Method::GET,
                "https://example.test/resource?a=1&b=2",
                &[
                    ("accept", Some("text/plain")),
                    ("accept-language", Some("en-US")),
                    ("content-type", Some("application/json")),
                    ("authorization", Some("Bearer alpha-token")),
                    ("x-api-key", Some("alpha-api-key")),
                ],
            ),
            build_request(
                Method::GET,
                "https://example.test/resource?a=1&b=2",
                &[
                    ("accept", Some("application/json")),
                    ("accept-language", Some("fr-FR")),
                    ("content-type", Some("application/json")),
                    ("authorization", Some("Bearer alpha-token")),
                    ("x-api-key", Some("alpha-api-key")),
                ],
            ),
            build_request(
                Method::GET,
                "https://example.test/resource?a=1&b=2",
                &[
                    ("accept", Some("application/json")),
                    ("accept-language", Some("en-US")),
                    ("content-type", Some("application/xml")),
                    ("authorization", Some("Bearer alpha-token")),
                    ("x-api-key", Some("alpha-api-key")),
                ],
            ),
            build_request(
                Method::GET,
                "https://example.test/resource?a=1&b=2",
                &[
                    ("accept", Some("application/json")),
                    ("accept-language", Some("en-US")),
                    ("content-type", Some("application/json")),
                    ("authorization", Some("Bearer beta-token")),
                    ("x-api-key", Some("alpha-api-key")),
                ],
            ),
            build_request(
                Method::GET,
                "https://example.test/resource?a=1&b=2",
                &[
                    ("accept", Some("application/json")),
                    ("accept-language", Some("en-US")),
                    ("content-type", Some("application/json")),
                    ("authorization", Some("Bearer alpha-token")),
                    ("x-api-key", Some("beta-api-key")),
                ],
            ),
        ];

        for variant in variants {
            let variant_key = cache.generate_cache_key(&variant);
            let variant_hash = compute_hash(variant_key.as_bytes());

            assert_ne!(
                variant_key, base_key,
                "variant unexpectedly produced same key"
            );
            assert_ne!(
                variant_hash, base_hash,
                "variant unexpectedly produced same hash"
            );
        }
    }

    /*
    Stress experiment (disabled):
    - This 100,000,000-sample collision test worked (no collisions observed).
    - End-to-end runtime was several hours, even in `--release` mode.
    - A Rayon parallelization attempt did not produce a meaningful speedup for
      this workload, so the test is commented out to keep normal test cycles fast.

        // Kept as commented code because it is only used by the disabled stress
        // test below. If we re-enable that test in the future, this helper can be
        // uncommented together with it.
        // fn build_unique_request_from_index(index: u64) -> Request {
        //     let methods = [
        //         Method::GET,
        //         Method::HEAD,
        //         Method::POST,
        //         Method::PUT,
        //         Method::PATCH,
        //         Method::DELETE,
        //     ];
        //
        //     let method = methods[(index % methods.len() as u64) as usize].clone();
        //     let path = format!("/resource/{}/{}/{}", index % 97, index % 503, index % 9973);
        //
        //     let query = format!(
        //         "a={}&b={}&c={}&d={}",
        //         index,
        //         index.wrapping_mul(31),
        //         index.rotate_left(7),
        //         index ^ 0xA5A5_A5A5_A5A5_A5A5
        //     );
        //
        //     let url = format!("https://example.test{}?{}", path, query);
        //
        //     let accept_values = ["application/json", "text/plain", "STAR_SLASH_STAR"];
        //     let language_values = ["en-US", "fr-FR", "es-ES", "de-DE"];
        //     let content_type_values = ["application/json", "application/xml", "text/plain"];
        //
        //     let mut request = Request::new(
        //         method,
        //         reqwest::Url::parse(&url).expect("failed to parse stress URL"),
        //     );
        //
        //     request.headers_mut().insert(
        //         http::header::ACCEPT,
        //         http::header::HeaderValue::from_str(
        //             accept_values[(index % accept_values.len() as u64) as usize],
        //         )
        //         .expect("invalid accept header value"),
        //     );
        //     request.headers_mut().insert(
        //         http::header::ACCEPT_LANGUAGE,
        //         http::header::HeaderValue::from_str(
        //             language_values[(index % language_values.len() as u64) as usize],
        //         )
        //         .expect("invalid accept-language header value"),
        //     );
        //     request.headers_mut().insert(
        //         http::header::CONTENT_TYPE,
        //         http::header::HeaderValue::from_str(
        //             content_type_values[(index % content_type_values.len() as u64) as usize],
        //         )
        //         .expect("invalid content-type header value"),
        //     );
        //
        //     let authorization_value = format!("Bearer token-{:016x}", index);
        //     request.headers_mut().insert(
        //         http::header::AUTHORIZATION,
        //         http::header::HeaderValue::from_str(&authorization_value)
        //             .expect("invalid authorization header value"),
        //     );
        //
        //     let api_key_value = format!("api-key-{:016x}", index.rotate_right(11));
        //     request.headers_mut().insert(
        //         http::header::HeaderName::from_static("x-api-key"),
        //         http::header::HeaderValue::from_str(&api_key_value)
        //             .expect("invalid x-api-key header value"),
        //     );
        //
        //     request
        // }

    #[test]
    #[ignore = "expensive: runs 100,000,000 samples"]
    fn cache_key_hash_collision_stress_100_million() {
        init_test_tracing();

        let cache = build_cache_for_tests();

        let samples = 100_000_000_u64;
        let mut seen_hashes: HashSet<u64> = HashSet::new();
        let started_at = Instant::now();

        for index in 0..samples {
            let request = build_unique_request_from_index(index);
            let cache_key = cache.generate_cache_key(&request);
            let hash = compute_hash(cache_key.as_bytes());

            assert!(
                seen_hashes.insert(hash),
                "hash collision detected in stress test at sample index {} (hash={})",
                index,
                hash
            );

            let completed = index + 1;
            if completed % 10_000 == 0 {
                let elapsed = started_at.elapsed();
                let pct = (completed as f64 / samples as f64) * 100.0;
                tracing::info!(
                    "stress progress: {}/{} ({:.4}%) elapsed={:?}",
                    completed,
                    samples,
                    pct,
                    elapsed
                );
            }
        }

        tracing::info!(
            "stress complete: {} samples in {:?}",
            samples,
            started_at.elapsed()
        );
    }
    */
}
