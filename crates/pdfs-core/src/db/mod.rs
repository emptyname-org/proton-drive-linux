//! Unified SQLite metadata cache — the single persistence layer behind FUSE
//! inode bookkeeping, full-text search, content-cache LRU tracking, and pins.
//!
//! Only the daemon (`pdfs-fuse`) opens this for writes; the GUI and CLI reach
//! the same data through the control socket. The connection is wrapped in a
//! `Mutex` because the FUSE callbacks are synchronous and already serialize
//! behind the `State` lock, so a connection pool would be overkill.
//!
//! This module is the P0 foundation: it opens the database, enables WAL, and
//! applies the forward-only schema migrations. Write-through of nodes, the
//! event cursor, FTS, and the cache index land in later phases on this schema.
//!
//! The surface is one `Db` type; its methods live in a submodule per table group
//! (`nodes`, `photos`, `pins`, …) as separate `impl Db` blocks. Everything those
//! modules define is re-exported here, so callers keep saying `db::StoredPhoto`
//! and never name a submodule.

use parking_lot::Mutex;
use rusqlite::Connection;

use crate::Result;
use std::path::Path;

mod activity;
mod albums;
mod cache;
mod devices;
mod local;
mod maintenance;
mod migrations;
mod mounts;
mod nodes;
mod ops;
mod photos;
mod pins;
mod share_access;
mod state;
mod sync;
mod trash;
mod utils;

pub use albums::StoredAlbum;
pub use cache::CacheEntryInput;
pub use devices::StoredDevice;
pub use local::LocalFileHit;
pub use maintenance::{DbStats, VacuumOutcome};
pub use nodes::{PublishedSharedRoot, SearchHit, StoredNode};
pub use ops::{
    AttachedBlob, LOCAL_VOLUME, OP_CREATE, OP_MKDIR, OP_RENAME, OP_REVISION, OP_TRASH, PARK_UNTIL,
    PendingCounts, PendingOp, RenameMeta, op_supersedes,
};
pub use photos::{StoredPhoto, THUMB_HAVE, THUMB_NONE, THUMB_UNKNOWN, TimelineRow};
pub use pins::PinRow;
pub use sync::{StoredSyncEntry, StoredSyncFolder};
pub use trash::StoredTrash;

/// Size the WAL is truncated back to after a checkpoint. Comfortably above the
/// steady-state working set (a few MB), so the truncation only claws back the
/// outliers rather than fighting the normal write path for disk.
const WAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

/// How long a statement waits for a database lock before giving up.
///
/// Long enough to outlast any transaction this daemon writes (the largest, a
/// full-index FTS rebuild, is well under a second), short enough that a
/// genuinely stuck holder surfaces as an error rather than an apparent hang.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Page cache, in KiB (applied negated, which is how SQLite reads a size in KiB
/// rather than in pages). 32 MiB against a database that is typically tens of
/// MiB: large enough to hold the working set of a listing-heavy session, small
/// enough to be unremarkable in a desktop daemon's RSS.
const CACHE_SIZE_KIB: i64 = 32 * 1024;

/// Ceiling on the memory-mapped window over the database file. 256 MiB covers
/// any realistic index while keeping the mapping bounded.
const MMAP_SIZE: i64 = 256 * 1024 * 1024;

/// WAL pages between automatic checkpoints (default 1000, i.e. ~4 MiB). Halved
/// so the synchronous checkpoint a committing thread inherits stays short.
const WAL_AUTOCHECKPOINT_PAGES: i64 = 500;

/// Handle to the unified metadata database.
///
/// Cheap to wrap in an `Arc`; clone the `Arc`, not this. All access goes through
/// the inner `Mutex<Connection>`.
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// Open (creating if absent) the database at `path`, enable WAL, and bring
    /// the schema up to [`SCHEMA_VERSION`].
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        // WAL: readers never block the single writer. NORMAL sync is the
        // standard durability/throughput tradeoff for WAL.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Hand the WAL's disk back after a checkpoint. Without a limit SQLite
        // reuses the file in place but never shrinks it, so a single large
        // transaction (a full-index FTS rebuild in `local_finish_scan` is the
        // one that reaches this size) leaves the WAL at its high-water mark
        // forever — a multi-GB file next to a database two orders of magnitude
        // smaller. Checkpointing is unaffected; only the file is truncated back.
        conn.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT)?;
        // Wait for a lock rather than failing instantly on SQLITE_BUSY.
        //
        // This is deliberately explicit and deliberately redundant: rusqlite
        // already applies a five-second busy timeout of its own, so this changes
        // nothing today. It is here so the value is *ours* — a behaviour the
        // daemon depends on should not be an inherited default that a dependency
        // bump can silently change. `open_bounds_the_wal_size` pins it.
        //
        // (Bare SQLite does default to 0, which is where the belief that this
        // was unset came from. That default is not what we get.)
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Page cache. The default is 2 MiB, against a schema whose hot table
        // stores a JSON blob per node — a listing of a few thousand files
        // evicts itself while it is being read.
        conn.pragma_update(None, "cache_size", -CACHE_SIZE_KIB)?;
        // Sorts and recursive CTEs materialise their intermediate results.
        // Every `ORDER BY … COLLATE NOCASE` listing and every ancestor walk
        // spills those to a file in `/tmp` by default; they are small and
        // short-lived, so memory is both faster and less surprising.
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        // Read the database through the page cache of the OS rather than
        // `pread` per page. Bounded rather than unlimited so the daemon's RSS
        // does not track the database size.
        conn.pragma_update(None, "mmap_size", MMAP_SIZE)?;
        // Checkpointing is synchronous in whichever thread happens to commit
        // the transaction that crosses the threshold — often a FUSE callback.
        // A smaller autocheckpoint makes that debt smaller and more frequent
        // rather than rare and multi-second.
        conn.pragma_update(None, "wal_autocheckpoint", WAL_AUTOCHECKPOINT_PAGES)?;

        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Open an in-memory database. For tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        // Keep relational behavior identical to `open`: V18 relies on the
        // `mount.sync_folder_id` cascade, and SQLite disables foreign keys per
        // connection unless explicitly enabled.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Run a closure with the locked connection. Escape hatch for callers that
    /// need a query no typed method covers yet.
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock();
        f(&conn)
    }
}

#[cfg(test)]
mod tests;
