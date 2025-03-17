use crate::DriveCache;
use async_trait::async_trait;
use http::Extensions;
use rand::Rng;
use reqwest::{Request, Response};
use reqwest_middleware::{Error, Middleware, Next};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::time::{sleep, Duration};

/// Defines the throttling and backoff behavior for handling HTTP requests.
///
/// This policy determines the **rate limiting strategy** used for outgoing requests,
/// including fixed delays, adaptive backoff, and retry settings.
#[derive(Clone, Debug)]
pub struct ThrottlePolicy {
    /// The base delay (in milliseconds) applied before making a request.
    ///
    /// This ensures a **minimum delay** between consecutive requests.
    pub base_delay_ms: u64,

    /// The maximum random jitter (in milliseconds) added to the backoff delay.
    ///
    /// This prevents synchronization issues when multiple clients are making requests,
    /// reducing the likelihood of rate-limit collisions.
    pub adaptive_jitter_ms: u64,

    /// The maximum number of concurrent requests allowed at any given time.
    ///
    /// This controls **parallel request execution**, ensuring that no more than
    /// `max_concurrent` requests are in-flight simultaneously.
    pub max_concurrent: usize,

    /// The maximum number of retries allowed in case of failed requests.
    ///
    /// If a request fails (e.g., due to a **server error or rate limiting**),
    /// it will be retried up to `max_retries` times with exponential backoff.
    pub max_retries: usize,
}

impl Default for ThrottlePolicy {
    /// Provides a sensible default throttling policy.
    ///
    /// This default configuration is suitable for most API use cases and includes:
    /// - A **base delay** of 500ms between requests.
    /// - A **random jitter** of up to 250ms to avoid synchronization issues.
    /// - A **maximum of 5 concurrent requests** to prevent excessive load.
    /// - A **maximum of 3 retries** for failed requests.
    ///
    /// # Returns
    ///
    /// A `ThrottlePolicy` instance with preconfigured defaults.
    fn default() -> Self {
        Self {
            base_delay_ms: 500,      // 500ms base delay between requests
            adaptive_jitter_ms: 250, // Add up to 250ms random jitter
            max_concurrent: 5,       // Allow up to 5 concurrent requests
            max_retries: 3,          // Retry failed requests up to 3 times
        }
    }
}

/// Implements a throttling and exponential backoff middleware for HTTP requests.
///
/// This middleware **limits request concurrency** and applies **adaptive delays**
/// between retries, helping to prevent rate-limiting issues when interacting
/// with APIs that enforce request quotas.
///
/// Requests are throttled using a **semaphore-based** approach, ensuring that
/// the maximum number of concurrent requests does not exceed `max_concurrent`.
///
/// If a request fails, it enters a **retry loop** where each retry is delayed
/// according to an **exponential backoff strategy**.
pub struct DriveThrottleBackoff {
    /// Semaphore controlling the maximum number of concurrent requests.
    semaphore: Arc<Semaphore>,

    /// Defines the backoff and throttling behavior.
    policy: ThrottlePolicy,

    /// Cache layer for detecting previously cached responses.
    cache: Arc<DriveCache>,
}

impl DriveThrottleBackoff {
    /// Creates a new `DriveThrottleBackoff` middleware with the specified throttling policy.
    ///
    /// # Arguments
    ///
    /// * `policy` - The throttling configuration defining concurrency limits, delays, and retry behavior.
    /// * `cache` - The shared cache instance used for **detecting previously cached requests**.
    ///
    /// # Returns
    ///
    /// A new instance of `DriveThrottleBackoff`.
    pub fn new(policy: ThrottlePolicy, cache: Arc<DriveCache>) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(policy.max_concurrent)),
            policy,
            cache,
        }
    }

    #[cfg(any(test, debug_assertions))]
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

#[async_trait]
impl Middleware for DriveThrottleBackoff {
    /// Handles throttling and retry logic for HTTP requests.
    ///
    /// This method:
    /// 1. **Checks the cache**: If the request is already cached, it bypasses throttling.
    /// 2. **Enforces concurrency limits**: Ensures no more than `max_concurrent` requests are in flight.
    /// 3. **Applies an initial delay** before sending the request.
    /// 4. **Retries failed requests**: Uses **exponential backoff** with jitter for failed requests.
    ///
    /// # Arguments
    ///
    /// * `req` - The incoming HTTP request.
    /// * `extensions` - A mutable reference to request extensions, used for tracking metadata.
    /// * `next` - The next middleware in the request chain.
    ///
    /// # Returns
    ///
    /// A `Result<Response, Error>` containing either:
    /// - A successfully processed response.
    /// - An error if the request failed after exhausting all retries.
    ///
    /// # Behavior
    ///
    /// - If the request is **already cached**, the middleware immediately forwards it.
    /// - If **throttling is required**, it waits according to the configured delay.
    /// - If a request fails, **exponential backoff** is applied before retrying.
    async fn handle(
        &self,
        req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> Result<Response, Error> {
        let url = req.url().to_string();

        let cache_key = format!("{} {}", req.method(), &url);

        if self.cache.is_cached(&req).await {
            eprintln!("Using cache for: {}", &cache_key);

            return next.run(req, extensions).await;
        } else {
            eprintln!("No cache found for: {}", &cache_key);
        }

        // Log if the permit is not immediately available
        if self.semaphore.available_permits() == 0 {
            eprintln!(
                "Waiting for permit... ({} in use)",
                self.policy.max_concurrent
            );
        }

        // Acquire the permit and log when granted
        let permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| Error::Middleware(e.into()))?;

        eprintln!(
            "Permit granted: {} ({} permits left)",
            cache_key,
            self.semaphore.available_permits()
        );

        // Hold the permit until this function completes
        let _permit_guard = permit;

        sleep(Duration::from_millis(self.policy.base_delay_ms)).await;

        let mut attempt = 0;

        loop {
            let req_clone = req.try_clone().expect("Request cloning failed");
            let result = next.clone().run(req_clone, extensions).await;

            match result {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                result if attempt >= self.policy.max_retries => return result,
                _ => {
                    attempt += 1;

                    let backoff_duration = {
                        let mut rng = rand::rng();
                        Duration::from_millis(
                            self.policy.base_delay_ms * 2u64.pow(attempt as u32)
                                + rng.random_range(0..=self.policy.adaptive_jitter_ms),
                        )
                    };

                    eprintln!(
                        "Retry {}/{} for URL {} after {} ms",
                        attempt,
                        self.policy.max_retries,
                        url,
                        backoff_duration.as_millis()
                    );

                    sleep(backoff_duration).await;

                    if attempt >= self.policy.max_retries {
                        break;
                    }
                }
            }
        }

        next.run(req, extensions).await
    }
}
