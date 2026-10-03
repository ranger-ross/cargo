//! Versioned immutable inventories and local-only usage receipts.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use anyhow::{bail, ensure};

use crate::CargoResult;
use crate::util::data_structures::HashMap;

pub(super) type Digest = [u8; 32];
const UNIT_MAGIC: &[u8] = b"cargo-shared-blob-unit-result-v2\0";
const SNAPSHOT_MAGIC: &[u8] = b"cargo-shared-blob-snapshot-v1\0";
const STATE_MAGIC: &[u8] = b"cargo-shared-blob-state-v1\0";

#[cfg(unix)]
const PATH_ENCODING: u8 = 1;
#[cfg(windows)]
const PATH_ENCODING: u8 = 2;
#[cfg(not(any(unix, windows)))]
const PATH_ENCODING: u8 = 3;

#[derive(Debug)]
pub(super) struct Output {
    pub path: Vec<u8>,
    pub hash: Digest,
    pub size: u64,
}

#[derive(Debug)]
pub(super) struct UnitResult {
    pub id: Digest,
    /// Local freshness token, excluded from the immutable inventory.
    pub generation: Digest,
    pub bytes: Vec<u8>,
}

impl UnitResult {
    pub fn new(mut outputs: Vec<Output>) -> Self {
        outputs.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        let capacity = UNIT_MAGIC.len()
            + 9
            + outputs
                .iter()
                .map(|output| 48 + output.path.len())
                .sum::<usize>();
        let mut bytes = Vec::with_capacity(capacity);
        bytes.extend_from_slice(UNIT_MAGIC);
        bytes.push(PATH_ENCODING);
        put_u64(&mut bytes, outputs.len() as u64);
        for output in outputs {
            put_bytes(&mut bytes, &output.path);
            bytes.extend_from_slice(&output.hash);
            put_u64(&mut bytes, output.size);
        }
        Self {
            id: *blake3::hash(&bytes).as_bytes(),
            generation: [0; 32],
            bytes,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct StoredUnit {
    pub result: Digest,
    pub generation: Digest,
}

#[derive(Default, Debug, Eq, PartialEq)]
pub(super) struct Receipt {
    pub units: HashMap<Vec<u8>, StoredUnit>,
    pub usage: BTreeMap<Digest, u64>,
}

/// Encode components explicitly; native OsStr encoding is not a wire format.
pub(super) fn encode_output_path(path: &Path) -> CargoResult<Vec<u8>> {
    let mut bytes = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            bail!(
                "shared blob output path must be relative: {}",
                path.display()
            );
        };
        if !bytes.is_empty() {
            bytes.push(b'/');
            #[cfg(windows)]
            bytes.push(0);
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            bytes.extend_from_slice(component.as_bytes());
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            for unit in component.encode_wide() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
        }
        #[cfg(not(any(unix, windows)))]
        bytes.extend_from_slice(
            component
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 output path"))?
                .as_bytes(),
        );
    }
    validate_path(PATH_ENCODING, &bytes)?;
    Ok(bytes)
}

pub(super) fn decode_unit(bytes: &[u8], id: &Digest) -> CargoResult<Vec<Output>> {
    verify_digest(bytes, id)?;
    let mut reader = Reader::new(bytes, UNIT_MAGIC)?;
    let encoding = reader.take(1)?[0];
    ensure!((1..=3).contains(&encoding), "unknown output path encoding");
    let count = reader.count(48)?;
    let mut outputs: Vec<Output> = Vec::with_capacity(count);
    for _ in 0..count {
        let path = reader.bytes()?;
        validate_path(encoding, path)?;
        ensure!(
            outputs
                .last()
                .is_none_or(|last| last.path.as_slice() < path),
            "unordered or duplicate output path"
        );
        outputs.push(Output {
            path: path.to_vec(),
            hash: reader.digest()?,
            size: reader.u64()?,
        });
    }
    reader.finish()?;
    Ok(outputs)
}

/// Sorting and hashing avoid serializing an already-published graph.
pub(super) fn snapshot_id(results: &mut Vec<Digest>) -> Digest {
    results.sort_unstable();
    results.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(SNAPSHOT_MAGIC);
    hasher.update(&(results.len() as u64).to_le_bytes());
    for result in results {
        hasher.update(result);
    }
    *hasher.finalize().as_bytes()
}

pub(super) fn encode_snapshot(results: &[Digest]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(SNAPSHOT_MAGIC.len() + 8 + results.len() * 32);
    bytes.extend_from_slice(SNAPSHOT_MAGIC);
    put_u64(&mut bytes, results.len() as u64);
    for result in results {
        bytes.extend_from_slice(result);
    }
    bytes
}

pub(super) fn decode_snapshot(bytes: &[u8], id: &Digest) -> CargoResult<Vec<Digest>> {
    verify_digest(bytes, id)?;
    let mut reader = Reader::new(bytes, SNAPSHOT_MAGIC)?;
    let count = reader.count(32)?;
    let mut results = Vec::with_capacity(count);
    for _ in 0..count {
        let result = reader.digest()?;
        ensure!(
            results.last().is_none_or(|last| last < &result),
            "unordered or duplicate snapshot member"
        );
        results.push(result);
    }
    reader.finish()?;
    Ok(results)
}

impl Receipt {
    pub fn encode(&self) -> Vec<u8> {
        let capacity = STATE_MAGIC.len()
            + 16
            + self.usage.len() * 40
            + self.units.keys().map(|path| 72 + path.len()).sum::<usize>();
        let mut bytes = Vec::with_capacity(capacity);
        bytes.extend_from_slice(STATE_MAGIC);
        put_u64(&mut bytes, self.units.len() as u64);
        let mut units: Vec<_> = self.units.iter().collect();
        units.sort_unstable_by(|a, b| a.0.cmp(b.0));
        for (path, unit) in units {
            put_bytes(&mut bytes, path);
            bytes.extend_from_slice(&unit.result);
            bytes.extend_from_slice(&unit.generation);
        }
        put_u64(&mut bytes, self.usage.len() as u64);
        for (snapshot, last_used) in &self.usage {
            bytes.extend_from_slice(snapshot);
            put_u64(&mut bytes, *last_used);
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> CargoResult<Self> {
        let mut reader = Reader::new(bytes, STATE_MAGIC)?;
        let count = reader.count(72)?;
        let mut receipt = Self::default();
        receipt.units.reserve(count);
        let mut previous: Option<&[u8]> = None;
        for _ in 0..count {
            let path = reader.bytes()?;
            ensure!(
                previous.is_none_or(|last| last < path),
                "unordered or duplicate local unit slot"
            );
            previous = Some(path);
            let unit = StoredUnit {
                result: reader.digest()?,
                generation: reader.digest()?,
            };
            receipt.units.insert(path.to_vec(), unit);
        }
        let count = reader.count(40)?;
        for _ in 0..count {
            let snapshot = reader.digest()?;
            ensure!(
                receipt
                    .usage
                    .last_key_value()
                    .is_none_or(|(last, _)| last < &snapshot),
                "unordered or duplicate snapshot usage"
            );
            receipt.usage.insert(snapshot, reader.u64()?);
        }
        reader.finish()?;
        Ok(receipt)
    }
}

pub(super) fn digest_filename(name: &str) -> Option<Digest> {
    if name.len() != 64 {
        return None;
    }
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }
    let mut digest = [0; 32];
    for (dest, pair) in digest.iter_mut().zip(name.as_bytes().chunks_exact(2)) {
        *dest = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(digest)
}

fn verify_digest(bytes: &[u8], id: &Digest) -> CargoResult<()> {
    ensure!(
        blake3::hash(bytes).as_bytes() == id,
        "shared blob manifest digest mismatch"
    );
    Ok(())
}

fn validate_path(encoding: u8, bytes: &[u8]) -> CargoResult<()> {
    if encoding == 2 {
        ensure!(bytes.len() % 2 == 0, "odd UTF-16 output path length");
        let mut component_len = 0;
        let mut dots_only = true;
        for pair in bytes.chunks_exact(2) {
            let unit = u16::from_le_bytes([pair[0], pair[1]]);
            ensure!(
                unit != 0 && unit != u16::from(b'\\') && unit != u16::from(b':'),
                "invalid UTF-16 output path"
            );
            if unit == u16::from(b'/') {
                ensure!(
                    component_len > 0 && !(dots_only && component_len <= 2),
                    "non-relative output path"
                );
                component_len = 0;
                dots_only = true;
            } else {
                component_len += 1;
                dots_only &= unit == u16::from(b'.');
            }
        }
        ensure!(
            component_len > 0 && !(dots_only && component_len <= 2),
            "non-relative output path"
        );
    } else {
        if encoding == 3 {
            std::str::from_utf8(bytes)?;
        }
        ensure!(!bytes.contains(&0), "NUL in output path");
        for part in bytes.split(|byte| *byte == b'/') {
            ensure!(
                !part.is_empty() && part != b"." && part != b"..",
                "non-relative output path"
            );
        }
    }
    Ok(())
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(bytes: &mut Vec<u8>, value: &[u8]) {
    put_u64(bytes, value.len() as u64);
    bytes.extend_from_slice(value);
}

struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], magic: &[u8]) -> CargoResult<Self> {
        let mut reader = Self { remaining: bytes };
        ensure!(
            reader.take(magic.len())? == magic,
            "unknown shared blob metadata format"
        );
        Ok(reader)
    }

    fn take(&mut self, len: usize) -> CargoResult<&'a [u8]> {
        ensure!(
            len <= self.remaining.len(),
            "truncated shared blob metadata"
        );
        let (value, rest) = self.remaining.split_at(len);
        self.remaining = rest;
        Ok(value)
    }

    fn u64(&mut self) -> CargoResult<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn digest(&mut self) -> CargoResult<Digest> {
        Ok(self.take(32)?.try_into().unwrap())
    }

    fn count(&mut self, minimum_size: usize) -> CargoResult<usize> {
        let count = usize::try_from(self.u64()?)?;
        ensure!(
            count <= self.remaining.len() / minimum_size,
            "shared blob metadata count exceeds available bytes"
        );
        Ok(count)
    }

    fn bytes(&mut self) -> CargoResult<&'a [u8]> {
        let len = usize::try_from(self.u64()?)?;
        self.take(len)
    }

    fn finish(self) -> CargoResult<()> {
        ensure!(
            self.remaining.is_empty(),
            "trailing shared blob metadata bytes"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(paths: &[&[u8]]) -> UnitResult {
        UnitResult::new(
            paths
                .iter()
                .map(|path| Output {
                    path: path.to_vec(),
                    hash: [1; 32],
                    size: u64::MAX,
                })
                .collect(),
        )
    }

    #[test]
    fn immutable_identity_and_full_width_sizes() {
        let path_a = encode_output_path(Path::new("a")).unwrap();
        let path_b = encode_output_path(Path::new("b")).unwrap();
        let first = unit(&[&path_a, &path_b]);
        assert_eq!(first.id, unit(&[&path_b, &path_a]).id);
        assert_eq!(
            decode_unit(&first.bytes, &first.id).unwrap()[0].size,
            u64::MAX
        );
        let mut members = vec![first.id, [2; 32], first.id];
        let id = snapshot_id(&mut members);
        assert_eq!(
            decode_snapshot(&encode_snapshot(&members), &id).unwrap(),
            members
        );
        assert_ne!(unit(&[]).id, snapshot_id(&mut vec![]));
    }

    #[test]
    fn rejects_truncation_digest_mismatch_and_trailing_data() {
        let path = encode_output_path(Path::new("a")).unwrap();
        let result = unit(&[&path]);
        for end in 0..result.bytes.len() {
            let bytes = &result.bytes[..end];
            assert!(decode_unit(bytes, blake3::hash(bytes).as_bytes()).is_err());
        }
        assert!(decode_unit(&result.bytes, &[0; 32]).is_err());
        let mut bytes = result.bytes;
        bytes.push(0);
        assert!(decode_unit(&bytes, blake3::hash(&bytes).as_bytes()).is_err());
        let receipt = Receipt::default().encode();
        for end in 0..receipt.len() {
            assert!(Receipt::decode(&receipt[..end]).is_err());
        }
        let mut snapshot = encode_snapshot(&[[1; 32]]);
        for end in 0..snapshot.len() {
            let bytes = &snapshot[..end];
            assert!(decode_snapshot(bytes, blake3::hash(bytes).as_bytes()).is_err());
        }
        snapshot.push(0);
        assert!(decode_snapshot(&snapshot, blake3::hash(&snapshot).as_bytes()).is_err());
        let mut receipt = receipt;
        receipt.push(0);
        assert!(Receipt::decode(&receipt).is_err());
    }

    #[test]
    fn rejects_unbounded_counts_and_noncanonical_order() {
        let mut bytes = encode_snapshot(&[]);
        bytes[SNAPSHOT_MAGIC.len()..].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_snapshot(&bytes, blake3::hash(&bytes).as_bytes()).is_err());
        for members in [vec![[1; 32], [1; 32]], vec![[2; 32], [1; 32]]] {
            let bytes = encode_snapshot(&members);
            assert!(decode_snapshot(&bytes, blake3::hash(&bytes).as_bytes()).is_err());
        }
        let path = encode_output_path(Path::new("a")).unwrap();
        let duplicate = unit(&[&path, &path]);
        assert!(decode_unit(&duplicate.bytes, &duplicate.id).is_err());
    }

    #[test]
    fn rejects_unknown_encoding_huge_paths_and_unordered_outputs() {
        let path = encode_output_path(Path::new("a")).unwrap();
        let canonical = unit(&[&path]).bytes;
        let mut unknown = canonical.clone();
        unknown[UNIT_MAGIC.len()] = 4;
        assert!(decode_unit(&unknown, blake3::hash(&unknown).as_bytes()).is_err());
        for offset in [UNIT_MAGIC.len() + 1, UNIT_MAGIC.len() + 9] {
            let mut bytes = canonical.clone();
            bytes[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
            assert!(decode_unit(&bytes, blake3::hash(&bytes).as_bytes()).is_err());
        }
        let mut bytes = UNIT_MAGIC.to_vec();
        bytes.push(1);
        put_u64(&mut bytes, 2);
        for path in [b"b", b"a"] {
            put_bytes(&mut bytes, path);
            bytes.extend_from_slice(&[1; 32]);
            put_u64(&mut bytes, 1);
        }
        assert!(decode_unit(&bytes, blake3::hash(&bytes).as_bytes()).is_err());
        let mut receipt = Receipt::default().encode();
        receipt[STATE_MAGIC.len()..STATE_MAGIC.len() + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(Receipt::decode(&receipt).is_err());
    }

    #[test]
    fn receipt_roundtrip_and_duplicate_rejection() {
        let mut receipt = Receipt::default();
        receipt.units.insert(
            b"unit".to_vec(),
            StoredUnit {
                result: [1; 32],
                generation: [2; 32],
            },
        );
        receipt.usage.insert([3; 32], u64::MAX);
        assert_eq!(Receipt::decode(&receipt.encode()).unwrap(), receipt);
        let mut bytes = Receipt::default().encode();
        let usage_offset = STATE_MAGIC.len() + 8;
        bytes[usage_offset..].copy_from_slice(&2_u64.to_le_bytes());
        for _ in 0..2 {
            bytes.extend_from_slice(&[3; 32]);
            put_u64(&mut bytes, 0);
        }
        assert!(Receipt::decode(&bytes).is_err());
    }

    #[test]
    fn accepts_explicit_cross_platform_paths_but_not_escape_paths() {
        for (tag, path) in [
            (1, b"a/\xff".as_slice()),
            (2, b"a\0/\0b\0".as_slice()),
            (3, b"a/b".as_slice()),
        ] {
            let mut bytes = UNIT_MAGIC.to_vec();
            bytes.push(tag);
            put_u64(&mut bytes, 1);
            put_bytes(&mut bytes, path);
            bytes.extend_from_slice(&[1; 32]);
            put_u64(&mut bytes, 0);
            assert_eq!(
                decode_unit(&bytes, blake3::hash(&bytes).as_bytes()).unwrap()[0].path,
                path
            );
        }
        for path in [b"".as_slice(), b"/a", b"a/../b", b"a//b", b"a\0"] {
            assert!(validate_path(1, path).is_err());
        }
        assert!(validate_path(2, b"a").is_err());
        assert!(validate_path(3, b"\xff").is_err());
    }
}
