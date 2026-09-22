//! A validated 32-byte hash.
//!
//! Block hashes, transaction hashes, key images and output keys are all 32 bytes
//! rendered as 64 hex characters. The C++ explorer re-checks that shape with an
//! ad-hoc regex at each use site, which means a missed check is invisible.
//!
//! Here it is a type. A [`Hash32`] cannot exist unless it parsed, so "this value
//! was validated" is something the compiler knows rather than something a reader
//! has to audit for.

use std::fmt;
use std::str::FromStr;

pub const HASH_LEN: usize = 32;
pub const HASH_HEX_LEN: usize = HASH_LEN * 2;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash32([u8; HASH_LEN]);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HashParseError {
    #[error("expected {HASH_HEX_LEN} hex characters, got {0}")]
    WrongLength(usize),
    #[error("{0:?} is not a hex character")]
    NotHex(char),
}

impl Hash32 {
    pub const fn from_bytes(bytes: [u8; HASH_LEN]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; HASH_LEN] {
        &self.0
    }

    /// The all-zero hash.
    ///
    /// monerod uses this as a sentinel — notably `prunable_hash` on a tx with no
    /// prunable part — so it is a value to recognise, not an error.
    pub const ZERO: Self = Self([0u8; HASH_LEN]);

    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; HASH_LEN]
    }

    pub fn to_hex(self) -> String {
        crate::hex::encode(self.0)
    }
}

impl FromStr for Hash32 {
    type Err = HashParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Check length before decoding. crate::hex::decode would also reject these, but
        // the length is the useful half of the error message for a user who
        // pasted a truncated hash.
        if s.len() != HASH_HEX_LEN {
            return Err(HashParseError::WrongLength(s.len()));
        }
        if let Some(c) = s.chars().find(|c| !c.is_ascii_hexdigit()) {
            return Err(HashParseError::NotHex(c));
        }

        let mut out = [0u8; HASH_LEN];
        crate::hex::decode_to_slice(s, &mut out)
            // Unreachable: length and alphabet are both checked above. Returning
            // an error rather than panicking keeps this total regardless.
            .map_err(|_| HashParseError::WrongLength(s.len()))?;
        Ok(Self(out))
    }
}

impl fmt::Display for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Renders as the full hash rather than a byte array, because these show up in
/// log lines and error messages where the truncated form is useless.
impl fmt::Debug for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash32({self})")
    }
}

impl serde::Serialize for Hash32 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> serde::Deserialize<'de> for Hash32 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = <std::borrow::Cow<'_, str> as serde::Deserialize>::deserialize(d)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    // Panicking is the correct failure mode in a test; the workspace lints
    // exist to keep panics out of request handling, not out of assertions.
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]

    use super::*;

    const REAL: &str = "2917a83ec63c66b14922ec0383ea682d2e3c2708aaeb1434d15762d32984eb83";

    #[test]
    fn round_trips_a_real_tx_hash() {
        let h: Hash32 = REAL.parse().expect("valid hash");
        assert_eq!(h.to_string(), REAL);
        assert_eq!(h.to_hex(), REAL);
    }

    #[test]
    fn uppercase_hex_is_accepted_and_normalised_to_lowercase() {
        // Users paste hashes from anywhere; rejecting uppercase would be a
        // gratuitous failure, but output must be canonical.
        let h: Hash32 = REAL.to_uppercase().parse().expect("uppercase is valid hex");
        assert_eq!(h.to_string(), REAL);
    }

    #[test]
    fn length_errors_are_precise() {
        assert_eq!("".parse::<Hash32>(), Err(HashParseError::WrongLength(0)));
        assert_eq!(
            REAL[..63].parse::<Hash32>(),
            Err(HashParseError::WrongLength(63))
        );
        assert_eq!(
            format!("{REAL}0").parse::<Hash32>(),
            Err(HashParseError::WrongLength(65))
        );
    }

    #[test]
    fn non_hex_is_rejected() {
        let bad = format!("{}zz", &REAL[..62]);
        assert_eq!(bad.parse::<Hash32>(), Err(HashParseError::NotHex('z')));
    }

    /// Multi-byte input must be rejected on length without panicking on a char
    /// boundary — `s.len()` is bytes, and slicing it would be a panic.
    #[test]
    fn multibyte_input_does_not_panic() {
        assert!("é".repeat(32).parse::<Hash32>().is_err());
        assert!("🙂".repeat(16).parse::<Hash32>().is_err());
        // Exactly 64 *bytes* of multi-byte text: passes the length gate, must
        // still fail on the alphabet check rather than slicing mid-character.
        let sixty_four_bytes = "é".repeat(32);
        assert_eq!(sixty_four_bytes.len(), 64);
        assert!(matches!(
            sixty_four_bytes.parse::<Hash32>(),
            Err(HashParseError::NotHex(_))
        ));
    }

    #[test]
    fn zero_hash_is_recognised() {
        let zero: Hash32 = "0".repeat(64).parse().expect("valid");
        assert!(zero.is_zero());
        assert_eq!(zero, Hash32::ZERO);
        let real: Hash32 = REAL.parse().expect("valid");
        assert!(!real.is_zero());
    }

    #[test]
    fn serde_round_trips_through_a_json_string() {
        let h: Hash32 = REAL.parse().expect("valid");
        let json = serde_json::to_string(&h).expect("serialises");
        assert_eq!(json, format!("\"{REAL}\""));
        let back: Hash32 = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back, h);
    }

    #[test]
    fn serde_rejects_a_malformed_hash_rather_than_defaulting() {
        assert!(serde_json::from_str::<Hash32>("\"deadbeef\"").is_err());
        assert!(serde_json::from_str::<Hash32>("123").is_err());
        assert!(serde_json::from_str::<Hash32>("null").is_err());
    }
}
