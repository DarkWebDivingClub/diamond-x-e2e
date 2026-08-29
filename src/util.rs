use std::net::TcpListener;
use std::time::Duration;

/// Allocate a free ephemeral port by binding to port 0.
pub fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("read local addr").port();
    drop(listener);
    port
}

/// Create a unique temporary directory path based on current time.
pub fn unique_tmp_dir(prefix: &str) -> String {
    format!(
        "/tmp/{prefix}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos()
    )
}

/// Retry an async predicate until it returns true or the timeout expires.
pub async fn wait_until<F, Fut>(label: &str, timeout: Duration, interval: Duration, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = tokio::time::Instant::now();
    loop {
        if f().await {
            return;
        }
        if start.elapsed() > timeout {
            panic!("timeout waiting for: {label}");
        }
        tokio::time::sleep(interval).await;
    }
}
