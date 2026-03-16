use reqwest_drive::{
    CachePolicy, ThrottlePolicy, init_cache_process_scoped_with_throttle,
    init_client_with_cache_and_throttle,
};

#[tokio::main]
async fn main() {
    let (cache, throttle) = init_cache_process_scoped_with_throttle(
        CachePolicy::default(),
        ThrottlePolicy {
            base_delay_ms: 200,
            adaptive_jitter_ms: 100,
            max_concurrent: 2,
            max_retries: 2,
        },
    )
    .expect("initialize process-scoped cache+throttle");

    let client = init_client_with_cache_and_throttle(cache, throttle);

    let response = client
        .get("https://httpbin.org/status/429")
        .send()
        .await
        .expect("request");

    println!("response status: {}", response.status());
}
