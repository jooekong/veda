pub mod access_stats;
pub mod answer;
pub mod collection;
pub mod fs;
pub mod search;
pub mod vector;
pub mod workspace;

use veda_types::{Result, VedaError};

const MAX_TX_RETRIES: u32 = 3;

pub async fn retry_on_deadlock<F, Fut, T>(mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Err(VedaError::Deadlock(_)) if attempt < MAX_TX_RETRIES => {
                attempt += 1;
                let backoff_ms = 10 * (1u64 << attempt);
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            }
            other => return other,
        }
    }
}
