use reqwest_drive::{CachePolicy, init_cache_process_scoped};
use reqwest_middleware::ClientBuilder;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let cache = init_cache_process_scoped(CachePolicy {
        default_ttl: Duration::from_secs(60),
        respect_headers: true,
        cache_status_override: None,
    })
    .expect("initialize process-scoped cache");

    let client = ClientBuilder::new(reqwest::Client::new())
        .with_arc(cache)
        .build();

    let first = client
        .get("https://httpbin.org/get")
        .send()
        .await
        .expect("first request")
        .status();

    let second = client
        .get("https://httpbin.org/get")
        .send()
        .await
        .expect("second request")
        .status();

    println!("first status: {first}");
    println!("second status (likely cached): {second}");
}
