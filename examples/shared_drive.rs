use reqwest_drive::{init_cache_with_drive_and_throttle, CachePolicy, ThrottlePolicy};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use simd_r_drive::DataStore;
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;
use tempdir::TempDir;

#[tokio::main]
async fn main() {
    let temp_dir = TempDir::new("cache_test").unwrap();
    let cache_path = temp_dir.path().join("cache.bin");

    // Initialize shared DataStore for caching
    let store = Arc::new(DataStore::open(&cache_path).expect("Failed to open DataStore"));

    // Define cache and throttle policies
    let cache_policy = CachePolicy {
        default_ttl: Duration::from_secs(60), // Cache responses for 60s
        respect_headers: true, // Use headers for TTL when available
        cache_status_override: None
    };

    let throttle_policy = ThrottlePolicy {
        base_delay_ms: 100,  // 100ms initial delay
        adaptive_jitter_ms: 50, // Small randomness to prevent bursts
        max_concurrent: 1, // Allow 1 request at a time
        max_retries: 2, // Allow up to 2 retries (3 total attempts)
    };

    // Initialize middleware using an existing `DataStore`
    let (cache, throttle) = init_cache_with_drive_and_throttle(store.clone(), cache_policy, throttle_policy);

    // Build Reqwest Client with Middleware
    let client: ClientWithMiddleware = ClientBuilder::new(reqwest::Client::new())
        .with_arc(throttle.clone()) // Add throttling/backoff layer
        .with_arc(cache.clone()) // Add caching layer
        .build();

    // Test URL (Stable endpoint that always returns 200 OK)
    let url = "https://httpbin.org/get";

    let start_time = Instant::now();

    println!("🌍 Sending request to: {}", url);

    let response = match client.get(url).send().await {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!("❌ Request failed after retries: {:?}", err);
            return;
        }
    };

    let elapsed = start_time.elapsed();

    // Output Results
    println!("✅ Final Response Status: {}", response.status());
    if let Ok(body) = response.text().await {
        println!("📜 Response Body: {}", body);
    }

    println!(
        "⏳ Total Time Taken (including throttling): {:?}",
        elapsed
    );

    // Run the request again to test caching
    println!("🔄 Sending another request (should be cached)...");
    let start_time_cached = Instant::now();

    let cached_response = client.get(url).send().await.unwrap();
    let cached_elapsed = start_time_cached.elapsed();

    println!("✅ Cached Response Status: {}", cached_response.status());

    if let Ok(cached_body) = cached_response.text().await {
        println!("📜 Cached Response Body: {}", cached_body);
    }

    println!(
        "⚡ Cached Request Time Taken: {:?} (should be near-instant)",
        cached_elapsed
    );
}
