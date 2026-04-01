use anyhow::{bail, Result};
use tracing::warn;

/// Validate that the actual output of a swap does not exceed the maximum
/// allowed slippage relative to the expected output.
///
/// # Arguments
/// * `expected_output` - The expected token output amount from the quote.
/// * `actual_output`   - The actual token output amount received.
/// * `max_slippage_bps` - Maximum acceptable slippage in basis points (1 bps = 0.01%).
///
/// # Errors
/// Returns an error if actual slippage exceeds `max_slippage_bps`.
pub fn validate_slippage(
    expected_output: u64,
    actual_output: u64,
    max_slippage_bps: u16,
) -> Result<()> {
    if expected_output == 0 {
        bail!("Expected output is zero, cannot calculate slippage");
    }

    if actual_output >= expected_output {
        // Positive slippage (we got more than expected) -- always acceptable.
        return Ok(());
    }

    let diff = expected_output - actual_output;
    // Slippage in basis points: (diff / expected) * 10_000
    let actual_slippage_bps = (diff as f64 / expected_output as f64 * 10_000.0) as u16;

    if actual_slippage_bps > max_slippage_bps {
        warn!(
            expected_output,
            actual_output,
            actual_slippage_bps,
            max_slippage_bps,
            "Slippage exceeded maximum allowed"
        );
        bail!(
            "Slippage too high: {} bps (max {} bps). Expected {} tokens, got {}",
            actual_slippage_bps,
            max_slippage_bps,
            expected_output,
            actual_output
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_slippage() {
        assert!(validate_slippage(1000, 1000, 100).is_ok());
    }

    #[test]
    fn test_positive_slippage() {
        // Got more than expected -- always OK.
        assert!(validate_slippage(1000, 1050, 100).is_ok());
    }

    #[test]
    fn test_within_tolerance() {
        // 0.5% slippage = 50 bps, limit is 100 bps.
        assert!(validate_slippage(10000, 9950, 100).is_ok());
    }

    #[test]
    fn test_exceeds_tolerance() {
        // 2% slippage = 200 bps, limit is 100 bps.
        assert!(validate_slippage(10000, 9800, 100).is_err());
    }

    #[test]
    fn test_zero_expected() {
        assert!(validate_slippage(0, 100, 100).is_err());
    }
}
