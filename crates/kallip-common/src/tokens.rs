//! Token amount parsing and formatting utilities.
//!
//! Provides human-friendly token unit support (K/M/G) for budget display and input.
//! Only integer amounts are accepted — use `1500K` instead of `1.5M`.

/// Suffix multipliers for token amounts.
const K: u64 = 1_000;
const M: u64 = 1_000_000;
const G: u64 = 1_000_000_000;
/// Practical upper bound for token amounts (1T). Rejects unreasonably large values early.
const MAX_TOKENS: u64 = 1_000_000_000_000;

/// Parse a human-friendly token amount string into a raw token count.
///
/// Supports:
/// - Suffixed values: `100K`, `1500K`, `2G` (case-insensitive, integers only)
/// - Plain numbers: `500000`
/// - Zero in any form: `0`, `0K`, `0M`, `0G`
///
/// # Errors
///
/// Returns a descriptive error string for:
/// - Empty input
/// - Negative values
/// - Non-integer values (decimal points)
/// - Invalid number format
/// - Values exceeding the practical upper bound (1T)
pub fn parse_token_amount(input: &str) -> Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty token amount".into());
    }

    let last = s.as_bytes().last().unwrap();
    let (num_str, multiplier) = match last {
        b'k' | b'K' => (&s[..s.len() - 1], K),
        b'm' | b'M' => (&s[..s.len() - 1], M),
        b'g' | b'G' => (&s[..s.len() - 1], G),
        _ => (s, 1), // plain number
    };

    let num_str = num_str.trim();
    if num_str.is_empty() {
        return Err(format!("missing number before unit in {s:?}"));
    }

    if num_str.contains('.') {
        return Err("token amounts must be integers (e.g. 1500K, 100M)".into());
    }

    let value: u64 = num_str
        .parse()
        .map_err(|_| format!("expected an integer, got {num_str:?} in token amount {s:?}"))?;

    let raw = match value.checked_mul(multiplier) {
        Some(v) => v,
        None => {
            return Err(format!(
                "token amount exceeds maximum ({})",
                format_tokens_m(MAX_TOKENS)
            ));
        }
    };

    if raw > MAX_TOKENS {
        return Err(format!(
            "token amount exceeds maximum ({})",
            format_tokens_m(MAX_TOKENS)
        ));
    }

    Ok(raw)
}

/// Format a raw token count as millions with one decimal place.
///
/// Always uses M (millions) as the display unit per project convention.
/// Examples: `0.5M`, `23.5M`, `100.0M`.
pub fn format_tokens_m(value: u64) -> String {
    let millions = value as f64 / M as f64;
    format!("{millions:.1}M")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_number() {
        assert_eq!(parse_token_amount("1000").unwrap(), 1_000);
    }

    #[test]
    fn parse_zero() {
        assert_eq!(parse_token_amount("0").unwrap(), 0);
        assert_eq!(parse_token_amount("0K").unwrap(), 0);
        assert_eq!(parse_token_amount("0M").unwrap(), 0);
        assert_eq!(parse_token_amount("0G").unwrap(), 0);
        assert_eq!(parse_token_amount("0k").unwrap(), 0);
        assert_eq!(parse_token_amount("0m").unwrap(), 0);
        assert_eq!(parse_token_amount("0g").unwrap(), 0);
    }

    #[test]
    fn parse_k_suffix() {
        assert_eq!(parse_token_amount("100K").unwrap(), 100_000);
        assert_eq!(parse_token_amount("100k").unwrap(), 100_000);
    }

    #[test]
    fn parse_m_suffix() {
        assert_eq!(parse_token_amount("100M").unwrap(), 100_000_000);
        assert_eq!(parse_token_amount("1500K").unwrap(), 1_500_000);
    }

    #[test]
    fn parse_g_suffix() {
        assert_eq!(parse_token_amount("1G").unwrap(), 1_000_000_000);
        assert_eq!(parse_token_amount("2G").unwrap(), 2_000_000_000);
    }

    #[test]
    fn parse_whitespace_trimmed() {
        assert_eq!(parse_token_amount("  100M  ").unwrap(), 100_000_000);
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_token_amount("").is_err());
        assert!(parse_token_amount("  ").is_err());
    }

    #[test]
    fn parse_rejects_decimal() {
        let err = parse_token_amount("1.5M").unwrap_err();
        assert!(err.contains("must be integers"), "{err}");
        assert!(parse_token_amount("0.5").is_err());
    }

    #[test]
    fn parse_rejects_negative() {
        assert!(parse_token_amount("-100M").is_err());
        assert!(parse_token_amount("-1").is_err());
    }

    #[test]
    fn parse_rejects_invalid_number() {
        assert!(parse_token_amount("abc").is_err());
        assert!(parse_token_amount("abcM").is_err());
    }

    #[test]
    fn parse_rejects_suffix_only() {
        assert!(parse_token_amount("M").is_err());
        assert!(parse_token_amount("K").is_err());
    }

    #[test]
    fn parse_rejects_overflow() {
        // 2T = 2_000_000_000_000, exceeds MAX_TOKENS (1T).
        assert!(parse_token_amount("2000G").is_err());
    }

    #[test]
    fn format_tokens_basic() {
        assert_eq!(format_tokens_m(0), "0.0M");
        assert_eq!(format_tokens_m(500_000), "0.5M");
        assert_eq!(format_tokens_m(1_000_000), "1.0M");
        assert_eq!(format_tokens_m(23_456_789), "23.5M");
        assert_eq!(format_tokens_m(100_000_000), "100.0M");
    }
}
