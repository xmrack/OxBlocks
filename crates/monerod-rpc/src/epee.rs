//! monerod's binary format, epee "portable storage", for the `.bin`
//! endpoints.
//!
//! One call needs it: `/get_path_by_unified_id.bin`, the only place the FCMP++
//! daemon reports how many outputs its curve tree held as of a block. Every
//! other call this crate makes is JSON, so this module covers exactly what that
//! one exchange needs: an encoder for a flat section of unsigned integers, and
//! a decoder total over anything the daemon could send back.
//!
//! The layout, from `contrib/epee/include/storages/portable_storage_*.h`:
//!
//! * a nine-byte header: two little-endian `u32` signatures, `0x01011101` and
//!   `0x01020101`, and a version byte of 1;
//! * a root section: a count, then that many entries, each a one-byte name
//!   length, the name, a type byte and the value;
//! * a count or length is epee's own varint: the low two bits of the first
//!   byte say whether it occupies 1, 2, 4 or 8 bytes, little-endian, and the
//!   value is what remains after shifting those two bits off;
//! * a type byte with `0x80` set is an array of that type: a count, then the
//!   elements without type bytes of their own. An array of arrays repeats the
//!   type byte for each inner array.
//!
//! The decoder is written for remote input. It allocates nothing that the
//! remaining input does not bound, rejects duplicate keys as epee does, and
//! stops at epee's own recursion limit of 100.

use std::fmt;

const SIGNATURE_A: u32 = 0x0101_1101;
const SIGNATURE_B: u32 = 0x0102_0101;
const FORMAT_VERSION: u8 = 1;
const HEADER_LEN: usize = 9;

const TYPE_INT64: u8 = 1;
const TYPE_INT32: u8 = 2;
const TYPE_INT16: u8 = 3;
const TYPE_INT8: u8 = 4;
const TYPE_UINT64: u8 = 5;
const TYPE_UINT32: u8 = 6;
const TYPE_UINT16: u8 = 7;
const TYPE_UINT8: u8 = 8;
const TYPE_DOUBLE: u8 = 9;
const TYPE_STRING: u8 = 10;
const TYPE_BOOL: u8 = 11;
const TYPE_OBJECT: u8 = 12;
const TYPE_ARRAY: u8 = 13;
const FLAG_ARRAY: u8 = 0x80;

/// epee's `EPEE_PORTABLE_STORAGE_RECURSION_LIMIT` default.
const MAX_DEPTH: usize = 100;

/// Why a body is not a portable-storage document this module can read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EpeeError {
    #[error("the body ended {0} bytes early")]
    Truncated(usize),
    #[error("the body does not start with the portable-storage header")]
    BadHeader,
    #[error("unknown entry type {0}")]
    UnknownType(u8),
    #[error("an array announces {0} elements, more than the body could hold")]
    ImpossibleCount(u64),
    #[error("an entry name is not UTF-8")]
    BadName,
    #[error("the key {0} appears twice in one section")]
    DuplicateKey(String),
    #[error("nesting deeper than {MAX_DEPTH} levels")]
    TooDeep,
    #[error("{0} bytes follow the root section")]
    TrailingBytes(usize),
    #[error("a value does not fit the encoding: {0}")]
    Unencodable(&'static str),
}

/// One decoded value.
///
/// Integers keep their sign rather than their width: every width folds into
/// one of two variants, because a field monerod declares `uint64_t` today may
/// arrive narrower from a daemon that declared it otherwise, and a caller
/// asking for an unsigned number should get it either way.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Signed(i64),
    Unsigned(u64),
    Double(f64),
    Bool(bool),
    /// epee strings are byte strings, and monerod puts raw binary in them.
    Bytes(Vec<u8>),
    Object(Section),
    Array(Vec<Entry>),
}

/// A section: named entries in the order they arrived.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Section(Vec<(String, Entry)>);

impl Section {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// An unsigned integer of any width, or a signed one that is not negative.
    #[must_use]
    pub fn unsigned(&self, name: &str) -> Option<u64> {
        match self.get(name)? {
            Entry::Unsigned(v) => Some(*v),
            Entry::Signed(v) => u64::try_from(*v).ok(),
            _ => None,
        }
    }

    /// A string that is valid UTF-8.
    #[must_use]
    pub fn text(&self, name: &str) -> Option<&str> {
        match self.get(name)? {
            Entry::Bytes(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A field of a request this module can encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field<'a> {
    U64(u64),
    /// Written as an array of `uint64`. epee drops an empty container rather
    /// than writing it, so an empty slice is left out here too.
    U64s(&'a [u64]),
}

/// Encode a root section of named fields.
pub fn encode(fields: &[(&str, Field<'_>)]) -> Result<Vec<u8>, EpeeError> {
    let present: Vec<&(&str, Field<'_>)> = fields
        .iter()
        .filter(|(_, f)| !matches!(f, Field::U64s(v) if v.is_empty()))
        .collect();

    let mut out = Vec::with_capacity(HEADER_LEN + 16 * present.len());
    out.extend_from_slice(&SIGNATURE_A.to_le_bytes());
    out.extend_from_slice(&SIGNATURE_B.to_le_bytes());
    out.push(FORMAT_VERSION);
    put_varint(&mut out, present.len() as u64)?;
    for (name, field) in present {
        let len = u8::try_from(name.len()).map_err(|_| EpeeError::Unencodable("name"))?;
        out.push(len);
        out.extend_from_slice(name.as_bytes());
        match field {
            Field::U64(v) => {
                out.push(TYPE_UINT64);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Field::U64s(vs) => {
                out.push(TYPE_UINT64 | FLAG_ARRAY);
                put_varint(&mut out, vs.len() as u64)?;
                for v in *vs {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
        }
    }
    Ok(out)
}

/// epee's varint. Values of 2^62 and above have no encoding.
fn put_varint(out: &mut Vec<u8>, v: u64) -> Result<(), EpeeError> {
    if v <= 0x3f {
        out.push(u8::try_from(v << 2).map_err(|_| EpeeError::Unencodable("varint"))?);
    } else if v <= 0x3fff {
        let w = u16::try_from(v << 2).map_err(|_| EpeeError::Unencodable("varint"))? | 1;
        out.extend_from_slice(&w.to_le_bytes());
    } else if v <= 0x3fff_ffff {
        let w = u32::try_from(v << 2).map_err(|_| EpeeError::Unencodable("varint"))? | 2;
        out.extend_from_slice(&w.to_le_bytes());
    } else if v <= 0x3fff_ffff_ffff_ffff {
        out.extend_from_slice(&((v << 2) | 3).to_le_bytes());
    } else {
        return Err(EpeeError::Unencodable("varint"));
    }
    Ok(())
}

/// Decode a whole portable-storage document into its root section.
pub fn decode(bytes: &[u8]) -> Result<Section, EpeeError> {
    let mut r = Reader { rest: bytes };
    let header = r.take(HEADER_LEN).map_err(|_| EpeeError::BadHeader)?;
    let (a, b, version) = match header {
        [a0, a1, a2, a3, b0, b1, b2, b3, v] => (
            u32::from_le_bytes([*a0, *a1, *a2, *a3]),
            u32::from_le_bytes([*b0, *b1, *b2, *b3]),
            *v,
        ),
        _ => return Err(EpeeError::BadHeader),
    };
    if a != SIGNATURE_A || b != SIGNATURE_B || version != FORMAT_VERSION {
        return Err(EpeeError::BadHeader);
    }
    let root = r.section(0)?;
    if !r.rest.is_empty() {
        return Err(EpeeError::TrailingBytes(r.rest.len()));
    }
    Ok(root)
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], EpeeError> {
        if self.rest.len() < n {
            return Err(EpeeError::Truncated(n - self.rest.len()));
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], EpeeError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn byte(&mut self) -> Result<u8, EpeeError> {
        Ok(self.array::<1>()?[0])
    }

    fn varint(&mut self) -> Result<u64, EpeeError> {
        let first = *self.rest.first().ok_or(EpeeError::Truncated(1))?;
        let raw = match first & 0x03 {
            0 => u64::from(self.byte()?),
            1 => u64::from(u16::from_le_bytes(self.array()?)),
            2 => u64::from(u32::from_le_bytes(self.array()?)),
            _ => u64::from_le_bytes(self.array()?),
        };
        Ok(raw >> 2)
    }

    /// A count that the remaining bytes could hold at `min` bytes apiece.
    ///
    /// The allocation bound: a count is attacker-chosen, and reserving for it
    /// before checking would let four bytes ask for gigabytes.
    fn count(&mut self, min: usize) -> Result<usize, EpeeError> {
        let n = self.varint()?;
        let possible = self.rest.len() / min.max(1);
        match usize::try_from(n) {
            Ok(n) if n <= possible => Ok(n),
            _ => Err(EpeeError::ImpossibleCount(n)),
        }
    }

    fn section(&mut self, depth: usize) -> Result<Section, EpeeError> {
        if depth > MAX_DEPTH {
            return Err(EpeeError::TooDeep);
        }
        // An entry is at least a name length, a type and a one-byte value.
        let n = self.count(3)?;
        let mut entries: Vec<(String, Entry)> = Vec::with_capacity(n);
        for _ in 0..n {
            let len = usize::from(self.byte()?);
            let name = std::str::from_utf8(self.take(len)?)
                .map_err(|_| EpeeError::BadName)?
                .to_owned();
            if entries.iter().any(|(k, _)| *k == name) {
                return Err(EpeeError::DuplicateKey(name));
            }
            let ty = self.byte()?;
            let value = self.value(ty, depth + 1)?;
            entries.push((name, value));
        }
        Ok(Section(entries))
    }

    fn value(&mut self, ty: u8, depth: usize) -> Result<Entry, EpeeError> {
        if depth > MAX_DEPTH {
            return Err(EpeeError::TooDeep);
        }
        if ty & FLAG_ARRAY != 0 {
            return self.elements(ty & !FLAG_ARRAY, depth);
        }
        self.scalar(ty, depth)
    }

    fn elements(&mut self, ty: u8, depth: usize) -> Result<Entry, EpeeError> {
        let n = self.count(min_len(ty)?)?;
        let mut items = Vec::with_capacity(n);
        for _ in 0..n {
            let item = if ty == TYPE_ARRAY {
                // An inner array carries its own element type.
                let inner = self.byte()?;
                if inner & FLAG_ARRAY == 0 {
                    return Err(EpeeError::UnknownType(inner));
                }
                self.elements(inner & !FLAG_ARRAY, depth + 1)?
            } else {
                self.scalar(ty, depth + 1)?
            };
            items.push(item);
        }
        Ok(Entry::Array(items))
    }

    fn scalar(&mut self, ty: u8, depth: usize) -> Result<Entry, EpeeError> {
        Ok(match ty {
            TYPE_INT64 => Entry::Signed(i64::from_le_bytes(self.array()?)),
            TYPE_INT32 => Entry::Signed(i64::from(i32::from_le_bytes(self.array()?))),
            TYPE_INT16 => Entry::Signed(i64::from(i16::from_le_bytes(self.array()?))),
            TYPE_INT8 => Entry::Signed(i64::from(i8::from_le_bytes(self.array()?))),
            TYPE_UINT64 => Entry::Unsigned(u64::from_le_bytes(self.array()?)),
            TYPE_UINT32 => Entry::Unsigned(u64::from(u32::from_le_bytes(self.array()?))),
            TYPE_UINT16 => Entry::Unsigned(u64::from(u16::from_le_bytes(self.array()?))),
            TYPE_UINT8 => Entry::Unsigned(u64::from(self.byte()?)),
            TYPE_DOUBLE => Entry::Double(f64::from_le_bytes(self.array()?)),
            TYPE_BOOL => Entry::Bool(self.byte()? != 0),
            TYPE_STRING => {
                let len = self.count(1)?;
                Entry::Bytes(self.take(len)?.to_vec())
            }
            TYPE_OBJECT => Entry::Object(self.section(depth)?),
            TYPE_ARRAY => {
                let inner = self.byte()?;
                if inner & FLAG_ARRAY == 0 {
                    return Err(EpeeError::UnknownType(inner));
                }
                self.elements(inner & !FLAG_ARRAY, depth)?
            }
            other => return Err(EpeeError::UnknownType(other)),
        })
    }
}

/// The fewest bytes one element of an array of `ty` can occupy.
const fn min_len(ty: u8) -> Result<usize, EpeeError> {
    Ok(match ty {
        TYPE_INT64 | TYPE_UINT64 | TYPE_DOUBLE => 8,
        TYPE_INT32 | TYPE_UINT32 => 4,
        TYPE_INT16 | TYPE_UINT16 => 2,
        TYPE_INT8 | TYPE_UINT8 | TYPE_BOOL | TYPE_STRING | TYPE_OBJECT | TYPE_ARRAY => 1,
        other => return Err(EpeeError::UnknownType(other)),
    })
}

impl fmt::Display for Section {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.0.iter().map(|(k, _)| k.as_str()).collect();
        write!(f, "{{{}}}", names.join(", "))
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::cast_possible_truncation
    )]

    use super::*;

    const HEADER: [u8; 9] = [0x01, 0x11, 0x01, 0x01, 0x01, 0x01, 0x02, 0x01, 0x01];

    #[test]
    fn a_request_encodes_the_way_epee_lays_it_out() {
        let bytes = encode(&[
            ("as_of_n_blocks", Field::U64(421)),
            ("unified_ids", Field::U64s(&[7])),
        ])
        .unwrap();
        let mut expected = HEADER.to_vec();
        expected.push(2 << 2); // two entries
        expected.push(14);
        expected.extend_from_slice(b"as_of_n_blocks");
        expected.push(TYPE_UINT64);
        expected.extend_from_slice(&421u64.to_le_bytes());
        expected.push(11);
        expected.extend_from_slice(b"unified_ids");
        expected.push(TYPE_UINT64 | FLAG_ARRAY);
        expected.push(1 << 2); // one element
        expected.extend_from_slice(&7u64.to_le_bytes());
        assert_eq!(bytes, expected);
    }

    #[test]
    fn an_empty_array_is_left_out_as_epee_does() {
        let bytes = encode(&[("unified_ids", Field::U64s(&[]))]).unwrap();
        let mut expected = HEADER.to_vec();
        expected.push(0);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn varints_take_the_width_their_value_needs() {
        for (v, width) in [
            (0u64, 1),
            (63, 1),
            (64, 2),
            (16_383, 2),
            (16_384, 4),
            (0x3fff_ffff, 4),
            (0x4000_0000, 8),
        ] {
            let mut out = Vec::new();
            put_varint(&mut out, v).unwrap();
            assert_eq!(out.len(), width, "{v}");
            assert_eq!(Reader { rest: &out }.varint().unwrap(), v);
        }
        assert!(put_varint(&mut Vec::new(), 1 << 62).is_err());
    }

    #[test]
    fn what_this_encodes_this_decodes() {
        let bytes = encode(&[
            ("as_of_n_blocks", Field::U64(421)),
            ("unified_ids", Field::U64s(&[7, 8, 9])),
        ])
        .unwrap();
        let root = decode(&bytes).unwrap();
        assert_eq!(root.unsigned("as_of_n_blocks"), Some(421));
        assert_eq!(
            root.get("unified_ids"),
            Some(&Entry::Array(vec![
                Entry::Unsigned(7),
                Entry::Unsigned(8),
                Entry::Unsigned(9)
            ]))
        );
    }

    /// Nested objects, an array of objects, strings holding raw bytes and a
    /// bool: every shape the path response uses, built by hand.
    #[test]
    fn nested_sections_and_arrays_of_objects_decode() {
        let mut b = HEADER.to_vec();
        b.push(3 << 2);
        b.push(6);
        b.extend_from_slice(b"status");
        b.push(TYPE_STRING);
        b.push(2 << 2);
        b.extend_from_slice(b"OK");
        b.push(5);
        b.extend_from_slice(b"paths");
        b.push(TYPE_OBJECT | FLAG_ARRAY);
        b.push(1 << 2);
        {
            b.push(2 << 2);
            b.push(8);
            b.extend_from_slice(b"leaf_idx");
            b.push(TYPE_UINT32);
            b.extend_from_slice(&5u32.to_le_bytes());
            b.push(4);
            b.extend_from_slice(b"blob");
            b.push(TYPE_STRING);
            b.push(2 << 2);
            b.extend_from_slice(&[0xff, 0x00]);
        }
        b.push(9);
        b.extend_from_slice(b"untrusted");
        b.push(TYPE_BOOL);
        b.push(0);

        let root = decode(&b).unwrap();
        assert_eq!(root.text("status"), Some("OK"));
        assert_eq!(root.get("untrusted"), Some(&Entry::Bool(false)));
        let Some(Entry::Array(paths)) = root.get("paths") else {
            panic!("paths is an array");
        };
        let Entry::Object(first) = &paths[0] else {
            panic!("of objects");
        };
        assert_eq!(first.unsigned("leaf_idx"), Some(5));
        assert_eq!(first.get("blob"), Some(&Entry::Bytes(vec![0xff, 0x00])));
    }

    #[test]
    fn hostile_input_is_refused_rather_than_trusted() {
        assert_eq!(decode(&[]), Err(EpeeError::BadHeader));
        assert_eq!(decode(&[0u8; 9]), Err(EpeeError::BadHeader));

        // A count of a billion elements in a ten-byte body.
        let mut huge = HEADER.to_vec();
        huge.push(1 << 2);
        huge.push(1);
        huge.push(b'x');
        huge.push(TYPE_UINT64 | FLAG_ARRAY);
        huge.extend_from_slice(&((1_000_000_000u32 << 2) | 2).to_le_bytes());
        assert!(matches!(decode(&huge), Err(EpeeError::ImpossibleCount(_))));

        // A duplicate key.
        let mut dup = HEADER.to_vec();
        dup.push(2 << 2);
        for _ in 0..2 {
            dup.push(1);
            dup.push(b'a');
            dup.push(TYPE_UINT8);
            dup.push(1);
        }
        assert_eq!(decode(&dup), Err(EpeeError::DuplicateKey("a".to_owned())));

        // Objects nested past the limit.
        let mut deep = HEADER.to_vec();
        for _ in 0..=MAX_DEPTH {
            deep.push(1 << 2);
            deep.push(1);
            deep.push(b'o');
            deep.push(TYPE_OBJECT);
        }
        deep.push(0);
        assert_eq!(decode(&deep), Err(EpeeError::TooDeep));

        // Truncated in the middle of a value, and bytes after the root.
        let good = encode(&[("n", Field::U64(1))]).unwrap();
        assert!(matches!(
            decode(&good[..good.len() - 1]),
            Err(EpeeError::Truncated(_))
        ));
        let mut long = good.clone();
        long.push(0);
        assert_eq!(decode(&long), Err(EpeeError::TrailingBytes(1)));

        // An unknown type byte.
        let mut unknown = HEADER.to_vec();
        unknown.push(1 << 2);
        unknown.push(1);
        unknown.push(b'z');
        unknown.push(42);
        assert_eq!(decode(&unknown), Err(EpeeError::UnknownType(42)));
    }
}
