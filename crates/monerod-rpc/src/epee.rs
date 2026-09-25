//! monerod's binary format, epee "portable storage", for the `.bin`
//! endpoints.
//!
//! One endpoint needs it: `/get_path_by_unified_id.bin`, which answers two
//! questions. It is the cheapest place the FCMP++ daemon reports how many
//! outputs its curve tree held as of a block, and it is the only place it
//! gives out outputs' paths through that tree. (`/getblocks.bin` reports the
//! size too, when asked to start a tree sync, beside a batch of whole blocks.)
//! Every other call this crate makes is JSON, so this module covers what those
//! exchanges need: an encoder for a flat section of unsigned integers, and a
//! reader that keeps the named entries of the answer's root, whole, and walks
//! past everything else.
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
//! skips; the one thing it holds per entry is a borrowed name for each key of
//! a section it keeps, to catch a repeated one. It does one pass over the
//! bytes, so its time grows only linearly, and it caps nesting well inside any
//! stack. What it keeps can take more memory than the body -- up to
//! `size_of::<Value>()` bytes per byte, for an array of one-byte integers --
//! so callers cap the body, e.g.
//! [`crate::types::TreeSizeQuery::MAX_ANSWER_BYTES`].
//!
//! What it refuses: an empty name, a bool other than 0 or 1, a repeated key
//! in the root or in a section it keeps, any array of arrays, and bytes after
//! the root. Inside a value it skips it checks framing only -- types, counts
//! and lengths -- since nothing is kept from it.

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
    #[error("the key {0:?} appears twice in one section")]
    DuplicateKey(String),
    #[error("sections nested deeper than {MAX_DEPTH}")]
    TooDeep,
    #[error("{0} bytes follow the root section")]
    TrailingBytes(usize),
    #[error("a value does not fit the encoding: {0}")]
    Unencodable(&'static str),
}

/// A value kept from the answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Signed(i64),
    Unsigned(u64),
    Double(f64),
    Bool(bool),
    /// epee strings are byte strings, and monerod puts raw binary in them.
    Bytes(Vec<u8>),
    /// A section, with every entry kept.
    Section(Root),
    /// An array's elements, each kept.
    Array(Vec<Value>),
}

/// A section's entries, as kept: for the answer's root, the ones a caller
/// asked for; for a section inside a kept value, all of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Root(Vec<(String, Value)>);

impl Root {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// An unsigned integer of any width, or a signed one that is not negative.
    #[must_use]
    pub fn unsigned(&self, name: &str) -> Option<u64> {
        match self.get(name)? {
            Value::Unsigned(v) => Some(*v),
            Value::Signed(v) => u64::try_from(*v).ok(),
            _ => None,
        }
    }

    /// A string that is valid UTF-8.
    #[must_use]
    pub fn text(&self, name: &str) -> Option<&str> {
        std::str::from_utf8(self.bytes(name)?).ok()
    }

    /// A string's raw bytes.
    #[must_use]
    pub fn bytes(&self, name: &str) -> Option<&[u8]> {
        match self.get(name)? {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    #[must_use]
    pub fn section(&self, name: &str) -> Option<&Root> {
        match self.get(name)? {
            Value::Section(s) => Some(s),
            _ => None,
        }
    }

    #[must_use]
    pub fn array(&self, name: &str) -> Option<&[Value]> {
        match self.get(name)? {
            Value::Array(a) => Some(a),
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
            Some(key) => kept.push((key.to_owned(), r.keep(ty, 0)?)),
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

    /// Read one value of type `ty`, inside `depth` enclosing sections, and
    /// keep it whole.
    ///
    /// Recursion is bounded the way [`Self::skip`]'s is.
    fn keep(&mut self, ty: u8, depth: usize) -> Result<Value, EpeeError> {
        if ty & FLAG_ARRAY != 0 {
            let inner = ty & !FLAG_ARRAY;
            if inner == TYPE_ARRAY {
                return Err(EpeeError::NestedArray);
            }
            let n = self.count(min_len(inner)?)?;
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                out.push(self.keep_one(inner, depth)?);
            }
            return Ok(Value::Array(out));
        }
        self.keep_one(ty, depth)
    }

    fn keep_one(&mut self, ty: u8, depth: usize) -> Result<Value, EpeeError> {
        Ok(match ty {
            TYPE_INT64 => Value::Signed(i64::from_le_bytes(self.array()?)),
            TYPE_INT32 => Value::Signed(i64::from(i32::from_le_bytes(self.array()?))),
            TYPE_INT16 => Value::Signed(i64::from(i16::from_le_bytes(self.array()?))),
            TYPE_INT8 => Value::Signed(i64::from(i8::from_le_bytes(self.array()?))),
            TYPE_UINT64 => Value::Unsigned(u64::from_le_bytes(self.array()?)),
            TYPE_UINT32 => Value::Unsigned(u64::from(u32::from_le_bytes(self.array()?))),
            TYPE_UINT16 => Value::Unsigned(u64::from(u16::from_le_bytes(self.array()?))),
            TYPE_UINT8 => Value::Unsigned(u64::from(self.byte()?)),
            TYPE_DOUBLE => Value::Double(f64::from_le_bytes(self.array()?)),
            TYPE_BOOL => Value::Bool(self.bool()?),
            TYPE_STRING => {
                let len = self.count(1)?;
                Value::Bytes(self.take(len)?.to_vec())
            }
            TYPE_OBJECT => Value::Section(self.keep_section(depth + 1)?),
            // See `skip_one`.
            TYPE_ARRAY => return Err(EpeeError::NestedArray),
            other => return Err(EpeeError::UnknownType(other)),
        })
    }

    fn keep_section(&mut self, depth: usize) -> Result<Root, EpeeError> {
        if depth > MAX_DEPTH {
            return Err(EpeeError::TooDeep);
        }
        let n = self.count(4)?;
        let mut seen: HashSet<&[u8]> = HashSet::with_capacity(n);
        let mut kept = Vec::with_capacity(n);
        for _ in 0..n {
            let name = self.name()?;
            if !seen.insert(name) {
                return Err(EpeeError::DuplicateKey(
                    String::from_utf8_lossy(name).into_owned(),
                ));
            }
            let ty = self.byte()?;
            kept.push((
                String::from_utf8_lossy(name).into_owned(),
                self.keep(ty, depth)?,
            ));
        }
        Ok(Root(kept))
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
        let root = read_root(&bytes, &["as_of_n_blocks"]).unwrap();
        assert_eq!(root.unsigned("as_of_n_blocks"), Some(421));
        assert_eq!(root.get("unified_ids"), None);

        let root = read_root(&bytes, &[]).unwrap();
        assert_eq!(root.get("as_of_n_blocks"), None);
    }

    #[test]
    fn a_wanted_array_is_kept_whole() {
        let bytes = encode(&[("unified_ids", Field::U64s(&[7, 8, 9]))]).unwrap();
        let root = read_root(&bytes, &["unified_ids"]).unwrap();
        assert_eq!(
            root.array("unified_ids"),
            Some(&[Value::Unsigned(7), Value::Unsigned(8), Value::Unsigned(9)][..])
        );
    }

    /// Nested objects, an array of objects, strings holding raw bytes and a
    /// bool: every shape the path response uses, walked past on the way to
    /// the scalars that are kept, or kept whole when asked for.
    #[test]
    fn nested_sections_and_arrays_of_objects_are_walked_past_or_kept() {
        let mut path = vec![2 << 2];
        path.extend(entry(b"leaf_idx", TYPE_UINT32, &5u32.to_le_bytes()));
        path.extend(entry(b"blob", TYPE_STRING, &[2 << 2, 0xff, 0x00]));
        let mut paths = vec![1 << 2];
        paths.extend(path);

        let mut body = entry(b"status", TYPE_STRING, &[2 << 2, b'O', b'K']);
        body.extend(entry(b"paths", TYPE_OBJECT | FLAG_ARRAY, &paths));
        body.extend(entry(b"untrusted", TYPE_BOOL, &[0]));
        body.extend(entry(b"n_leaf_tuples", TYPE_UINT64, &62u64.to_le_bytes()));

        let root = read_root(&doc(4, &body), &["status", "untrusted", "n_leaf_tuples"]).unwrap();
        assert_eq!(root.text("status"), Some("OK"));
        assert_eq!(root.get("untrusted"), Some(&Value::Bool(false)));
        assert_eq!(root.unsigned("n_leaf_tuples"), Some(62));
        assert_eq!(root.get("paths"), None);

        let root = read_root(&doc(4, &body), &["paths"]).unwrap();
        let [Value::Section(path)] = root.array("paths").unwrap() else {
            panic!("one path")
        };
        assert_eq!(path.unsigned("leaf_idx"), Some(5));
        assert_eq!(path.bytes("blob"), Some(&[0xff, 0x00][..]));
        assert_eq!(root.get("status"), None);
    }

    #[test]
    fn a_repeated_key_inside_a_kept_section_is_refused() {
        let mut inner = vec![2 << 2];
        inner.extend(entry(b"a", TYPE_UINT8, &[1]));
        inner.extend(entry(b"a", TYPE_UINT8, &[2]));
        let body = doc(1, &entry(b"s", TYPE_OBJECT, &inner));
        assert_eq!(
            read_root(&body, &["s"]),
            Err(EpeeError::DuplicateKey("a".to_owned()))
        );
        // Walked past, it is framing only.
        assert!(read_root(&body, &[]).is_ok());
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
        assert!(read_root(&nested(MAX_DEPTH), &["o"]).is_ok());
        assert_eq!(
            read_root(&nested(MAX_DEPTH + 1), &[]),
            Err(EpeeError::TooDeep)
        );
        assert_eq!(
            read_root(&nested(MAX_DEPTH + 1), &["o"]),
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
        assert_eq!(read_root(&long, &[]).unwrap().get("x"), None);
    }
}
