//! Hex encoding and decoding.
//!
//! Replaces the `hex` crate, last released in 2021. Three small functions are
//! cheaper to own than an unmaintained dependency is to carry.
//!
//! Uppercase input is accepted on decode, matching what it replaced: users
//! paste hashes from block explorers that print them either way.

/// Lowercase hex, two characters per byte.
pub fn encode(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(char::from(digit(byte >> 4)));
        out.push(char::from(digit(byte & 0x0f)));
    }
    out
}

/// Decodes into a caller-sized buffer. `s` must be exactly twice its length.
pub fn decode_to_slice(s: &str, out: &mut [u8]) -> Result<(), DecodeError> {
    let src = s.as_bytes();
    if src.len() != out.len().saturating_mul(2) {
        return Err(DecodeError);
    }
    let (pairs, _) = src.as_chunks::<2>();
    for (slot, &[hi, lo]) in out.iter_mut().zip(pairs) {
        *slot = (value(hi)? << 4) | value(lo)?;
    }
    Ok(())
}

/// Decodes a string of even length into a fresh buffer.
///
/// Odd input needs no check here: it rounds down to a buffer `decode_to_slice`
/// then rejects for being the wrong length.
pub fn decode(s: &str) -> Result<Vec<u8>, DecodeError> {
    let mut out = vec![0u8; s.len() / 2];
    decode_to_slice(s, &mut out)?;
    Ok(out)
}

/// The input was not hex, or was not the length the destination needed.
///
/// Carries no detail because every caller here checks length and alphabet
/// first and reports its own error; this is the unreachable branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeError;

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid hex")
    }
}

impl std::error::Error for DecodeError {}

const fn digit(nibble: u8) -> u8 {
    match nibble {
        0..=9 => b'0' + nibble,
        _ => b'a' + nibble - 10,
    }
}

const fn value(c: u8) -> Result<u8, DecodeError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(DecodeError),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    /// Pinned values, not a round trip: a round trip passes just as happily
    /// with the nibbles swapped or the alphabet shifted.
    #[test]
    fn encoding_is_lowercase_and_big_endian_per_byte() {
        assert_eq!(encode([0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(encode([0x00, 0x0f, 0xf0, 0xff]), "000ff0ff");
        assert_eq!(encode([0x01]), "01", "the high nibble is not dropped");
        assert_eq!(encode([0x10]), "10", "the low nibble is not dropped");
        assert_eq!(encode([0u8; 0]), "");
    }

    #[test]
    fn decoding_reads_the_same_bytes_back() {
        let mut out = [0u8; 4];
        decode_to_slice("deadbeef", &mut out).unwrap();
        assert_eq!(out, [0xde, 0xad, 0xbe, 0xef]);

        let mut out = [0u8; 4];
        decode_to_slice("000ff0ff", &mut out).unwrap();
        assert_eq!(out, [0x00, 0x0f, 0xf0, 0xff]);
    }

    /// Hash32 and PaymentId8 both rely on this; uppercase reaching them is
    /// ordinary, not exotic.
    #[test]
    fn uppercase_and_mixed_case_decode_to_the_same_bytes() {
        let mut upper = [0u8; 4];
        let mut mixed = [0u8; 4];
        decode_to_slice("DEADBEEF", &mut upper).unwrap();
        decode_to_slice("DeAdBeEf", &mut mixed).unwrap();
        assert_eq!(upper, [0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(mixed, [0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn a_wrong_length_or_a_non_hex_character_is_refused() {
        let mut out = [0u8; 4];
        assert_eq!(decode_to_slice("deadbe", &mut out), Err(DecodeError));
        assert_eq!(decode_to_slice("deadbeefff", &mut out), Err(DecodeError));
        assert_eq!(decode_to_slice("deadbeeg", &mut out), Err(DecodeError));
        // 'g' is past 'f' in the alphabet but still a letter, and the byte
        // arithmetic would otherwise fold it silently into a valid value.
        assert_eq!(decode_to_slice("gggggggg", &mut out), Err(DecodeError));
        assert_eq!(decode_to_slice("dead beef", &mut out), Err(DecodeError));
        // Non-ASCII must not be read as bytes: 'é' is two bytes in UTF-8.
        assert_eq!(decode_to_slice("dead\u{e9}ef", &mut out), Err(DecodeError));
    }

    #[test]
    fn decode_allocates_the_right_length_and_rejects_odd_input() {
        assert_eq!(decode("deadbeef").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(decode("").unwrap(), Vec::<u8>::new());
        assert_eq!(decode("abc"), Err(DecodeError));
        assert_eq!(decode("zz"), Err(DecodeError));
    }

    #[test]
    fn every_byte_survives_a_round_trip() {
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&all)).unwrap(), all);
    }
}
