//! Monero amounts.
//!
//! Amounts on the wire are atomic units (piconero); 1 XMR is 10^12 of them.
//!
//! Formatting is done with integer arithmetic only. The tempting
//! `value as f64 / 1e12` is wrong: `f64` carries a 53-bit mantissa, so any
//! amount above 2^53 atomic units (about 9007 XMR) starts losing low digits.
//! Monero's supply cap is deliberately just under `u64::MAX` atomic units, so
//! the range where `f64` is wrong is ordinary, not exotic — a large exchange
//! transfer would render with corrupted digits and look entirely plausible.

use std::fmt;

/// Atomic units in one XMR.
pub const ATOMIC_PER_XMR: u64 = 1_000_000_000_000;

/// Decimal places Monero renders.
pub const DECIMALS: usize = 12;

/// An amount, for rendering.
///
/// Deliberately not an arithmetic type. Chain amounts are summed in the places
/// that need it, on plain `u64` with saturating operators; wrapping this in
/// checked arithmetic nothing calls would be surface to audit for no gain.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Amount(u64);

impl Amount {
    pub const fn from_atomic(atomic: u64) -> Self {
        Self(atomic)
    }

    pub const fn as_atomic(self) -> u64 {
        self.0
    }

    /// Full-precision decimal XMR, always with all 12 places.
    pub fn to_xmr_string(self) -> String {
        let whole = self.0 / ATOMIC_PER_XMR;
        let frac = self.0 % ATOMIC_PER_XMR;
        format!("{whole}.{frac:0width$}", width = DECIMALS)
    }

    /// Decimal XMR with trailing zeros removed, keeping at least one place.
    ///
    /// Used in dense tables where twelve places of mostly zeros is noise.
    pub fn to_trimmed_xmr_string(self) -> String {
        let full = self.to_xmr_string();
        let trimmed = full.trim_end_matches('0');
        if trimmed.ends_with('.') {
            format!("{trimmed}0")
        } else {
            trimmed.to_owned()
        }
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_xmr_string())
    }
}

impl fmt::Debug for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Amount({} atomic = {} XMR)",
            self.0,
            self.to_xmr_string()
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]

    use super::*;

    #[test]
    fn formats_the_boundaries() {
        assert_eq!(Amount::from_atomic(0).to_xmr_string(), "0.000000000000");
        assert_eq!(Amount::from_atomic(1).to_xmr_string(), "0.000000000001");
        assert_eq!(
            Amount::from_atomic(ATOMIC_PER_XMR).to_xmr_string(),
            "1.000000000000"
        );
        assert_eq!(
            Amount::from_atomic(ATOMIC_PER_XMR - 1).to_xmr_string(),
            "0.999999999999"
        );
    }

    /// The real fee from mainnet tx 31feabba…4700, captured in fixtures.
    #[test]
    fn formats_a_real_mainnet_fee() {
        assert_eq!(
            Amount::from_atomic(75_336_453_412).to_xmr_string(),
            "0.075336453412"
        );
    }

    #[test]
    fn formats_the_top_of_the_u64_range_exactly() {
        assert_eq!(
            Amount::from_atomic(u64::MAX).to_xmr_string(),
            "18446744.073709551615"
        );
    }

    /// Guards the reason this module does integer arithmetic. If anyone
    /// "simplifies" it to floating point, this fails.
    #[test]
    fn integer_formatting_beats_the_float_shortcut() {
        let atomic = 9_007_199_254_740_993_u64; // 2^53 + 1
        let exact = Amount::from_atomic(atomic).to_xmr_string();
        assert_eq!(exact, "9007.199254740993");

        let via_float = format!("{:.12}", atomic as f64 / ATOMIC_PER_XMR as f64);
        assert_ne!(
            via_float, exact,
            "f64 must lose precision here; if it does not, this test is not testing anything"
        );
    }

    #[test]
    fn trimming_keeps_one_decimal_place() {
        assert_eq!(Amount::from_atomic(0).to_trimmed_xmr_string(), "0.0");
        assert_eq!(
            Amount::from_atomic(ATOMIC_PER_XMR).to_trimmed_xmr_string(),
            "1.0"
        );
        assert_eq!(
            Amount::from_atomic(1_500_000_000_000).to_trimmed_xmr_string(),
            "1.5"
        );
        assert_eq!(
            Amount::from_atomic(1).to_trimmed_xmr_string(),
            "0.000000000001"
        );
    }

    /// The pre-RingCT amounts from the testnet fixture are real denominations.
    #[test]
    fn formats_pre_ringct_denominations() {
        assert_eq!(
            Amount::from_atomic(7_000_000_000_000).to_trimmed_xmr_string(),
            "7.0"
        );
        assert_eq!(
            Amount::from_atomic(6_000_000_000).to_trimmed_xmr_string(),
            "0.006"
        );
    }

    /// The accessor must return exactly what went in: rendering is the only
    /// transformation this type performs.
    #[test]
    fn the_atomic_value_survives_the_round_trip() {
        for atomic in [0, 1, ATOMIC_PER_XMR, u64::MAX] {
            assert_eq!(Amount::from_atomic(atomic).as_atomic(), atomic);
        }
    }
}
