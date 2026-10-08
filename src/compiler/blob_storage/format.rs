//! Versioned immutable unit outputs and snapshots, plus mutable cache entries.

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, ensure};

use crate::CargoResult;

pub(super) type Digest = [u8; 32];
const UNIT_MAGIC: &[u8] = b"cargo-shared-storage-unit-output-v1\0";
const SNAPSHOT_MAGIC: &[u8] = b"cargo-shared-storage-snapshot-v2\0";
const CACHE_MAGIC: &[u8] = b"cargo-shared-storage-cache-entry-v2\0";

#[cfg(unix)]
const PATH_ENCODING: u8 = 1;
#[cfg(windows)]
const PATH_ENCODING: u8 = 2;
#[cfg(not(any(unix, windows)))]
const PATH_ENCODING: u8 = 3;

/// One captured file: its path relative to the unit directory and its blob.
#[derive(Debug)]
pub(in crate::compiler) struct Output {
    pub path: Vec<u8>,
    pub hash: Digest,
    pub size: u64,
}

#[derive(Debug)]
pub(super) struct UnitOutput {
    pub id: Digest,
    pub bytes: Vec<u8>,
}

impl UnitOutput {
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
            bytes,
        }
    }
}

/// A cache entry duplicates the unit-output paths and hashes so a lookup needs
/// a single read. It also stores the metadata used to restore each file.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct CachedOutput {
    pub path: Vec<u8>,
    pub hash: Digest,
    pub size: u64,
    pub mode: u32,
    pub mtime_seconds: i64,
    pub mtime_nanos: u32,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct CacheEntry {
    /// Input guard covering inputs that the unit hash does not.
    pub fingerprint: u64,
    /// Outputs in canonical path order.
    pub outputs: Vec<CachedOutput>,
}

/// Unit-output and cache-entry members of one successful build.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct Snapshot {
    pub unit_outputs: Vec<Digest>,
    pub cache_entries: Vec<String>,
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

/// Decode only native paths for restoration; unit-output traversal is portable.
pub(super) fn decode_output_path(bytes: &[u8]) -> CargoResult<PathBuf> {
    validate_path(PATH_ENCODING, bytes)?;
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    };
    #[cfg(windows)]
    let path = {
        use std::os::windows::ffi::OsStringExt;
        let units: Vec<_> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        PathBuf::from(std::ffi::OsString::from_wide(&units))
    };
    #[cfg(not(any(unix, windows)))]
    let path = PathBuf::from(std::str::from_utf8(bytes)?);
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "non-relative output path"
    );
    Ok(path)
}

pub(super) fn decode_native_unit(bytes: &[u8], id: &Digest) -> CargoResult<Vec<Output>> {
    let outputs = decode_unit(bytes, id)?;
    ensure!(
        bytes[UNIT_MAGIC.len()] == PATH_ENCODING,
        "foreign output path encoding"
    );
    Ok(outputs)
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

impl Snapshot {
    /// Sorting makes identical member sets share an identity.
    pub fn new(mut unit_outputs: Vec<Digest>, mut cache_entries: Vec<String>) -> Self {
        unit_outputs.sort_unstable();
        unit_outputs.dedup();
        cache_entries.sort_unstable();
        cache_entries.dedup();
        Self {
            unit_outputs,
            cache_entries,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let capacity = SNAPSHOT_MAGIC.len()
            + 16
            + self.unit_outputs.len() * 32
            + self
                .cache_entries
                .iter()
                .map(|name| 8 + name.len())
                .sum::<usize>();
        let mut bytes = Vec::with_capacity(capacity);
        bytes.extend_from_slice(SNAPSHOT_MAGIC);
        put_u64(&mut bytes, self.unit_outputs.len() as u64);
        for id in &self.unit_outputs {
            bytes.extend_from_slice(id);
        }
        put_u64(&mut bytes, self.cache_entries.len() as u64);
        for name in &self.cache_entries {
            put_bytes(&mut bytes, name.as_bytes());
        }
        bytes
    }

    pub fn decode(bytes: &[u8], id: &Digest) -> CargoResult<Self> {
        verify_digest(bytes, id)?;
        let mut reader = Reader::new(bytes, SNAPSHOT_MAGIC)?;
        let count = reader.count(32)?;
        let mut unit_outputs: Vec<Digest> = Vec::with_capacity(count);
        for _ in 0..count {
            let id = reader.digest()?;
            ensure!(
                unit_outputs.last().is_none_or(|last| last < &id),
                "unordered or duplicate snapshot unit output"
            );
            unit_outputs.push(id);
        }
        let count = reader.count(9)?;
        let mut cache_entries: Vec<String> = Vec::with_capacity(count);
        for _ in 0..count {
            let name = std::str::from_utf8(reader.bytes()?)?;
            ensure!(valid_unit_hash(name), "invalid snapshot cache entry");
            ensure!(
                cache_entries.last().is_none_or(|last| last.as_str() < name),
                "unordered or duplicate snapshot cache entry"
            );
            cache_entries.push(name.to_owned());
        }
        reader.finish()?;
        Ok(Self {
            unit_outputs,
            cache_entries,
        })
    }
}

impl CacheEntry {
    pub fn new(fingerprint: u64, mut outputs: Vec<CachedOutput>) -> Self {
        outputs.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        Self {
            fingerprint,
            outputs,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let capacity = CACHE_MAGIC.len()
            + 17
            + self
                .outputs
                .iter()
                .map(|output| 64 + output.path.len())
                .sum::<usize>();
        let mut bytes = Vec::with_capacity(capacity);
        bytes.extend_from_slice(CACHE_MAGIC);
        bytes.push(PATH_ENCODING);
        put_u64(&mut bytes, self.fingerprint);
        put_u64(&mut bytes, self.outputs.len() as u64);
        for output in &self.outputs {
            put_bytes(&mut bytes, &output.path);
            bytes.extend_from_slice(&output.hash);
            put_u64(&mut bytes, output.size);
            bytes.extend_from_slice(&output.mode.to_le_bytes());
            bytes.extend_from_slice(&output.mtime_seconds.to_le_bytes());
            bytes.extend_from_slice(&output.mtime_nanos.to_le_bytes());
        }
        bytes
    }

    /// Decode any path encoding. Restoration must use `decode_native`.
    pub fn decode(bytes: &[u8]) -> CargoResult<Self> {
        let mut reader = Reader::new(bytes, CACHE_MAGIC)?;
        let encoding = reader.take(1)?[0];
        ensure!((1..=3).contains(&encoding), "unknown output path encoding");
        let fingerprint = reader.u64()?;
        let count = reader.count(64)?;
        let mut outputs: Vec<CachedOutput> = Vec::with_capacity(count);
        for _ in 0..count {
            let path = reader.bytes()?;
            validate_path(encoding, path)?;
            ensure!(
                outputs
                    .last()
                    .is_none_or(|last| last.path.as_slice() < path),
                "unordered or duplicate output path"
            );
            let hash = reader.digest()?;
            let size = reader.u64()?;
            let mode = reader.u32()?;
            let mtime_seconds = i64::from_le_bytes(reader.take(8)?.try_into().unwrap());
            let mtime_nanos = reader.u32()?;
            ensure!(mode & !0o777 == 0, "invalid cached output permissions");
            ensure!(
                mtime_nanos < 1_000_000_000,
                "invalid cached output timestamp"
            );
            outputs.push(CachedOutput {
                path: path.to_vec(),
                hash,
                size,
                mode,
                mtime_seconds,
                mtime_nanos,
            });
        }
        reader.finish()?;
        Ok(Self {
            fingerprint,
            outputs,
        })
    }

    pub fn decode_native(bytes: &[u8]) -> CargoResult<Self> {
        let entry = Self::decode(bytes)?;
        ensure!(
            bytes[CACHE_MAGIC.len()] == PATH_ENCODING,
            "foreign output path encoding"
        );
        Ok(entry)
    }
}

/// Cache entries are named by Cargo's lowercase hexadecimal unit hash.
pub(super) fn valid_unit_hash(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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

    fn u32(&mut self) -> CargoResult<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
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

    fn unit(paths: &[&[u8]]) -> UnitOutput {
        UnitOutput::new(
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
        let snapshot = Snapshot::new(
            vec![first.id, [2; 32], first.id],
            vec!["b".to_owned(), "a".to_owned(), "b".to_owned()],
        );
        let reordered = Snapshot::new(
            vec![[2; 32], first.id],
            vec!["a".to_owned(), "b".to_owned()],
        );
        let bytes = snapshot.encode();
        assert_eq!(bytes, reordered.encode());
        assert_eq!(
            Snapshot::decode(&bytes, blake3::hash(&bytes).as_bytes()).unwrap(),
            snapshot
        );
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
        let mut snapshot = Snapshot::new(vec![[1; 32]], vec!["ab".to_owned()]).encode();
        for end in 0..snapshot.len() {
            let bytes = &snapshot[..end];
            assert!(Snapshot::decode(bytes, blake3::hash(bytes).as_bytes()).is_err());
        }
        snapshot.push(0);
        assert!(Snapshot::decode(&snapshot, blake3::hash(&snapshot).as_bytes()).is_err());
    }

    #[test]
    fn rejects_unbounded_counts_and_noncanonical_order() {
        let mut bytes = Snapshot::new(Vec::new(), Vec::new()).encode();
        bytes[SNAPSHOT_MAGIC.len()..SNAPSHOT_MAGIC.len() + 8]
            .copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(Snapshot::decode(&bytes, blake3::hash(&bytes).as_bytes()).is_err());
        for (unit_outputs, cache_entries) in [
            (vec![[1; 32], [1; 32]], vec![]),
            (vec![[2; 32], [1; 32]], vec![]),
            (vec![], vec!["b", "a"]),
            (vec![], vec!["a", "a"]),
            (vec![], vec!["../a"]),
        ] {
            let snapshot = Snapshot {
                unit_outputs,
                cache_entries: cache_entries.into_iter().map(str::to_owned).collect(),
            };
            let bytes = snapshot.encode();
            assert!(Snapshot::decode(&bytes, blake3::hash(&bytes).as_bytes()).is_err());
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
    }

    fn cached(path: &str) -> CachedOutput {
        CachedOutput {
            path: encode_output_path(Path::new(path)).unwrap(),
            hash: [3; 32],
            size: u64::MAX,
            mode: 0o755,
            mtime_seconds: -1,
            mtime_nanos: 123,
        }
    }

    #[test]
    fn cache_entry_roundtrip_and_rejects_truncation() {
        let entry = CacheEntry::new(u64::MAX, vec![cached("out/b"), cached("out/a")]);
        assert_eq!(entry.outputs[0].path, cached("out/a").path);
        let bytes = entry.encode();
        assert_eq!(CacheEntry::decode(&bytes).unwrap(), entry);
        assert_eq!(CacheEntry::decode_native(&bytes).unwrap(), entry);
        for end in 0..bytes.len() {
            assert!(CacheEntry::decode(&bytes[..end]).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(CacheEntry::decode(&trailing).is_err());
        let duplicate = CacheEntry {
            fingerprint: 0,
            outputs: vec![cached("out/a"), cached("out/a")],
        };
        assert!(CacheEntry::decode(&duplicate.encode()).is_err());
    }

    #[test]
    fn foreign_cache_entries_decode_for_collection_but_not_restoration() {
        let mut bytes = CACHE_MAGIC.to_vec();
        bytes.push(if PATH_ENCODING == 3 { 1 } else { 3 });
        put_u64(&mut bytes, 7);
        put_u64(&mut bytes, 1);
        put_bytes(&mut bytes, b"out/a");
        bytes.extend_from_slice(&[1; 32]);
        put_u64(&mut bytes, 0);
        bytes.extend_from_slice(&0o644_u32.to_le_bytes());
        bytes.extend_from_slice(&0_i64.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        assert_eq!(CacheEntry::decode(&bytes).unwrap().fingerprint, 7);
        assert!(CacheEntry::decode_native(&bytes).is_err());
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
