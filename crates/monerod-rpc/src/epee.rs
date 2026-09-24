//! monerod's binary format, epee "portable storage", for the `.bin`
//! endpoints.
//!
//! One call needs it: `/get_path_by_unified_id.bin`, the cheapest place the
//! FCMP++ daemon reports how many outputs its curve tree held as of a block.
//! (`/getblocks.bin` reports it too, when asked to start a tree sync, beside a
//! batch of whole blocks.) Every other call this crate makes is JSON, so this
//! module covers exactly what that one exchange needs: an encoder for a flat
//! section of unsigned integers, and a reader that pulls a few named scalars
//! out of the root of the answer and walks past everything else.
//!
//! The layout:
//!
//! * a nine-byte header: two little-endian `u32` signatures, `0x01011101` and
//!   `0x01020101`, and a version byte of 1;
//! * a root section: a count, then that many entries, each a one-byte name
//!   length, the name, a type byte and the value;
//! * a count or length is epee's own varint: the low two bits of the first
//!   byte say whether it occupies 1, 2, 4 or 8 bytes, little-endian, and the
//!   value is what remains after shifting those two bits off;
//! * a type byte with `0x80` set is an array of that type: a count, then the
//!   elements without type bytes of their own.
//!
//! The reader is written for remote input: the daemon may be a public node,
//! and the path to it may be plain HTTP. It copies nothing out of a value it
//! skips; the one thing it holds per entry is a borrowed name for each root
//! key, to catch a repeated one. It does one pass over the bytes, so its time
//! grows only linearly, and it caps nesting well inside any stack. Callers
//! also cap the body, e.g. [`crate::types::TreeSizeQuery::MAX_ANSWER_BYTES`].
//!
//! What it refuses: an empty name, a bool other than 0 or 1, a repeated root
//! key, any array of arrays, and bytes after the root. Inside a value it
//! skips it checks framing only -- types, counts and lengths -- since nothing
//! is kept from it.

use std::collections::HashSet;

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

/// Sections nested inside the root, at most.
///
/// The deepest answer this crate reads nests three: the chunks of a path,
/// inside the path, inside a path entry in the root's list of them. Recursion
/// is bounded by this and nothing else, because an array element this reader
/// accepts is a scalar or a section.
pub const MAX_DEPTH: usize = 32;

/// Why a body is not a portable-storage document this module accepts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EpeeError {
    #[error("the body ended {0} bytes early")]
    Truncated(usize),
    #[error("the body does not start with the portable-storage header")]
    BadHeader,
    #[error("unknown entry type {0}")]
    UnknownType(u8),
    #[error("an array of arrays")]
    NestedArray,
    #[error("a count of {0}, more than the body could hold")]
    ImpossibleCount(u64),
    #[error("an entry with an empty name")]
    EmptyName,
    #[error("a bool byte of {0}")]
    BadBool(u8),
    #[error("the root key {0:?} appears twice")]
    DuplicateKey(String),
    #[error("sections nested deeper than {MAX_DEPTH}")]
    TooDeep,
    #[error("{0} bytes follow the root section")]
    TrailingBytes(usize),
    #[error("a value does not fit the encoding: {0}")]
    Unencodable(&'static str),
}

/// A value read from the root section.
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Signed(i64),
    Unsigned(u64),
    Double(f64),
    Bool(bool),
    /// epee strings are byte strings, and monerod puts raw binary in them.
    Bytes(Vec<u8>),
    /// The key is there but holds a section or an array, which this reader
    /// walks past rather than keeping. Recorded so that "present with the
    /// wrong type" is not mistaken for "absent".
    Container,
}

/// The root entries a caller asked for, as found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Root(Vec<(String, Scalar)>);

impl Root {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Scalar> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// An unsigned integer of any width, or a signed one that is not negative.
    #[must_use]
    pub fn unsigned(&self, name: &str) -> Option<u64> {
        match self.get(name)? {
            Scalar::Unsigned(v) => Some(*v),
            Scalar::Signed(v) => u64::try_from(*v).ok(),
            _ => None,
        }
    }

    /// A string that is valid UTF-8.
    #[must_use]
    pub fn text(&self, name: &str) -> Option<&str> {
        match self.get(name)? {
            Scalar::Bytes(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }
}

/// A field of a request this module can encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field<'a> {
    U64(u64),
    /// Written as an array of `uint64`. An empty slice is left out of the
    /// section altogether.
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
        if len == 0 {
            return Err(EpeeError::Unencodable("empty name"));
        }
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

/// Read a whole document, keeping the root entries named in `wanted`.
///
/// Every byte is still checked -- a malformed value anywhere, kept or skipped,
/// fails the read -- but only the wanted scalars are copied out.
pub fn read_root(bytes: &[u8], wanted: &[&str]) -> Result<Root, EpeeError> {
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

    // An entry is at least a name length, a one-byte name, a type and a
    // one-byte value.
    let n = r.count(4)?;
    // Borrowed from the body, so the duplicate check copies nothing and costs
    // one hash per key rather than a scan of every key before it.
    let mut seen: HashSet<&[u8]> = HashSet::with_capacity(n);
    let mut kept = Vec::new();
    for _ in 0..n {
        let name = r.name()?;
        if !seen.insert(name) {
            return Err(EpeeError::DuplicateKey(
                String::from_utf8_lossy(name).into_owned(),
            ));
        }
        let ty = r.byte()?;
        let keep = std::str::from_utf8(name)
            .ok()
            .filter(|n| wanted.contains(n));
        match keep {
            Some(key) => kept.push((key.to_owned(), r.keep(ty)?)),
            None => r.skip(ty, 0)?,
        }
    }
    if !r.rest.is_empty() {
        return Err(EpeeError::TrailingBytes(r.rest.len()));
    }
    Ok(Root(kept))
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

    /// Step over `n` bytes.
    fn advance(&mut self, n: usize) -> Result<(), EpeeError> {
        self.take(n).map(|_| ())
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], EpeeError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn byte(&mut self) -> Result<u8, EpeeError> {
        let [b] = self.array::<1>()?;
        Ok(b)
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
    fn count(&mut self, min: usize) -> Result<usize, EpeeError> {
        let n = self.varint()?;
        let possible = self.rest.len() / min.max(1);
        match usize::try_from(n) {
            Ok(n) if n <= possible => Ok(n),
            _ => Err(EpeeError::ImpossibleCount(n)),
        }
    }

    fn name(&mut self) -> Result<&'a [u8], EpeeError> {
        let len = usize::from(self.byte()?);
        if len == 0 {
            return Err(EpeeError::EmptyName);
        }
        self.take(len)
    }

    fn bool(&mut self) -> Result<bool, EpeeError> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(EpeeError::BadBool(other)),
        }
    }

    /// Read one root value into a [`Scalar`], walking past a container.
    fn keep(&mut self, ty: u8) -> Result<Scalar, EpeeError> {
        if ty & FLAG_ARRAY != 0 || ty == TYPE_OBJECT || ty == TYPE_ARRAY {
            self.skip(ty, 0)?;
            return Ok(Scalar::Container);
        }
        Ok(match ty {
            TYPE_INT64 => Scalar::Signed(i64::from_le_bytes(self.array()?)),
            TYPE_INT32 => Scalar::Signed(i64::from(i32::from_le_bytes(self.array()?))),
            TYPE_INT16 => Scalar::Signed(i64::from(i16::from_le_bytes(self.array()?))),
            TYPE_INT8 => Scalar::Signed(i64::from(i8::from_le_bytes(self.array()?))),
            TYPE_UINT64 => Scalar::Unsigned(u64::from_le_bytes(self.array()?)),
            TYPE_UINT32 => Scalar::Unsigned(u64::from(u32::from_le_bytes(self.array()?))),
            TYPE_UINT16 => Scalar::Unsigned(u64::from(u16::from_le_bytes(self.array()?))),
            TYPE_UINT8 => Scalar::Unsigned(u64::from(self.byte()?)),
            TYPE_DOUBLE => Scalar::Double(f64::from_le_bytes(self.array()?)),
            TYPE_BOOL => Scalar::Bool(self.bool()?),
            TYPE_STRING => {
                let len = self.count(1)?;
                Scalar::Bytes(self.take(len)?.to_vec())
            }
            other => return Err(EpeeError::UnknownType(other)),
        })
    }

    /// Walk past one value of type `ty` inside `depth` enclosing sections.
    ///
    /// Recursion happens only through a section, and every section checks the
    /// depth before it reads anything, so this cannot outrun the stack
    /// whatever the body says.
    fn skip(&mut self, ty: u8, depth: usize) -> Result<(), EpeeError> {
        if ty & FLAG_ARRAY != 0 {
            let inner = ty & !FLAG_ARRAY;
            if inner == TYPE_ARRAY {
                return Err(EpeeError::NestedArray);
            }
            let n = self.count(min_len(inner)?)?;
            for _ in 0..n {
                self.skip_one(inner, depth)?;
            }
            return Ok(());
        }
        self.skip_one(ty, depth)
    }

    fn skip_one(&mut self, ty: u8, depth: usize) -> Result<(), EpeeError> {
        match ty {
            TYPE_INT64 | TYPE_UINT64 | TYPE_DOUBLE => self.advance(8),
            TYPE_INT32 | TYPE_UINT32 => self.advance(4),
            TYPE_INT16 | TYPE_UINT16 => self.advance(2),
            TYPE_INT8 | TYPE_UINT8 => self.advance(1),
            TYPE_BOOL => self.bool().map(|_| ()),
            TYPE_STRING => {
                let len = self.count(1)?;
                self.advance(len)
            }
            TYPE_OBJECT => self.skip_section(depth + 1),
            // A bare "array" entry: a type byte of 13, then the array's own
            // typed header. It is how an array of arrays is written, so it is
            // refused; an array in a field is read by its 0x80 flag.
            TYPE_ARRAY => Err(EpeeError::NestedArray),
            other => Err(EpeeError::UnknownType(other)),
        }
    }

    fn skip_section(&mut self, depth: usize) -> Result<(), EpeeError> {
        if depth > MAX_DEPTH {
            return Err(EpeeError::TooDeep);
        }
        let n = self.count(4)?;
        for _ in 0..n {
            self.name()?;
            let ty = self.byte()?;
            self.skip(ty, depth)?;
        }
        Ok(())
    }
}

/// The fewest bytes one element of an array of `ty` can occupy.
const fn min_len(ty: u8) -> Result<usize, EpeeError> {
    Ok(match ty {
        TYPE_INT64 | TYPE_UINT64 | TYPE_DOUBLE => 8,
        TYPE_INT32 | TYPE_UINT32 => 4,
        TYPE_INT16 | TYPE_UINT16 => 2,
        TYPE_INT8 | TYPE_UINT8 | TYPE_BOOL | TYPE_STRING | TYPE_OBJECT => 1,
        TYPE_ARRAY => return Err(EpeeError::NestedArray),
        other => return Err(EpeeError::UnknownType(other)),
    })
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

    /// A document whose root holds `entries`, each already encoded as
    /// name length, name, type and value.
    fn doc(count: u64, entries: &[u8]) -> Vec<u8> {
        let mut b = HEADER.to_vec();
        put_varint(&mut b, count).unwrap();
        b.extend_from_slice(entries);
        b
    }

    fn entry(name: &[u8], ty: u8, value: &[u8]) -> Vec<u8> {
        let mut e = vec![name.len() as u8];
        e.extend_from_slice(name);
        e.push(ty);
        e.extend_from_slice(value);
        e
    }

    #[test]
    fn a_request_encodes_as_a_header_and_two_entries() {
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
    fn an_empty_array_is_left_out() {
        let bytes = encode(&[("unified_ids", Field::U64s(&[]))]).unwrap();
        assert_eq!(bytes, doc(0, &[]));
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
    fn only_the_wanted_keys_are_kept() {
        let bytes = encode(&[
            ("as_of_n_blocks", Field::U64(421)),
            ("unified_ids", Field::U64s(&[7, 8, 9])),
        ])
        .unwrap();
        let root = read_root(&bytes, &["as_of_n_blocks", "unified_ids"]).unwrap();
        assert_eq!(root.unsigned("as_of_n_blocks"), Some(421));
        assert_eq!(root.get("unified_ids"), Some(&Scalar::Container));

        let root = read_root(&bytes, &[]).unwrap();
        assert_eq!(root.get("as_of_n_blocks"), None);
    }

    /// Nested objects, an array of objects, strings holding raw bytes and a
    /// bool: every shape the path response uses, walked past on the way to
    /// the scalars that are kept.
    #[test]
    fn nested_sections_and_arrays_of_objects_are_walked_past() {
        let mut path = vec![2 << 2];
        path.extend(entry(b"leaf_idx", TYPE_UINT32, &5u32.to_le_bytes()));
        path.extend(entry(b"blob", TYPE_STRING, &[2 << 2, 0xff, 0x00]));
        let mut paths = vec![1 << 2];
        paths.extend(path);

        let mut body = entry(b"status", TYPE_STRING, &[2 << 2, b'O', b'K']);
        body.extend(entry(b"paths", TYPE_OBJECT | FLAG_ARRAY, &paths));
        body.extend(entry(b"untrusted", TYPE_BOOL, &[0]));
        body.extend(entry(b"n_leaf_tuples", TYPE_UINT64, &62u64.to_le_bytes()));

        let root = read_root(
            &doc(4, &body),
            &["status", "untrusted", "n_leaf_tuples", "paths"],
        )
        .unwrap();
        assert_eq!(root.text("status"), Some("OK"));
        assert_eq!(root.get("untrusted"), Some(&Scalar::Bool(false)));
        assert_eq!(root.unsigned("n_leaf_tuples"), Some(62));
        assert_eq!(root.get("paths"), Some(&Scalar::Container));
    }

    #[test]
    fn what_would_make_a_kept_value_wrong_is_refused() {
        // An array of arrays: nesting that recurses without a section, and so
        // without the depth check.
        let nested = doc(1, &entry(b"x", TYPE_ARRAY | FLAG_ARRAY, &[1 << 2]));
        assert_eq!(read_root(&nested, &[]), Err(EpeeError::NestedArray));
        // A bare array entry. See `skip_one`.
        let bare = doc(1, &entry(b"x", TYPE_ARRAY, &[TYPE_UINT8 | FLAG_ARRAY, 0]));
        assert_eq!(read_root(&bare, &[]), Err(EpeeError::NestedArray));

        // An empty name.
        let mut empty = HEADER.to_vec();
        // Padded so the entry count's size bound passes and the name is read.
        empty.extend_from_slice(&[1 << 2, 0, TYPE_UINT8, 1, 0]);
        assert_eq!(read_root(&empty, &[]), Err(EpeeError::EmptyName));

        // A bool that is neither 0 nor 1, kept or skipped.
        let bad = doc(1, &entry(b"b", TYPE_BOOL, &[2]));
        assert_eq!(read_root(&bad, &["b"]), Err(EpeeError::BadBool(2)));
        assert_eq!(read_root(&bad, &[]), Err(EpeeError::BadBool(2)));

        // A repeated root key.
        let mut dup = entry(b"a", TYPE_UINT8, &[1]);
        dup.extend(entry(b"a", TYPE_UINT8, &[2]));
        assert_eq!(
            read_root(&doc(2, &dup), &["a"]),
            Err(EpeeError::DuplicateKey("a".to_owned()))
        );

        // An unknown type byte.
        assert_eq!(
            read_root(&doc(1, &entry(b"z", 42, &[0])), &[]),
            Err(EpeeError::UnknownType(42))
        );
    }

    #[test]
    fn malformed_bodies_are_errors_not_panics() {
        assert_eq!(read_root(&[], &[]), Err(EpeeError::BadHeader));
        assert_eq!(read_root(&[0u8; 9], &[]), Err(EpeeError::BadHeader));

        // A count of a billion elements in a short body.
        let mut huge = vec![];
        huge.extend_from_slice(&((1_000_000_000u32 << 2) | 2).to_le_bytes());
        let huge = doc(1, &entry(b"x", TYPE_UINT64 | FLAG_ARRAY, &huge));
        assert!(matches!(
            read_root(&huge, &[]),
            Err(EpeeError::ImpossibleCount(_))
        ));

        // Truncated in the middle of a value, and bytes after the root.
        let good = encode(&[("n", Field::U64(1))]).unwrap();
        assert!(matches!(
            read_root(&good[..good.len() - 1], &["n"]),
            Err(EpeeError::Truncated(_))
        ));
        let mut long = good.clone();
        long.push(0);
        assert_eq!(read_root(&long, &["n"]), Err(EpeeError::TrailingBytes(1)));
    }

    /// Sections nested past the limit fail before the stack is at risk, and
    /// at the limit they are read.
    #[test]
    fn nesting_is_capped() {
        fn nested(levels: usize) -> Vec<u8> {
            // `levels` sections, each holding the next under the key "o", the
            // innermost empty. Written front to back, once, rather than by
            // wrapping a copy of the inner document at every level.
            let level = [1 << 2, 1, b'o', TYPE_OBJECT];
            let mut b = HEADER.to_vec();
            for _ in 0..levels {
                b.extend_from_slice(&level);
            }
            b.push(0);
            b
        }
        assert!(read_root(&nested(MAX_DEPTH), &[]).is_ok());
        assert_eq!(
            read_root(&nested(MAX_DEPTH + 1), &[]),
            Err(EpeeError::TooDeep)
        );
        assert_eq!(read_root(&nested(100_000), &[]), Err(EpeeError::TooDeep));
    }

    /// Wide bodies cost one pass and keep nothing: many root keys, and a long
    /// array none of which is kept.
    #[test]
    fn wide_bodies_cost_one_pass() {
        // 200,000 distinct root keys.
        let mut keys = Vec::new();
        for i in 0..200_000u32 {
            keys.extend(entry(format!("{i:x}").as_bytes(), TYPE_UINT8, &[0]));
        }
        let wide = doc(200_000, &keys);
        let started = std::time::Instant::now();
        assert!(read_root(&wide, &["n_leaf_tuples"]).is_ok());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));

        // Five million one-byte elements, none of them kept.
        let mut bytes = Vec::new();
        put_varint(&mut bytes, 5_000_000).unwrap();
        bytes.resize(bytes.len() + 5_000_000, 0);
        let long = doc(1, &entry(b"x", TYPE_UINT8 | FLAG_ARRAY, &bytes));
        assert_eq!(
            read_root(&long, &["x"]).unwrap().get("x"),
            Some(&Scalar::Container)
        );
    }
}
