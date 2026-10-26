//! Decimal amounts.

use std::fmt;

use alloy::primitives::U256;

/// Allowances at or above this are shown as `unlimited` rather than a 57-digit number.
/// Approvals are commonly set to `type(uint256).max`, but some tooling uses `2^255` or
/// similar, so the threshold is a half-range test rather than an equality check.
pub const UNLIMITED_THRESHOLD_BITS: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Amount {
    pub raw: U256,
    pub decimals: u8,
}

impl Amount {
    pub fn new(raw: U256, decimals: u8) -> Self {
        Self { raw, decimals }
    }

    pub fn zero(decimals: u8) -> Self {
        Self {
            raw: U256::ZERO,
            decimals,
        }
    }

    /// True when this looks like an "infinite" approval — either untouched or partly spent.
    pub fn is_unlimited(&self) -> bool {
        self.raw >= (U256::from(1) << UNLIMITED_THRESHOLD_BITS)
    }

    /// Exactly `type(uint256).max`: an infinite approval nothing has been drawn against.
    ///
    /// Worth separating from [`is_unlimited`], because an approval set to max and then spent
    /// reads *slightly* below max forever after. Both are effectively unlimited, but only one
    /// has never been used.
    pub fn is_max(&self) -> bool {
        self.raw == U256::MAX
    }

    /// How much has been spent against a max approval, if this looks like one.
    pub fn drawn_down(&self) -> Option<U256> {
        self.is_unlimited().then(|| U256::MAX - self.raw)
    }

    /// Lossy projection for metrics, which only speak `f64`.
    ///
    /// This is the single place a float appears. Above ~2^53 base units the low digits are
    /// lost, which is fine for a graph and never acceptable for a comparison — every
    /// threshold judgment works on `raw`, not on this.
    ///
    /// Summing limbs rather than converting through an integer type: an unlimited approval is
    /// near `2^256`, whose whole-token part still overflows `u128`. `f64` reaches `1.8e308`,
    /// so the sum itself never saturates.
    pub fn to_f64(&self) -> f64 {
        let mut raw = 0.0f64;
        for (i, limb) in self.raw.as_limbs().iter().enumerate() {
            raw += *limb as f64 * 2f64.powi(64 * i as i32);
        }
        raw / 10f64.powi(self.decimals as i32)
    }

    pub fn is_zero(&self) -> bool {
        self.raw.is_zero()
    }

    /// Human rendering: thousands-separated integer part, `places` fractional digits.
    ///
    /// The fraction is truncated rather than rounded. Rounding 0.9999 ETH up to "1.0000" in a
    /// low-balance alert would round in the direction that hides the problem.
    pub fn formatted(&self, places: usize) -> String {
        self.render(places, true)
    }

    /// Same value without thousands separators, for `--json`.
    ///
    /// Separators are a table-alignment device; a consumer parsing this field should not have
    /// to strip commas first. `raw` remains the exact value either way.
    pub fn decimal_string(&self, places: usize) -> String {
        self.render(places, false)
    }

    fn render(&self, places: usize, separators: bool) -> String {
        let base = pow10(self.decimals as u32);
        let int_part = self.raw / base;
        let frac_part = self.raw % base;

        let int_digits = int_part.to_string();
        let mut out = if separators {
            group_thousands(&int_digits)
        } else {
            int_digits
        };

        if places > 0 {
            // frac_part < 10^decimals, and places is small, so this cannot overflow U256.
            let scaled = if (places as u32) <= self.decimals as u32 {
                frac_part / pow10(self.decimals as u32 - places as u32)
            } else {
                frac_part * pow10(places as u32 - self.decimals as u32)
            };
            out.push('.');
            out.push_str(&format!("{:0>width$}", scaled.to_string(), width = places));
        }

        out
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.formatted(self.decimals.min(6) as usize))
    }
}

fn pow10(exp: u32) -> U256 {
    U256::from(10).pow(U256::from(exp))
}

fn group_thousands(digits: &str) -> String {
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnitError {
    #[error("not a decimal number")]
    Syntax,
    #[error("more fractional digits than the token has decimals ({decimals})")]
    TooPrecise { decimals: u8 },
}

/// A validated decimal literal from config, held until the token's decimals are known.
///
/// Thresholds are written as `"0.05"` in TOML, but GRT decimals are read from the chain at
/// query time, so the conversion to base units cannot happen at load time. This keeps the
/// syntax check early (config errors should surface before any network I/O) and defers only
/// the scaling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecimalStr(String);

impl DecimalStr {
    pub fn parse(s: &str) -> std::result::Result<Self, UnitError> {
        let (int, frac) = split_decimal(s)?;
        if int.is_empty() && frac.is_empty() {
            return Err(UnitError::Syntax);
        }
        Ok(Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn to_base_units(&self, decimals: u8) -> std::result::Result<U256, UnitError> {
        parse_decimal(&self.0, decimals)
    }
}

impl fmt::Display for DecimalStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn split_decimal(s: &str) -> std::result::Result<(&str, &str), UnitError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(UnitError::Syntax);
    }
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    // No signs, no exponents, no separators. A threshold is a plain positive quantity.
    if !int.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return Err(UnitError::Syntax);
    }
    if int.is_empty() && frac.is_empty() {
        return Err(UnitError::Syntax);
    }
    Ok((int, frac))
}

/// Convert a decimal literal into base units for a token with `decimals` decimals.
pub fn parse_decimal(s: &str, decimals: u8) -> std::result::Result<U256, UnitError> {
    let (int, frac) = split_decimal(s)?;
    if frac.len() > decimals as usize {
        // Silently truncating a threshold would change what the operator asked to be alerted
        // about, so this is an error rather than a rounding decision.
        return Err(UnitError::TooPrecise { decimals });
    }

    let mut digits = String::with_capacity(int.len() + decimals as usize);
    digits.push_str(if int.is_empty() { "0" } else { int });
    digits.push_str(frac);
    for _ in 0..(decimals as usize - frac.len()) {
        digits.push('0');
    }

    U256::from_str_radix(&digits, 10).map_err(|_| UnitError::Syntax)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grt(raw: &str) -> Amount {
        Amount::new(U256::from_str_radix(raw, 10).unwrap(), 18)
    }

    #[test]
    fn json_rendering_omits_thousands_separators() {
        let a = grt("1204882110000000000000000");
        assert_eq!(a.formatted(2), "1,204,882.11");
        assert_eq!(a.decimal_string(2), "1204882.11");
        assert_eq!(grt("0").decimal_string(2), "0.00");
    }

    #[test]
    fn formats_with_thousands_separators() {
        assert_eq!(
            grt("1204882110000000000000000").formatted(2),
            "1,204,882.11"
        );
        assert_eq!(grt("0").formatted(2), "0.00");
        assert_eq!(grt("1000000000000000000").formatted(2), "1.00");
        assert_eq!(grt("999000000000000000").formatted(2), "0.99");
    }

    #[test]
    fn zero_renders_as_zero_not_blank() {
        // The spec reserves blank for "not configured" and `!` for "could not read".
        assert_eq!(grt("0").formatted(2), "0.00");
        assert_eq!(Amount::zero(18).formatted(4), "0.0000");
    }

    #[test]
    fn truncates_rather_than_rounds() {
        // 0.99999... ETH must not render as 1.0000 in a low-balance check.
        let eth = Amount::new(U256::from_str_radix("999990000000000000", 10).unwrap(), 18);
        assert_eq!(eth.formatted(4), "0.9999");
    }

    #[test]
    fn converts_to_f64_without_overflowing_on_huge_values() {
        // An unlimited approval is near 2^256; its whole-token part overflows u128, so the
        // conversion cannot route through an integer type.
        let unlimited = Amount::new(U256::MAX, 18).to_f64();
        assert!(unlimited.is_finite(), "{unlimited}");
        assert!(unlimited > 1e59, "{unlimited}");

        // Ordinary values stay accurate.
        assert_eq!(grt("1500000000000000000").to_f64(), 1.5);
        assert_eq!(grt("0").to_f64(), 0.0);
        let big = grt("1948826585212886128390797180").to_f64();
        assert!((big - 1_948_826_585.21).abs() < 1.0, "{big}");
    }

    #[test]
    fn separates_untouched_max_from_a_spent_one() {
        let untouched = Amount::new(U256::MAX, 18);
        // A max approval with 3,960,100 GRT spent against it — a real case from Arbitrum One.
        let spent = Amount::new(
            U256::MAX - U256::from_str_radix("3960100000000000000000000", 10).unwrap(),
            18,
        );

        assert!(untouched.is_max());
        assert!(!spent.is_max());
        // Both are still effectively infinite.
        assert!(untouched.is_unlimited());
        assert!(spent.is_unlimited());

        assert_eq!(untouched.drawn_down(), Some(U256::ZERO));
        assert_eq!(
            spent.drawn_down().unwrap(),
            U256::from_str_radix("3960100000000000000000000", 10).unwrap()
        );
        // A finite allowance is not a drawn-down infinite one.
        assert_eq!(grt("1000000000000000000").drawn_down(), None);
    }

    #[test]
    fn detects_unlimited_approvals() {
        assert!(Amount::new(U256::MAX, 18).is_unlimited());
        assert!(Amount::new(U256::from(1) << 255, 18).is_unlimited());
        assert!(!Amount::new((U256::from(1) << 255) - U256::from(1), 18).is_unlimited());
        assert!(!grt("1000000000000000000").is_unlimited());
    }

    #[test]
    fn parses_decimal_thresholds() {
        assert_eq!(
            parse_decimal("0.05", 18).unwrap(),
            U256::from_str_radix("50000000000000000", 10).unwrap()
        );
        assert_eq!(
            parse_decimal("10000", 18).unwrap(),
            U256::from_str_radix("10000000000000000000000", 10).unwrap()
        );
        assert_eq!(parse_decimal("0", 18).unwrap(), U256::ZERO);
    }

    #[test]
    fn rejects_malformed_amounts() {
        assert_eq!(DecimalStr::parse("abc"), Err(UnitError::Syntax));
        assert_eq!(DecimalStr::parse("-1"), Err(UnitError::Syntax));
        assert_eq!(DecimalStr::parse("1e18"), Err(UnitError::Syntax));
        assert_eq!(DecimalStr::parse("1,000"), Err(UnitError::Syntax));
        assert_eq!(DecimalStr::parse(""), Err(UnitError::Syntax));
    }

    #[test]
    fn rejects_over_precise_thresholds() {
        assert_eq!(
            parse_decimal("0.1234", 2),
            Err(UnitError::TooPrecise { decimals: 2 })
        );
    }

    #[test]
    fn decimal_str_defers_scaling_to_token_decimals() {
        let d = DecimalStr::parse("1.5").unwrap();
        assert_eq!(
            d.to_base_units(18).unwrap(),
            parse_decimal("1.5", 18).unwrap()
        );
        assert_eq!(d.to_base_units(6).unwrap(), U256::from(1_500_000u64));
    }
}
