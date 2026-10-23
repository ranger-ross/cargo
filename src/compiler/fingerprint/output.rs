use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, bail};
use filetime::FileTime;
use serde::{Deserialize, Serialize};

use super::dep_info::{Checksum, ChecksumAlgo};
use crate::util::data_structures::HashMap;
use crate::util::errors::CargoResult;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct OutputFingerprint {
    /// An ID representing some collection of output files.
    #[serde(with = "output_id")]
    pub output_id: u128,
    /// The last time the output files where checked.
    /// This is only updated if the mtime of an output changes, forcing a recheck
    #[serde(with = "unix_millis")]
    pub checked_at: SystemTime,
    /// A set of output files and their checksums
    pub checksums: BTreeMap<PathBuf, Checksum>,
    /// The root of the build-dir
    /// FIXME: Should this be the root of the unit dir instead?
    #[serde(skip)]
    pub root: PathBuf,
    #[serde(skip)]
    pub refreshed: bool,
}

impl OutputFingerprint {
    pub fn new(root: PathBuf) -> Self {
        Self {
            output_id: rand::random(),
            checked_at: SystemTime::UNIX_EPOCH,
            checksums: BTreeMap::new(),
            root,
            refreshed: false,
        }
    }

    pub fn from_short_fingerprint(short: &str, root: PathBuf) -> Option<Self> {
        let mut fields = short.split('\n');
        let hash = fields.next()?;
        if hash.len() != 16 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let id = fields.next()?;
        if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let millis = fields.next()?;
        if millis.is_empty()
            || !millis.bytes().all(|byte| byte.is_ascii_digit())
            || fields.next().is_some()
        {
            return None;
        }
        Some(Self {
            checked_at: unix_millis::from_millis(millis.parse().ok()?)?,
            output_id: u128::from_str_radix(id, 16).ok()?,
            checksums: BTreeMap::new(),
            root,
            refreshed: false,
        })
    }

    pub fn compare(
        &mut self,
        outputs: &[PathBuf],
        mtimes: &HashMap<PathBuf, FileTime>,
        load: impl FnOnce() -> CargoResult<Self>,
    ) -> CargoResult<bool> {
        if self.checked_at == SystemTime::UNIX_EPOCH {
            return Ok(false);
        }

        let now = SystemTime::now();
        let checked_at = FileTime::from_system_time(self.checked_at);
        if now >= self.checked_at && mtimes.values().all(|mtime| *mtime < checked_at) {
            return Ok(true);
        }

        let saved = load()?;
        if saved.output_id != self.output_id
            || saved.checked_at != self.checked_at
            || saved.checksums.len() != outputs.len()
            || outputs.iter().any(|path| {
                !saved
                    .checksums
                    .contains_key(self.relative_path(path).as_ref())
            })
        {
            return Ok(false);
        }

        let checksums = self.compute_checksums(outputs)?;
        if checksums != saved.checksums {
            self.output_id = rand::random();
        }
        self.checksums = checksums;
        self.checked_at = now;
        self.refreshed = true;
        Ok(true)
    }

    pub fn begin_build(&mut self) {
        self.output_id = rand::random();
    }

    pub fn finish_build(&mut self, outputs: &[PathBuf]) -> CargoResult<()> {
        let checked_at = SystemTime::now();
        self.checksums = self.compute_checksums(outputs)?;
        self.checked_at = checked_at;
        Ok(())
    }

    fn compute_checksums(&self, outputs: &[PathBuf]) -> CargoResult<BTreeMap<PathBuf, Checksum>> {
        outputs
            .iter()
            .map(|path| {
                let checksum = (|| -> CargoResult<Checksum> {
                    let mut file = File::open(path)?;
                    let before = file.metadata()?;
                    let checksum = Checksum::compute(ChecksumAlgo::Blake3, &mut file)?;
                    let after = fs::metadata(path)?;
                    if before.len() != after.len() || before.modified()? != after.modified()? {
                        bail!("output changed while computing its checksum");
                    }
                    Ok(checksum)
                })()
                .with_context(|| format!("failed to checksum output `{}`", path.display()))?;
                Ok((self.relative_path(path).into_owned(), checksum))
            })
            .collect()
    }

    fn relative_path<'a>(&self, path: &'a Path) -> Cow<'a, Path> {
        match path.strip_prefix(&self.root) {
            Ok(path) => Cow::Borrowed(path),
            Err(_) => Cow::Owned(
                pathdiff::diff_paths(path, &self.root).unwrap_or_else(|| path.to_path_buf()),
            ),
        }
    }

    // FIXME: This feels like an ugly interface
    pub fn append_to_short(&self, short: &mut String) -> CargoResult<()> {
        let millis = unix_millis::to_millis(&self.checked_at)?;
        write!(short, "\n{:032x}\n{millis}", self.output_id)?;
        Ok(())
    }
}

mod unix_millis {
    use std::time::{Duration, SystemTime};

    use serde::{Deserialize, Deserializer, Serializer};

    use crate::util::errors::CargoResult;

    pub fn to_millis(time: &SystemTime) -> CargoResult<u64> {
        Ok(time
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_millis()
            .try_into()?)
    }

    pub fn from_millis(millis: u64) -> Option<SystemTime> {
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(millis))
    }

    pub fn serialize<S: Serializer>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error> {
        // Round down so newer outputs within this millisecond still get rehashed.
        serializer.serialize_u64(to_millis(time).map_err(serde::ser::Error::custom)?)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SystemTime, D::Error> {
        let millis = u64::deserialize(deserializer)?;
        from_millis(millis)
            .ok_or_else(|| serde::de::Error::custom("Unix millisecond timestamp out of range"))
    }
}

mod output_id {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(id: &u128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("{id:032x}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
        let id = String::deserialize(deserializer)?;
        u128::from_str_radix(&id, 16).map_err(serde::de::Error::custom)
    }
}
