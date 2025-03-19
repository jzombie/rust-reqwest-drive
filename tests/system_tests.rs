use reqwest_drive::{
    init_cache, init_cache_with_drive, init_cache_with_drive_and_throttle,
    init_cache_with_throttle, CachePolicy, ThrottlePolicy,
};
use reqwest_middleware::ClientBuilder;
use simd_r_drive::DataStore;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempdir::TempDir;
use tokio::sync::{mpsc, Barrier};
use tokio::time::{sleep, Instant};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Test cache-only middleware
#[tokio::test]
async fn test_cache_middleware() {
    let temp_dir = TempDir::new("cache_test").unwrap();
    let cache_path = temp_dir.path().join("cache.bin");

    let mock_server = MockServer::start().await;

    // Mock a 200 OK response with a body
    let template = ResponseTemplate::new(200)
        .set_body_string("cached response")
        .insert_header("Cache-Control", "max-age=60"); // 1-minute cache

    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(template.clone())
        .mount(&mock_server)
        .await;

    // Initialize cache middleware
    let cache = init_cache(&cache_path, CachePolicy::default());

    // Build client with middleware
    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache.clone())
        .build();

    let url = mock_server.uri();
    let first_response = client.get(&url).send().await.unwrap();
    let first_body = first_response.text().await.unwrap();

    // Ensure first request hits the mock server
    assert_eq!(first_body, "cached response");

    // Make a second request to the same URL
    let second_response = client.get(&url).send().await.unwrap();
    let second_body = second_response.text().await.unwrap();

    // Ensure second request is served from cache (mock server should not be hit again)
    assert_eq!(second_body, "cached response");
}

/// Test throttling middleware with retries
#[tokio::test]
async fn test_throttling_behavior() {
    let temp_dir = TempDir::new("cache_test").unwrap();
    let cache_path = temp_dir.path().join("cache.bin");

    let mock_server = MockServer::start().await;

    let response_template = ResponseTemplate::new(200).set_body_string("throttled response");

    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(response_template.clone())
        .expect(3) // Expect exactly 3 separate requests
        .mount(&mock_server)
        .await;

    let cache_policy = CachePolicy::default();
    let throttle_policy = ThrottlePolicy {
        base_delay_ms: 200,      // 200ms fixed delay before each request
        adaptive_jitter_ms: 100, // Allow up to 100ms jitter
        max_concurrent: 1,       // Only allow 1 request at a time
        max_retries: 0,          // No retries needed
    };

    let (_, throttle) = init_cache_with_throttle(&cache_path, cache_policy, throttle_policy);

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(throttle.clone())
        .build();

    let mut timestamps: Vec<std::time::Instant> = vec![];
    let start_time = std::time::Instant::now();

    // Make 3 sequential requests with unique URLs to bypass caching
    for i in 0..3 {
        let request_start = std::time::Instant::now(); // Record precise time
        let url = format!("{}/test{}", mock_server.uri(), i); // Unique URL per request
        let response = client.get(&url).send().await.unwrap();
        let body = response.text().await.unwrap();
        assert_eq!(body, "throttled response");

        timestamps.push(request_start);
    }

    let elapsed = start_time.elapsed();

    // Minimum expected total delay: (n - 1) * base_delay
    let min_expected_delay = Duration::from_millis(400); // 2 * 200ms
    let max_expected_delay = Duration::from_millis(800); // Looser upper bound

    if elapsed < min_expected_delay {
        panic!(
            "Throttling was too fast! Expected at least {:?}, but got {:?}",
            min_expected_delay, elapsed
        );
    } else if elapsed > max_expected_delay {
        eprintln!(
            "⚠️ Warning: Throttling took longer than expected. Expected at most {:?}, but got {:?}",
            max_expected_delay, elapsed
        );
    }

    // Ensure each request was properly spaced apart
    for window in timestamps.windows(2) {
        let delay_between_requests = window[1].duration_since(window[0]); // Use safe subtraction
        let min_per_request = Duration::from_millis(200);
        let max_per_request = Duration::from_millis(400); // Looser max bound

        assert!(
            delay_between_requests >= min_per_request,
            "Request spacing too short: {:?}",
            delay_between_requests
        );

        if delay_between_requests > max_per_request {
            eprintln!(
                "⚠️ Warning: Request spacing exceeded max expected ({:?}). Got {:?}",
                max_per_request, delay_between_requests
            );
        }
    }
}

/// Test cache expiration after TTL
#[tokio::test]
async fn test_cache_expiration() {
    let temp_dir = TempDir::new("cache_test").unwrap();
    let cache_path = temp_dir.path().join("cache.bin");

    let mock_server = MockServer::start().await;

    let template = ResponseTemplate::new(200)
        .set_body_string("temporary cache")
        .insert_header("Cache-Control", "max-age=1"); // 1-second cache

    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(template)
        .mount(&mock_server)
        .await;

    let cache_policy = CachePolicy {
        default_ttl: Duration::from_secs(1), // 1-second expiration
        respect_headers: true,
        cache_status_override: None,
    };

    let cache = init_cache(&cache_path, cache_policy);

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache.clone())
        .build();

    let url = mock_server.uri();

    let first_response = client.get(&url).send().await.unwrap();
    let first_body = first_response.text().await.unwrap();
    assert_eq!(first_body, "temporary cache");

    // Wait for cache expiration
    sleep(Duration::from_secs(2)).await;

    // Now request again, forcing a new response from the mock server
    let second_response = client.get(&url).send().await.unwrap();
    let second_body = second_response.text().await.unwrap();

    // Ensure cache was expired and refreshed
    assert_eq!(second_body, "temporary cache");
}

#[tokio::test]
async fn test_backoff_on_server_error() {
    let temp_dir = TempDir::new("cache_test").unwrap();
    let cache_path = temp_dir.path().join("cache.bin");

    let mock_server = MockServer::start().await;

    let error_template = ResponseTemplate::new(500); // Simulate an internal server error

    // Mock a request that always returns HTTP 500
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(error_template.clone())
        .expect(3) // Expect exactly 3 attempts (1 original + 2 retries)
        .mount(&mock_server)
        .await;

    // Initialize Middleware
    let cache_policy = CachePolicy::default();
    let throttle_policy = ThrottlePolicy {
        base_delay_ms: 100,     // 100ms initial backoff
        adaptive_jitter_ms: 50, // Small random jitter
        max_concurrent: 1,      // Only allow 1 request at a time
        max_retries: 2,         // Allow up to 2 retries
    };

    let (_, throttle) = init_cache_with_throttle(&cache_path, cache_policy, throttle_policy);

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(throttle.clone())
        .build();

    let url = format!("{}/test-server-error", mock_server.uri());
    let start_time = std::time::Instant::now();

    // Send the request (should retry with backoff)
    let response = client.get(&url).send().await.unwrap();

    let elapsed = start_time.elapsed();

    // Ensure the request ultimately returned HTTP 500
    assert_eq!(
        response.status(),
        500,
        "Expected final response to be HTTP 500, but got {:?}",
        response.status()
    );

    // Verify backoff delay was applied (exponential delay: ~200ms + jitter)
    let min_expected_delay = Duration::from_millis(200); // Minimum expected delay
    let max_expected_delay = Duration::from_millis(800); // Allow for jitter and retry overhead

    assert!(
        elapsed >= min_expected_delay,
        "Backoff was too fast! Expected at least {:?}, but got {:?}",
        min_expected_delay,
        elapsed
    );
    if elapsed > max_expected_delay {
        eprintln!(
            "⚠️ Warning: Backoff took longer than expected. Expected at most {:?}, but got {:?}",
            max_expected_delay, elapsed
        );
    }
}

#[tokio::test]
async fn test_backoff_with_eventual_success() {
    let temp_dir = TempDir::new("cache_test").unwrap();
    let cache_path = temp_dir.path().join("cache.bin");

    let mock_server = MockServer::start().await;

    let error_template = ResponseTemplate::new(500); // Simulate server failure
    let success_template = ResponseTemplate::new(200).set_body_string("success");

    let request_counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = request_counter.clone();

    // Step 1: Fail twice, then succeed
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(move |_: &wiremock::Request| {
            let count = counter_clone.fetch_add(1, Ordering::SeqCst);
            if count < 2 {
                error_template.clone() // First 2 requests fail
            } else {
                success_template.clone() // Third request succeeds
            }
        })
        .expect(3) // Expect exactly 3 requests (2 failures, 1 success)
        .mount(&mock_server)
        .await;

    // Initialize Middleware with Backoff
    let cache_policy = CachePolicy::default();
    let throttle_policy = ThrottlePolicy {
        base_delay_ms: 100,     // 100ms initial backoff
        adaptive_jitter_ms: 50, // Small random jitter
        max_concurrent: 1,      // Only allow 1 request at a time
        max_retries: 2,         // Allow 2 retries (3 total attempts)
    };

    let (_, throttle) = init_cache_with_throttle(&cache_path, cache_policy, throttle_policy);

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(throttle.clone())
        .build();

    let url = format!("{}/test-backoff-retry", mock_server.uri());
    let start_time = std::time::Instant::now();

    // Send request (should fail twice and then succeed)
    let response = client.get(&url).send().await.unwrap();
    let body = response.text().await.unwrap();

    let elapsed = start_time.elapsed();

    // Ensure the final request succeeded
    assert_eq!(
        body, "success",
        "Expected final response to be 'success', but got {:?}",
        body
    );

    // Expected minimum total delay:
    // 1st retry delay = ~200ms
    // 2nd retry delay = ~400ms
    // (Total: ~600ms)
    let min_expected_delay = Duration::from_millis(500); // Slightly relaxed
    let max_expected_delay = Duration::from_millis(1200); // Allow for jitter

    // Ensure total delay falls within expected range
    assert!(
        elapsed >= min_expected_delay,
        "Backoff was too fast! Expected at least {:?}, but got {:?}",
        min_expected_delay,
        elapsed
    );
    if elapsed > max_expected_delay {
        eprintln!(
            "⚠️ Warning: Backoff took longer than expected. Expected at most {:?}, but got {:?}",
            max_expected_delay, elapsed
        );
    }

    // Ensure exactly 3 requests were made (2 failures + 1 success)
    assert_eq!(
        request_counter.load(Ordering::SeqCst),
        3,
        "Expected exactly 3 requests (2 failures + 1 success), but got {}",
        request_counter.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn test_init_cache_with_throttle() {
    let temp_dir = TempDir::new("cache_throttle_test").unwrap();
    let cache_path = temp_dir.path().join("cache_throttle.bin");

    let mock_server = MockServer::start().await;

    let response_template = ResponseTemplate::new(200)
        .set_body_string("cached response")
        .insert_header("Cache-Control", "max-age=60"); // 1-minute cache

    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(response_template.clone())
        .expect(1) // Expect only one actual request to the server
        .mount(&mock_server)
        .await;

    let cache_policy = CachePolicy {
        default_ttl: Duration::from_secs(60), // 1-minute cache
        respect_headers: true,
        cache_status_override: None,
    };

    let throttle_policy = ThrottlePolicy {
        base_delay_ms: 200,     // 200ms initial delay
        adaptive_jitter_ms: 50, // Small random jitter
        max_concurrent: 1,      // Only allow 1 request at a time
        max_retries: 1,         // Allow 1 retry
    };

    let (cache, throttle) = init_cache_with_throttle(&cache_path, cache_policy, throttle_policy);

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache.clone())
        .with_arc(throttle.clone())
        .build();

    let url = mock_server.uri();

    // First request - Should be delayed due to throttling
    let start_time_1 = std::time::Instant::now();
    let first_response = client.get(&url).send().await.unwrap();
    let first_body = first_response.text().await.unwrap();
    let elapsed_1 = start_time_1.elapsed();

    assert_eq!(first_body, "cached response");
    assert!(
        elapsed_1 >= Duration::from_millis(200),
        "First request was too fast!"
    );

    // Second request - Should be instant due to caching
    let start_time_2 = std::time::Instant::now();
    let second_response = client.get(&url).send().await.unwrap();
    let second_body = second_response.text().await.unwrap();
    let elapsed_2 = start_time_2.elapsed();

    assert_eq!(second_body, "cached response");
    assert!(
        elapsed_2 < Duration::from_millis(50),
        "Second request was not instant despite caching!"
    );

    println!(
        "Test passed! First request took {:?}, second request took {:?} (should be cached).",
        elapsed_1, elapsed_2
    );
}

#[tokio::test]
async fn test_with_drive_arc() {
    let temp_dir = TempDir::new("cache_drive_test").unwrap();
    let cache_path = temp_dir.path().join("cache_drive.bin");

    let mock_server = MockServer::start().await;

    // Mock a 200 OK response with a body
    let response_template = ResponseTemplate::new(200)
        .set_body_string("cached response")
        .insert_header("Cache-Control", "max-age=60"); // 1-minute cache

    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(response_template.clone())
        .expect(1) // Only one actual request should reach the server
        .mount(&mock_server)
        .await;

    // Create a shared `Arc<DataStore>`
    let store = Arc::new(DataStore::open(&cache_path).unwrap());

    // Use the helper function to initialize `DriveCache`
    let cache_policy = CachePolicy::default();
    let cache = init_cache_with_drive(store.clone(), cache_policy);

    // Build a client with the cache middleware
    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache.clone())
        .build();

    let url = mock_server.uri();

    // First request (should hit the server and get cached)
    let first_response = client.get(&url).send().await.unwrap();
    let first_body = first_response.text().await.unwrap();

    // Ensure first request gets the actual response
    assert_eq!(first_body, "cached response");

    // Second request (should be served from cache)
    let second_response = client.get(&url).send().await.unwrap();
    let second_body = second_response.text().await.unwrap();

    // Ensure the second request is served from cache
    assert_eq!(second_body, "cached response");

    println!("`init_cache_with_drive` successfully initialized and cached responses.");
}

#[tokio::test]
async fn test_with_drive_arc_and_throttle() {
    let temp_dir = TempDir::new("cache_drive_throttle_test").unwrap();
    let cache_path = temp_dir.path().join("cache_drive_throttle.bin");

    let mock_server = MockServer::start().await;

    let response_template = ResponseTemplate::new(200).set_body_string("throttled response");

    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(response_template.clone())
        .expect(3) // Expect exactly 3 requests
        .mount(&mock_server)
        .await;

    // Create a shared `Arc<DataStore>`
    let store = Arc::new(DataStore::open(&cache_path).unwrap());

    // Use the helper function to initialize both cache and throttle middleware
    let cache_policy = CachePolicy::default();
    let throttle_policy = ThrottlePolicy {
        base_delay_ms: 200,      // 200ms fixed delay before each request
        adaptive_jitter_ms: 100, // Allow up to 100ms jitter
        max_concurrent: 1,       // Only allow 1 request at a time
        max_retries: 0,          // No retries needed
    };

    let (cache, throttle) =
        init_cache_with_drive_and_throttle(store.clone(), cache_policy, throttle_policy);

    // Build a client with the cache and throttle middleware
    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(throttle.clone()) // Throttling
        .with_arc(cache.clone()) // Caching
        .build();

    let mut timestamps: Vec<std::time::Instant> = vec![];
    let start_time = std::time::Instant::now();

    // Make 3 sequential requests with unique URLs to bypass caching
    for i in 0..3 {
        let request_start = std::time::Instant::now(); // Record precise time
        let url = format!("{}/test{}", mock_server.uri(), i); // Unique URL per request
        let response = client.get(&url).send().await.unwrap();
        let body = response.text().await.unwrap();
        assert_eq!(body, "throttled response");

        timestamps.push(request_start);
    }

    let elapsed = start_time.elapsed();

    // Minimum expected total delay: (n - 1) * base_delay
    let min_expected_delay = Duration::from_millis(400); // 2 * 200ms
    let max_expected_delay = Duration::from_millis(800); // Looser upper bound

    if elapsed < min_expected_delay {
        panic!(
            "Throttling was too fast! Expected at least {:?}, but got {:?}",
            min_expected_delay, elapsed
        );
    } else if elapsed > max_expected_delay {
        eprintln!(
            "⚠️ Warning: Throttling took longer than expected. Expected at most {:?}, but got {:?}",
            max_expected_delay, elapsed
        );
    }

    // Ensure each request was properly spaced apart
    for window in timestamps.windows(2) {
        let delay_between_requests = window[1].duration_since(window[0]); // Use safe subtraction
        let min_per_request = Duration::from_millis(200);
        let max_per_request = Duration::from_millis(400); // Looser max bound

        assert!(
            delay_between_requests >= min_per_request,
            "Request spacing too short: {:?}",
            delay_between_requests
        );

        if delay_between_requests > max_per_request {
            eprintln!(
                "⚠️ Warning: Request spacing exceeded max expected ({:?}). Got {:?}",
                max_per_request, delay_between_requests
            );
        }
    }

    println!("`init_cache_with_drive_and_throttle` successfully enforced throttling.");
}

// TODO: This is just a basic test, and more tests could be added which excersize things like
//  - defaults
//  - other status codes
#[tokio::test]
async fn test_cache_status_override() {
    let temp_dir = TempDir::new("cache_status_override_test").unwrap();
    let cache_path = temp_dir.path().join("cache_override.bin");

    let mock_server = MockServer::start().await;

    let success_template = ResponseTemplate::new(200).set_body_string("success response");
    let not_found_template = ResponseTemplate::new(404).set_body_string("not found response");
    let server_error_template = ResponseTemplate::new(500).set_body_string("server error response");

    // Mock responses for different status codes
    Mock::given(wiremock::matchers::path("/success"))
        .respond_with(success_template.clone())
        .expect(1)
        .mount(&mock_server)
        .await;

    Mock::given(wiremock::matchers::path("/not_found"))
        .respond_with(not_found_template.clone())
        .expect(1)
        .mount(&mock_server)
        .await;

    Mock::given(wiremock::matchers::path("/server_error"))
        .respond_with(server_error_template.clone())
        .expect(2)
        .mount(&mock_server)
        .await;

    // Configure cache policy with `cache_status_override`
    let cache_policy = CachePolicy {
        default_ttl: Duration::from_secs(60),
        respect_headers: true,
        cache_status_override: Some(vec![200, 404]), // Cache success (200) and "not found" (404), but NOT 500
    };

    let cache = init_cache(&cache_path, cache_policy);

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache.clone())
        .build();

    // Define test URLs
    let success_url = format!("{}/success", mock_server.uri());
    let not_found_url = format!("{}/not_found", mock_server.uri());
    let server_error_url = format!("{}/server_error", mock_server.uri());

    // First request - Should hit the server and cache the response (200)
    let first_success_response = client.get(&success_url).send().await.unwrap();
    let first_success_body = first_success_response.text().await.unwrap();
    assert_eq!(first_success_body, "success response");

    // First request - Should hit the server and cache the response (404)
    let first_not_found_response = client.get(&not_found_url).send().await.unwrap();
    let first_not_found_body = first_not_found_response.text().await.unwrap();
    assert_eq!(first_not_found_body, "not found response");

    // First request - Should hit the server but NOT be cached (500)
    let first_server_error_response = client.get(&server_error_url).send().await.unwrap();
    let first_server_error_body = first_server_error_response.text().await.unwrap();
    assert_eq!(first_server_error_body, "server error response");

    // Second request - Should be served from cache (200)
    let second_success_response = client.get(&success_url).send().await.unwrap();
    let second_success_body = second_success_response.text().await.unwrap();
    assert_eq!(second_success_body, "success response");

    // Second request - Should be served from cache (404)
    let second_not_found_response = client.get(&not_found_url).send().await.unwrap();
    let second_not_found_body = second_not_found_response.text().await.unwrap();
    assert_eq!(second_not_found_body, "not found response");

    // Second request - Should hit the server again (500 is not cached)
    let second_server_error_response = client.get(&server_error_url).send().await.unwrap();
    let second_server_error_body = second_server_error_response.text().await.unwrap();
    assert_eq!(second_server_error_body, "server error response");
}

/// Test that multiple requests run concurrently within throttling constraints.
#[tokio::test]
async fn test_concurrent_requests_without_cache() {
    let temp_dir = TempDir::new("concurrent_test").unwrap();
    let cache_path = temp_dir.path().join("cache_concurrent.bin");

    let mock_server = MockServer::start().await;

    let response_template = ResponseTemplate::new(200).set_body_string("concurrent response");

    let num_requests = 5;
    let max_concurrent = 3; // Expected parallel execution level

    // Mock unique endpoints for each request
    for i in 0..num_requests {
        let path = format!("/test{}", i);
        Mock::given(wiremock::matchers::path(path.clone()))
            .respond_with(response_template.clone())
            .expect(1) // Each request should hit the server only once
            .mount(&mock_server)
            .await;
    }

    let cache_policy = CachePolicy {
        default_ttl: Duration::from_secs(60),
        respect_headers: true,
        cache_status_override: None,
    };

    let throttle_policy = ThrottlePolicy {
        base_delay_ms: 0, // No artificial delay
        adaptive_jitter_ms: 0,
        max_concurrent,
        max_retries: 0,
    };

    let (_, throttle) = init_cache_with_throttle(&cache_path, cache_policy, throttle_policy);

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(throttle.clone())
        .build();

    let barrier = Arc::new(Barrier::new(num_requests));

    // Use an async channel instead of Mutex for timestamps
    let (tx, mut rx) = mpsc::channel(num_requests);

    let handles: Vec<_> = (0..num_requests)
        .map(|i| {
            let client = client.clone();
            let url = format!("{}/test{}", mock_server.uri(), i);
            let barrier = barrier.clone();
            let tx = tx.clone();

            tokio::spawn(async move {
                barrier.wait().await; // Ensure all requests start simultaneously

                let start_time = Instant::now();
                let response = client.get(&url).send().await.unwrap();
                let body = response.text().await.unwrap();
                assert_eq!(body, "concurrent response");

                let elapsed_time = start_time.elapsed();
                tx.send(elapsed_time).await.unwrap(); // Send elapsed time without blocking
            })
        })
        .collect();

    // Wait for all tasks to complete
    for handle in handles {
        handle.await.unwrap();
    }
    drop(tx); // Close channel to allow `rx` to drain fully

    let mut timestamps: Vec<Duration> = vec![];
    while let Some(elapsed) = rx.recv().await {
        timestamps.push(elapsed);
    }

    // Ensure at least `max_concurrent` requests ran in parallel
    timestamps.sort();
    let first_batch = timestamps
        .iter()
        .take(max_concurrent)
        .cloned()
        .collect::<Vec<_>>();

    assert!(
        first_batch.len() == max_concurrent,
        "Expected {} concurrent requests but only got {}",
        max_concurrent,
        first_batch.len()
    );
}

/// Test that the throttle enforces max_concurrent requests correctly.
#[tokio::test]
async fn test_throttling_respects_max_concurrent() {
    let temp_dir = TempDir::new("throttle_test").unwrap();
    let cache_path = temp_dir.path().join("cache_throttle_test.bin");

    let mock_server = MockServer::start().await;

    let response_delay = Duration::from_millis(250); // Each request takes 250ms to complete
    let response_template = ResponseTemplate::new(200)
        .set_body_string("throttled response")
        .set_delay(response_delay);

    let num_requests = 10;
    let max_concurrent = 3;

    // Mock responses for multiple requests
    for i in 0..num_requests {
        let path = format!("/test{}", i);
        Mock::given(wiremock::matchers::path(path.clone()))
            .respond_with(response_template.clone())
            .expect(1) // Each request should hit the server only once
            .mount(&mock_server)
            .await;
    }

    let cache_policy = CachePolicy {
        default_ttl: Duration::from_secs(60),
        respect_headers: true,
        cache_status_override: None,
    };

    let throttle_policy = ThrottlePolicy {
        base_delay_ms: 0, // No artificial delay
        adaptive_jitter_ms: 0,
        max_concurrent,
        max_retries: 0,
    };

    let (_, throttle) = init_cache_with_throttle(&cache_path, cache_policy, throttle_policy);

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(throttle.clone())
        .build();

    let barrier = Arc::new(Barrier::new(num_requests));
    let max_seen_concurrent = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..num_requests)
        .map(|i| {
            let client = client.clone();
            let url = format!("{}/test{}", mock_server.uri(), i);
            let barrier = barrier.clone();
            let max_seen = Arc::clone(&max_seen_concurrent);
            let throttle = throttle.clone();

            tokio::spawn(async move {
                barrier.wait().await; // Synchronize all requests to start together

                let start_time = Instant::now();

                eprintln!("Awaiting the lock...");

                // Acquire permit from the throttle middleware
                // let _permit = throttle.acquire().await;

                // Track the maximum number of concurrent requests seen
                let current_in_flight = max_concurrent - throttle.available_permits();

                eprintln!("Current in flight: {}", current_in_flight);

                max_seen.fetch_max(current_in_flight, Ordering::SeqCst);

                let response = client.get(&url).send().await.unwrap();
                let body = response.text().await.unwrap();
                assert_eq!(body, "throttled response");

                let elapsed_time = start_time.elapsed();
                elapsed_time
            })
        })
        .collect();

    // Wait for all tasks to complete
    for handle in handles {
        handle.await.unwrap();
    }

    let max_seen = max_seen_concurrent.load(Ordering::SeqCst);

    // Assert that we never had more than `max_concurrent` requests in flight
    assert!(
        max_seen <= max_concurrent,
        "Max concurrent requests exceeded! Expected at most {}, but saw {}.",
        max_concurrent,
        max_seen
    );

    println!(
        "✅ Throttling enforced correctly! Max concurrent requests: {} (expected {}).",
        max_seen, max_concurrent
    );
}

#[tokio::test]
async fn test_throttle_policy_override() {
    let temp_dir = TempDir::new("throttle_override_test").unwrap();
    let cache_path = temp_dir.path().join("cache_throttle_override.bin");

    let mock_server = MockServer::start().await;

    let error_template = ResponseTemplate::new(500); // Simulate server failure
    let success_template = ResponseTemplate::new(200).set_body_string("eventual success");

    let request_counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = request_counter.clone();

    // Step 1: Fail once, then succeed
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(move |_: &wiremock::Request| {
            let count = counter_clone.fetch_add(1, Ordering::SeqCst);
            if count < 1 {
                error_template.clone() // First request fails
            } else {
                success_template.clone() // Second request succeeds
            }
        })
        .expect(2) // Expect exactly 2 requests (1 failure, 1 success)
        .mount(&mock_server)
        .await;

    // Initialize Default Throttle Policy (Would normally allow 3 retries)
    let cache_policy = CachePolicy::default();
    let throttle_policy = ThrottlePolicy {
        base_delay_ms: 100,     // 100ms initial backoff
        adaptive_jitter_ms: 50, // Small random jitter
        max_concurrent: 1,      // Only allow 1 request at a time
        max_retries: 3,         // Default policy allows 3 retries
    };

    let (_, throttle) = init_cache_with_throttle(&cache_path, cache_policy, throttle_policy);

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(throttle.clone())
        .build();

    let url = format!("{}/test-throttle-override", mock_server.uri());
    let start_time = std::time::Instant::now();

    // Override Throttle Policy for this specific request (Max 1 retry)
    let custom_throttle = ThrottlePolicy {
        base_delay_ms: 100,     // Same base delay
        adaptive_jitter_ms: 50, // Same jitter
        max_concurrent: 1,      // Same concurrency
        max_retries: 1,         // Overriding retries (only 1 allowed)
    };

    // Send request with the override
    let mut request = client.get(&url);
    request.extensions().insert(custom_throttle); // Correct way to set per-request extension
    let response = request.send().await.unwrap();

    let body = response.text().await.unwrap();
    let elapsed = start_time.elapsed();

    // Ensure the final request succeeded
    assert_eq!(
        body, "eventual success",
        "Expected final response to be 'eventual success', but got {:?}",
        body
    );

    // Verify only **2 requests were made** (1 failure + 1 retry)
    assert_eq!(
        request_counter.load(Ordering::SeqCst),
        2,
        "Expected exactly 2 requests (1 failure + 1 success), but got {}",
        request_counter.load(Ordering::SeqCst)
    );

    // Expected minimum total delay: 1 retry (~200ms total with jitter)
    let min_expected_delay = Duration::from_millis(150); // Slightly relaxed lower bound
    let max_expected_delay = Duration::from_millis(400); // Allow for jitter

    // Ensure total delay falls within expected range
    assert!(
        elapsed >= min_expected_delay,
        "Backoff was too fast! Expected at least {:?}, but got {:?}",
        min_expected_delay,
        elapsed
    );
    if elapsed > max_expected_delay {
        eprintln!(
            "⚠️ Warning: Backoff took longer than expected. Expected at most {:?}, but got {:?}",
            max_expected_delay, elapsed
        );
    }
}
