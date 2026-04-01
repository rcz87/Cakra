use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use tracing::{info, warn};

/// Execute an async operation with exponential backoff retry logic.
///
/// Retries on any error with delays of 1s, 2s, 4s, 8s, etc.
/// Logs each retry attempt with the error message.
///
/// # Arguments
/// * `f` - A closure that returns a Future producing a Result<T>.
///   The closure is called on each attempt (including retries).
/// * `max_retries` - Maximum number of retry attempts (0 = no retries, just one attempt).
///
/// # Returns
/// The successful result, or the last error if all retries are exhausted.
///
/// # Example
/// ```ignore
/// let result = retry_with_backoff(
///     || async { some_fallible_operation().await },
///     3,
/// ).await?;
/// ```
pub async fn retry_with_backoff<F, Fut, T>(f: F, max_retries: u32) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut last_error: Option<anyhow::Error> = None;
    let total_attempts = max_retries + 1;

    for attempt in 0..total_attempts {
        match f().await {
            Ok(result) => {
                if attempt > 0 {
                    info!(attempt = attempt + 1, "Operation succeeded after retry");
                }
                return Ok(result);
            }
            Err(e) => {
                if attempt < max_retries {
                    let delay_secs = 1u64 << attempt; // 1, 2, 4, 8, ...
                    let delay = Duration::from_secs(delay_secs);

                    warn!(
                        attempt = attempt + 1,
                        max_retries = max_retries,
                        delay_secs = delay_secs,
                        error = %e,
                        "Operation failed, retrying"
                    );

                    tokio::time::sleep(delay).await;
                } else {
                    warn!(
                        attempt = attempt + 1,
                        error = %e,
                        "Operation failed, no more retries"
                    );
                }
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Retry exhausted with no error captured")))
}

/// Retry with a fixed delay between attempts (no exponential backoff).
pub async fn retry_with_fixed_delay<F, Fut, T>(
    f: F,
    max_retries: u32,
    delay: Duration,
) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut last_error: Option<anyhow::Error> = None;
    let total_attempts = max_retries + 1;

    for attempt in 0..total_attempts {
        match f().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                if attempt < max_retries {
                    warn!(
                        attempt = attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        error = %e,
                        "Operation failed, retrying with fixed delay"
                    );
                    tokio::time::sleep(delay).await;
                }
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Retry exhausted with no error captured")))
}

/// Retry with a custom backoff strategy.
pub async fn retry_with_custom_backoff<F, Fut, T, B>(
    f: F,
    max_retries: u32,
    backoff_fn: B,
) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T>>,
    B: Fn(u32) -> Duration,
{
    let mut last_error: Option<anyhow::Error> = None;
    let total_attempts = max_retries + 1;

    for attempt in 0..total_attempts {
        match f().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                if attempt < max_retries {
                    let delay = backoff_fn(attempt);
                    warn!(
                        attempt = attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        error = %e,
                        "Operation failed, retrying with custom backoff"
                    );
                    tokio::time::sleep(delay).await;
                }
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Retry exhausted with no error captured")))
}
