//! Local graph inventories for shared blob storage. The caller holds the package
//! cache lock for every database operation, and the mutation lock while cleaning.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, Row, Transaction, params};

use crate::CargoResult;
use crate::ops::CleanContext;
use crate::util::data_structures::{HashMap, HashSet};
use crate::util::sqlite::{Migration, basic_migration, migrate};

pub(super) type Digest = [u8; 32];

const USAGE_UPDATE_INTERVAL: u64 = 4 * 60 * 60;
const RETENTION: u64 = 30 * 24 * 60 * 60;

#[derive(Debug)]
pub(super) struct Output {
    pub path: Vec<u8>,
    pub hash: Digest,
    pub size: u64,
}

#[derive(Debug)]
pub(super) struct UnitResult {
    pub id: Digest,
    /// Build-dir freshness token, deliberately excluded from the result ID.
    pub generation: Digest,
    pub outputs: Vec<Output>,
}

impl UnitResult {
    pub fn new(mut outputs: Vec<Output>) -> Self {
        outputs.sort_unstable_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.hash.cmp(&b.hash))
                .then_with(|| a.size.cmp(&b.size))
        });
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"cargo-shared-blob-unit-result-v1\0");
        hasher.update(&(outputs.len() as u64).to_le_bytes());
        for output in &outputs {
            hasher.update(&(output.path.len() as u64).to_le_bytes());
            hasher.update(&output.path);
            hasher.update(&output.hash);
            hasher.update(&output.size.to_le_bytes());
        }
        Self {
            id: *hasher.finalize().as_bytes(),
            generation: [0; 32],
            outputs,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct StoredUnit {
    pub result: Digest,
    pub generation: Digest,
}

pub(super) struct SnapshotIndex {
    conn: Connection,
}

impl SnapshotIndex {
    pub fn open(root: &Path) -> CargoResult<Self> {
        cargo_util::paths::create_dir_all(root)?;
        let mut conn = Connection::open(root.join("index.sqlite"))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut conn, &migrations())?;
        Ok(Self { conn })
    }

    /// A legacy cache can be inspected without creating an index. Existing
    /// indexes are not migrated during dry-run; all cleanup writes roll back.
    pub fn open_for_clean(root: &Path, dry_run: bool) -> CargoResult<Self> {
        if !dry_run {
            return Self::open(root);
        }
        let path = root.join("index.sqlite");
        let conn = if path.try_exists()? {
            Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?
        } else {
            let mut conn = Connection::open_in_memory()?;
            migrate(&mut conn, &migrations())?;
            conn
        };
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(Self { conn })
    }

    pub fn load_units(&self, build_dir: &[u8]) -> CargoResult<HashMap<Vec<u8>, StoredUnit>> {
        let mut statement = self.conn.prepare(
            "SELECT unit_path, result_id, generation FROM build_unit WHERE build_dir = ?1",
        )?;
        let units = statement.query_map([build_dir], |row| {
            Ok((
                row.get(0)?,
                StoredUnit {
                    result: read_digest(row, 1)?,
                    generation: read_digest(row, 2)?,
                },
            ))
        })?;
        Ok(units.collect::<rusqlite::Result<_>>()?)
    }

    /// Persist completed work even on failure, but only publish a graph when the
    /// entire build succeeded. Unit slots are lookup hints, not retention roots.
    pub fn save(
        &mut self,
        build_dir: &[u8],
        invalidated: &HashSet<Vec<u8>>,
        updates: &HashMap<Vec<u8>, UnitResult>,
        snapshot: Option<Vec<Digest>>,
        now: u64,
    ) -> CargoResult<()> {
        let tx = self.conn.transaction()?;
        if !invalidated.is_empty() || !updates.is_empty() {
            let mut invalidate = tx
                .prepare_cached("DELETE FROM build_unit WHERE build_dir = ?1 AND unit_path = ?2")?;
            for path in invalidated {
                invalidate.execute(params![build_dir, path])?;
            }
            let mut insert_result =
                tx.prepare_cached("INSERT OR IGNORE INTO unit_result (id) VALUES (?1)")?;
            let mut insert_blob =
                tx.prepare_cached("INSERT OR IGNORE INTO blob (hash, size) VALUES (?1, ?2)")?;
            let mut insert_output = tx.prepare_cached(
                "INSERT INTO unit_output (result_id, path, blob_hash, size)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut update_slot = tx.prepare_cached(
                "INSERT INTO build_unit (build_dir, unit_path, result_id, generation)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (build_dir, unit_path) DO UPDATE
                 SET result_id = excluded.result_id, generation = excluded.generation
                 WHERE build_unit.result_id != excluded.result_id
                    OR build_unit.generation != excluded.generation",
            )?;
            for (path, result) in updates {
                if insert_result.execute([result.id.as_slice()])? != 0 {
                    for output in &result.outputs {
                        insert_blob.execute(params![output.hash.as_slice(), output.size])?;
                        insert_output.execute(params![
                            result.id.as_slice(),
                            output.path,
                            output.hash.as_slice(),
                            output.size,
                        ])?;
                    }
                }
                update_slot.execute(params![
                    build_dir,
                    path,
                    result.id.as_slice(),
                    result.generation.as_slice(),
                ])?;
            }
        }
        if let Some(mut results) = snapshot {
            let id = snapshot_id(&mut results);
            if tx.execute(
                "INSERT OR IGNORE INTO snapshot (id) VALUES (?1)",
                [id.as_slice()],
            )? != 0
            {
                let mut insert_member = tx.prepare_cached(
                    "INSERT INTO snapshot_member (snapshot_id, result_id) VALUES (?1, ?2)",
                )?;
                for result in results {
                    insert_member.execute(params![id.as_slice(), result.as_slice()])?;
                }
            }
            tx.execute(
                "INSERT INTO snapshot_usage (build_dir, snapshot_id, last_used) VALUES (?1, ?2, ?3)
                 ON CONFLICT (build_dir, snapshot_id) DO UPDATE
                 SET last_used = MAX(snapshot_usage.last_used, excluded.last_used)
                 WHERE excluded.last_used - snapshot_usage.last_used >= ?4",
                params![build_dir, id.as_slice(), now, USAGE_UPDATE_INTERVAL],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn clean(
        &mut self,
        root: &Path,
        clean_ctx: &mut CleanContext<'_>,
        max_size: Option<u64>,
        now: u64,
    ) -> CargoResult<()> {
        // Inventory only regular CAS files. In particular, do not follow a
        // digest-named symlink or recurse into a digest-named directory.
        let mut files = HashMap::default();
        let mut legacy_timestamps = Vec::new();
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some(hash) = digest_filename(name) {
                files.insert(hash, (entry.path(), entry.metadata()?.len()));
            } else if name
                .strip_suffix(".timestamp")
                .and_then(digest_filename)
                .is_some()
            {
                legacy_timestamps.push(entry.path());
            }
        }

        let tx = self.conn.transaction()?;
        // A missing output makes a result incomplete. Drop affected snapshots
        // instead of changing their content-addressed membership in place.
        let missing = {
            let mut statement = tx.prepare("SELECT hash, size FROM blob")?;
            let blobs =
                statement.query_map([], |row| Ok((read_digest(row, 0)?, row.get::<_, u64>(1)?)))?;
            let mut missing = Vec::new();
            for blob in blobs {
                let (hash, size) = blob?;
                if !files.get(&hash).is_some_and(|(_, actual)| *actual == size) {
                    missing.push(hash);
                }
            }
            missing
        };
        {
            let mut remove_snapshots = tx.prepare_cached(
                "DELETE FROM snapshot WHERE id IN (
                     SELECT snapshot_id FROM snapshot_member
                     JOIN unit_output ON unit_output.result_id = snapshot_member.result_id
                     WHERE blob_hash = ?1
                 )",
            )?;
            let mut remove_results = tx.prepare_cached(
                "DELETE FROM unit_result WHERE id IN (
                     SELECT result_id FROM unit_output WHERE blob_hash = ?1
                 )",
            )?;
            for hash in missing {
                remove_snapshots.execute([hash.as_slice()])?;
                remove_results.execute([hash.as_slice()])?;
            }
        }
        tx.execute(
            "DELETE FROM snapshot_usage WHERE last_used < ?1",
            [now.saturating_sub(RETENTION)],
        )?;
        tx.execute(
            "DELETE FROM snapshot WHERE NOT EXISTS (
                 SELECT 1 FROM snapshot_usage WHERE snapshot_id = snapshot.id
             )",
            [],
        )?;
        prune_results(&tx)?;

        if let Some(max_size) = max_size {
            evict_to_size(&tx, max_size)?;
            prune_results(&tx)?;
        }

        let retained = {
            let mut statement = tx.prepare("SELECT hash FROM blob")?;
            let rows = statement.query_map([], |row| read_digest(row, 0))?;
            rows.collect::<rusqlite::Result<HashSet<_>>>()?
        };
        // Remove names, never the build-dir links. If removal fails, rolling back
        // the index is conservative; a subsequent clean repairs missing files.
        for (hash, (path, _)) in files {
            if !retained.contains(&hash) {
                clean_ctx.rm_rf(&path)?;
            }
        }
        for path in legacy_timestamps {
            clean_ctx.rm_rf(&path)?;
        }
        if clean_ctx.dry_run {
            tx.rollback()?;
        } else {
            tx.commit()?;
        }
        Ok(())
    }
}

fn snapshot_id(results: &mut Vec<Digest>) -> Digest {
    results.sort_unstable();
    results.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cargo-shared-blob-snapshot-v1\0");
    hasher.update(&(results.len() as u64).to_le_bytes());
    for result in results.iter() {
        hasher.update(result);
    }
    *hasher.finalize().as_bytes()
}

fn prune_results(tx: &Transaction<'_>) -> CargoResult<()> {
    tx.execute(
        "DELETE FROM unit_result WHERE NOT EXISTS (
             SELECT 1 FROM snapshot_member WHERE result_id = unit_result.id
         )",
        [],
    )?;
    tx.execute(
        "DELETE FROM blob WHERE NOT EXISTS (
             SELECT 1 FROM unit_output WHERE blob_hash = blob.hash
         )",
        [],
    )?;
    Ok(())
}

fn evict_to_size(tx: &Transaction<'_>, max_size: u64) -> CargoResult<()> {
    let mut size: u64 = tx.query_row("SELECT COALESCE(SUM(size), 0) FROM blob", [], |row| {
        row.get(0)
    })?;
    if size <= max_size {
        return Ok(());
    }
    let oldest = {
        let mut statement = tx.prepare(
            "SELECT snapshot.id FROM snapshot
             JOIN snapshot_usage ON snapshot_usage.snapshot_id = snapshot.id
             GROUP BY snapshot.id ORDER BY MAX(last_used), snapshot.id",
        )?;
        statement
            .query_map([], |row| read_digest(row, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut exclusive_size = tx.prepare_cached(
        "SELECT COALESCE(SUM(size), 0) FROM blob WHERE hash IN (
             SELECT blob_hash FROM unit_output
             JOIN snapshot_member ON snapshot_member.result_id = unit_output.result_id
             WHERE snapshot_id = ?1
         ) AND NOT EXISTS (
             SELECT 1 FROM unit_output
             JOIN snapshot_member ON snapshot_member.result_id = unit_output.result_id
             WHERE blob_hash = blob.hash AND snapshot_id != ?1
         )",
    )?;
    let mut remove = tx.prepare_cached("DELETE FROM snapshot WHERE id = ?1")?;
    for id in oldest {
        let reclaimed: u64 = exclusive_size.query_row([id.as_slice()], |row| row.get(0))?;
        remove.execute([id.as_slice()])?;
        size = size.saturating_sub(reclaimed);
        if size <= max_size {
            break;
        }
    }
    Ok(())
}

fn read_digest(row: &Row<'_>, column: usize) -> rusqlite::Result<Digest> {
    let value = row.get_ref(column)?;
    match value {
        rusqlite::types::ValueRef::Blob(bytes) => bytes.try_into().map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Blob,
                Box::new(err),
            )
        }),
        _ => Err(rusqlite::Error::InvalidColumnType(
            column,
            column.to_string(),
            value.data_type(),
        )),
    }
}

fn digest_filename(name: &str) -> Option<Digest> {
    let bytes = name.as_bytes();
    if bytes.len() != 64 {
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
    for (dest, pair) in digest.iter_mut().zip(bytes.chunks_exact(2)) {
        *dest = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(digest)
}

/// Append migrations; their positions are the persistent schema version.
fn migrations() -> Vec<Migration> {
    vec![
        basic_migration(
            "CREATE TABLE blob (
                 hash BLOB PRIMARY KEY NOT NULL CHECK(length(hash) = 32),
                 size INTEGER NOT NULL CHECK(size >= 0)
             ) WITHOUT ROWID",
        ),
        basic_migration(
            "CREATE TABLE unit_result (
                 id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 32)
             ) WITHOUT ROWID",
        ),
        basic_migration(
            "CREATE TABLE unit_output (
                 result_id BLOB NOT NULL REFERENCES unit_result(id) ON DELETE CASCADE,
                 path BLOB NOT NULL,
                 blob_hash BLOB NOT NULL REFERENCES blob(hash),
                 size INTEGER NOT NULL CHECK(size >= 0),
                 PRIMARY KEY (result_id, path)
             ) WITHOUT ROWID",
        ),
        basic_migration(
            "CREATE TABLE build_unit (
                 build_dir BLOB NOT NULL,
                 unit_path BLOB NOT NULL,
                 result_id BLOB NOT NULL REFERENCES unit_result(id) ON DELETE CASCADE,
                 generation BLOB NOT NULL CHECK(length(generation) = 32),
                 PRIMARY KEY (build_dir, unit_path)
             ) WITHOUT ROWID",
        ),
        basic_migration(
            "CREATE TABLE snapshot (
                 id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 32)
             ) WITHOUT ROWID",
        ),
        basic_migration(
            "CREATE TABLE snapshot_member (
                 snapshot_id BLOB NOT NULL REFERENCES snapshot(id) ON DELETE CASCADE,
                 result_id BLOB NOT NULL REFERENCES unit_result(id) ON DELETE CASCADE,
                 PRIMARY KEY (snapshot_id, result_id)
             ) WITHOUT ROWID",
        ),
        basic_migration(
            "CREATE TABLE snapshot_usage (
                 build_dir BLOB NOT NULL,
                 snapshot_id BLOB NOT NULL REFERENCES snapshot(id) ON DELETE CASCADE,
                 last_used INTEGER NOT NULL CHECK(last_used >= 0),
                 PRIMARY KEY (build_dir, snapshot_id)
             ) WITHOUT ROWID",
        ),
        basic_migration("CREATE INDEX unit_output_blob ON unit_output(blob_hash)"),
        basic_migration("CREATE INDEX build_unit_result ON build_unit(result_id)"),
        basic_migration("CREATE INDEX snapshot_member_result ON snapshot_member(result_id)"),
        basic_migration("CREATE INDEX snapshot_usage_snapshot ON snapshot_usage(snapshot_id)"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(path: &[u8], byte: u8) -> Output {
        Output {
            path: path.to_vec(),
            hash: [byte; 32],
            size: 1,
        }
    }

    #[test]
    fn inventory_identity_uses_paths_and_contents_not_order() {
        let first = UnitResult::new(vec![output(b"a", 1), output(b"\xff", 2)]);
        let reordered = UnitResult::new(vec![output(b"\xff", 2), output(b"a", 1)]);
        assert_eq!(first.id, reordered.id);
        assert_ne!(
            first.id,
            UnitResult::new(vec![output(b"b", 1), output(b"\xff", 2)]).id
        );
        assert_ne!(
            first.id,
            UnitResult::new(vec![output(b"a", 3), output(b"\xff", 2)]).id
        );
    }

    #[test]
    fn graph_identity_is_a_set_of_results() {
        assert_eq!(
            snapshot_id(&mut vec![[1; 32], [2; 32]]),
            snapshot_id(&mut vec![[2; 32], [1; 32], [1; 32]])
        );
        assert_ne!(
            snapshot_id(&mut vec![[1; 32]]),
            snapshot_id(&mut vec![[2; 32]])
        );
        assert_ne!(UnitResult::new(Vec::new()).id, snapshot_id(&mut Vec::new()));
    }
}
