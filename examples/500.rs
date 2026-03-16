use reqwest_drive::{CachePolicy, DriveCache, DriveThrottleBackoff, ThrottlePolicy};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use std::{sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::time::Instant;

#[tokio::main]
async fn main() {
    let temp_dir = TempDir::new().unwrap();
    let cache_path = temp_dir.path().join("cache.bin");

    // Configure Cache Settings
    let cache_policy = CachePolicy {
        default_ttl: Duration::from_secs(60), // Cache responses for 60s
        respect_headers: true,                // Use headers for TTL when available
        cache_status_override: None,
    };

    // Configure Throttling & Backoff Settings
    let throttle_policy = ThrottlePolicy {
        base_delay_ms: 100,     // 100ms initial delay
        adaptive_jitter_ms: 50, // Add randomness to prevent bursts
        max_concurrent: 1,      // Allow 1 request at a time
        max_retries: 3,         // Allow 3 retries (4 total attempts)
    };

    // Initialize Middleware
    let cache = Arc::new(DriveCache::new(&cache_path, cache_policy));
    let throttle = Arc::new(DriveThrottleBackoff::new(throttle_policy, cache.clone()));

    // Build Reqwest Client with Middleware
    let client: ClientWithMiddleware = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache.clone()) // Add caching layer
        .with_arc(throttle.clone()) // Add throttling/backoff layer
        .build();

    // Test URL (Will fail first, then succeed after retries)
    let url = "https://httpbin.org/status/500"; // Returns 500 to trigger backoff

    let start_time = Instant::now();

    tracing::info!("Sending request to: {}", url);

    let response = match client.get(url).send().await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::error!("❌ Request failed after retries: {:?}", err);
            return;
        }
    };

    let elapsed = start_time.elapsed();

    // Output Results
    tracing::info!("✅ Final Response Status: {}", response.status());
    if let Ok(body) = response.text().await {
        tracing::info!("📜 Response Body: {}", body);
    }

    tracing::info!(
        "⏳ Total Time Taken (including retries & backoff): {:?}",
        elapsed
    );
}
