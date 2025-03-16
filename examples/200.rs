use reqwest_drive::{CachePolicy, DriveCache, DriveThrottleBackoff, ThrottlePolicy};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use std::{sync::Arc, time::Duration};
use tempdir::TempDir;
use tokio::time::Instant;

#[tokio::main]
async fn main() {
    let temp_dir = TempDir::new("cache_test").unwrap();
    let cache_path = temp_dir.path().join("cache.bin");

    // Configure Cache Settings
    let cache_policy = CachePolicy {
        default_ttl: Duration::from_secs(60), // Cache responses for 60s
        respect_headers: true,                // Use headers for TTL when available
        cache_status_override: None
    };

    // Configure Throttling & Backoff Settings
    let throttle_policy = ThrottlePolicy {
        base_delay_ms: 200,     // 200ms initial delay
        adaptive_jitter_ms: 50, // Add randomness to prevent bursts
        max_concurrent: 1,      // Allow 1 request at a time
        max_retries: 2,         // Allow 2 retries (3 total attempts)
    };

    // Initialize Middleware
    let cache = Arc::new(DriveCache::new(&cache_path, cache_policy));
    let throttle = Arc::new(DriveThrottleBackoff::new(throttle_policy, cache.clone()));

    // Build Reqwest Client with Middleware
    let client: ClientWithMiddleware = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache.clone()) // Add caching layer
        .with_arc(throttle.clone()) // Add throttling/backoff layer
        .build();

    // Test URL (Stable endpoint that always returns 200 OK)
    let url = "https://httpbin.org/get";

    // First request: Expect delay due to throttling
    let start_time_1 = Instant::now();
    println!("🌍 Sending first request to: {}", url);

    let response_1 = match client.get(url).send().await {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!("❌ First request failed after retries: {:?}", err);
            return;
        }
    };

    let elapsed_1 = start_time_1.elapsed();

    println!("✅ First Request Status: {}", response_1.status());
    println!(
        "⏳ First Request Time (including throttling): {:?}",
        elapsed_1
    );

    // Second request: Expect near-instant response due to caching
    let start_time_2 = Instant::now();
    println!("🌍 Sending second request to: {}", url);

    let response_2 = match client.get(url).send().await {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!("❌ Second request failed: {:?}", err);
            return;
        }
    };

    let elapsed_2 = start_time_2.elapsed();

    println!("✅ Second Request Status: {}", response_2.status());
    println!(
        "⚡ Second Request Time (should be fast due to cache): {:?}",
        elapsed_2
    );

    // Compare timings
    println!(
        "\n📊 Comparison: First request took {:?}, second request took {:?} (expected near-instant)",
        elapsed_1, elapsed_2
    );

    // Ensure cache actually worked (second request should be much faster)
    assert!(
        elapsed_2 < Duration::from_millis(50),
        "⚠️ Cache did not work! Second request took too long: {:?}",
        elapsed_2
    );
}
