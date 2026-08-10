//! A persistent [`CacheRepository`] for the SDK's Drive entity cache.
//!
//! The SDK caches decrypted node metadata (name, size, parent, the share whose
//! membership signs operations on it) so navigating a tree does not re-fetch and
//! re-decrypt what it already resolved. That cache defaults to memory, which
//! means every daemon restart pays for the whole walk again. This backs it with
//! SQLite so it survives one.
//!
//! **Its own database file, not `cache.db`.** The daemon's `Db` is a single
//! `Mutex<Connection>` shared by every FUSE thread and the control socket; SDK
//! cache traffic is frequent, small, and entirely reconstructible, so putting it
//! on that mutex would add contention to the daemon's hottest lock (and to a
//! write-ahead log whose growth is already sensitive to how long that lock is
//! held) for no durability benefit. A separate file also means no schema
//! migration: this store can be deleted and rebuilt at any time.
//!
//! **What it holds.** Decrypted metadata, never key material — the SDK keeps
//! node keys and hash keys in a separate in-memory secret cache that nothing
//! here can reach. The same names and sizes are already persisted in `cache.db`
//! (`nodes`), so this file widens no exposure that the state directory did not
//! already have; `AppDirs::ensure` is what keeps that directory private.
//!
//! **Staleness.** A persisted cache outlives the process, so an entry can
//! describe a node another client has since changed. The daemon's event loop is
//! what closes that: it resumes from a persisted cursor and calls the SDK's
//! `invalidate_caches_for_event` for every event it replays, including the ones
//! that happened while the daemon was down. The one case that leaves no trail is
//! a *seeded* cursor — a first-ever mount, or a cursor the daemon lost — where
//! nothing says what changed, so the caller [`clear`](SdkCache::clear)s this
//! store instead of trusting it.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use parking_lot::Mutex;
use proton_sdk::cache::CacheRepository;
use proton_sdk::error::{ProtonError, Result as SdkResult};
use rusqlite::{Connection, params};

use crate::config::AppDirs;
use crate::error::Result;

/// Process-wide handle, so the daemon's event loop can clear the same store the
/// client is writing through without threading it down every call.
static SHARED: OnceLock<Arc<SdkCache>> = OnceLock::new();

/// SQLite-backed entity cache for the SDK.
pub struct SdkCache {
    conn: Mutex<Connection>,
}

impl SdkCache {
    /// Open (creating if needed) the entity cache under the app's state
    /// directory. The first call wins; later calls return the same handle, which
    /// is what lets the event loop clear the store the Drive client is using.
    pub fn shared(dirs: &AppDirs) -> Result<Arc<Self>> {
        if let Some(existing) = SHARED.get() {
            return Ok(existing.clone());
        }
        let cache = Arc::new(Self::open(&dirs.state_dir().join("sdk_cache.db"))?);
        Ok(SHARED.get_or_init(|| cache).clone())
    }

    /// The already-open shared cache, if one was opened in this process.
    pub fn opened() -> Option<Arc<Self>> {
        SHARED.get().cloned()
    }

    /// Open the store at `path`, creating its schema.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Tags cascade with their entry, so a `set` that replaces a value cannot
        // leave the old value's tags behind pointing at the new one.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS entry (
               key   TEXT PRIMARY KEY,
               value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS tag (
               key TEXT NOT NULL REFERENCES entry(key) ON DELETE CASCADE,
               tag TEXT NOT NULL,
               PRIMARY KEY (key, tag)
             );
             CREATE INDEX IF NOT EXISTS idx_tag_tag ON tag(tag);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Drop every entry. Used when the daemon cannot prove the cache is current
    /// — a seeded event cursor — and on logout.
    pub fn clear_now(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch("DELETE FROM tag; DELETE FROM entry;")?;
        Ok(())
    }

    fn set_now(&self, key: &str, value: &str, tags: &[String]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO entry (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        tx.execute("DELETE FROM tag WHERE key = ?1", params![key])?;
        {
            let mut stmt = tx.prepare("INSERT INTO tag (key, tag) VALUES (?1, ?2)")?;
            for tag in tags {
                stmt.execute(params![key, tag])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn get_now(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT value FROM entry WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    fn remove_now(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM entry WHERE key = ?1", params![key])?;
        Ok(())
    }

    fn remove_by_tag_now(&self, tag: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM entry WHERE key IN (SELECT key FROM tag WHERE tag = ?1)",
            params![tag],
        )?;
        Ok(())
    }

    /// Entries carrying **all** of `tags` — the set intersection the SDK's
    /// `get_by_tags` contract specifies, not a union.
    fn get_by_tags_now(&self, tags: &[String]) -> Result<Vec<(String, String)>> {
        if tags.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock();
        let placeholders = vec!["?"; tags.len()].join(",");
        let sql = format!(
            "SELECT e.key, e.value FROM entry e
             JOIN tag t ON t.key = e.key
             WHERE t.tag IN ({placeholders})
             GROUP BY e.key
             HAVING COUNT(DISTINCT t.tag) = {}",
            tags.len()
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(tags), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// A cache failure is never fatal to the operation that triggered it — the SDK
/// treats this repository as best-effort — but it still has to come back as the
/// SDK's error type.
fn sdk_error(e: crate::error::Error) -> ProtonError {
    ProtonError::invalid_operation(format!("sdk entity cache: {e}"))
}

// Every method is a local SQLite statement on its own connection: microseconds,
// no network, no shared lock with the daemon's database. Running them inline on
// the caller's task is cheaper than the thread hop that moving them off would
// cost.
#[async_trait]
impl CacheRepository for SdkCache {
    async fn set(&self, key: &str, value: &str, tags: &[String]) -> SdkResult<()> {
        self.set_now(key, value, tags).map_err(sdk_error)
    }

    async fn get(&self, key: &str) -> SdkResult<Option<String>> {
        self.get_now(key).map_err(sdk_error)
    }

    async fn remove(&self, key: &str) -> SdkResult<()> {
        self.remove_now(key).map_err(sdk_error)
    }

    async fn remove_by_tag(&self, tag: &str) -> SdkResult<()> {
        self.remove_by_tag_now(tag).map_err(sdk_error)
    }

    async fn clear(&self) -> SdkResult<()> {
        self.clear_now().map_err(sdk_error)
    }

    async fn get_by_tags(&self, tags: &[String]) -> SdkResult<Vec<(String, String)>> {
        self.get_by_tags_now(tags).map_err(sdk_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> SdkCache {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(
            "CREATE TABLE entry (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE tag (
               key TEXT NOT NULL REFERENCES entry(key) ON DELETE CASCADE,
               tag TEXT NOT NULL,
               PRIMARY KEY (key, tag)
             );",
        )
        .unwrap();
        SdkCache {
            conn: Mutex::new(conn),
        }
    }

    fn tags(values: &[&str]) -> Vec<String> {
        values.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn set_replaces_the_value_and_its_tags() {
        let cache = cache();
        cache.set_now("k", "one", &tags(&["a", "b"])).unwrap();
        cache.set_now("k", "two", &tags(&["b"])).unwrap();

        assert_eq!(cache.get_now("k").unwrap().as_deref(), Some("two"));
        // The tag dropped by the second write must not still select the entry —
        // that is how a stale invalidation would miss it.
        assert!(cache.get_by_tags_now(&tags(&["a"])).unwrap().is_empty());
        assert_eq!(cache.get_by_tags_now(&tags(&["b"])).unwrap().len(), 1);
    }

    #[test]
    fn get_by_tags_intersects_rather_than_unions() {
        let cache = cache();
        cache.set_now("both", "1", &tags(&["x", "y"])).unwrap();
        cache.set_now("one", "2", &tags(&["x"])).unwrap();

        let hits = cache.get_by_tags_now(&tags(&["x", "y"])).unwrap();
        assert_eq!(hits, vec![("both".to_string(), "1".to_string())]);
        // An empty tag list selects nothing, matching the SDK contract.
        assert!(cache.get_by_tags_now(&[]).unwrap().is_empty());
    }

    #[test]
    fn removing_by_tag_takes_the_entry_and_its_tags() {
        let cache = cache();
        cache.set_now("k", "v", &tags(&["gone"])).unwrap();
        cache.remove_by_tag_now("gone").unwrap();

        assert!(cache.get_now("k").unwrap().is_none());
        assert!(cache.get_by_tags_now(&tags(&["gone"])).unwrap().is_empty());
    }

    #[test]
    fn clear_empties_the_store() {
        let cache = cache();
        cache.set_now("a", "1", &tags(&["t"])).unwrap();
        cache.set_now("b", "2", &[]).unwrap();
        cache.clear_now().unwrap();

        assert!(cache.get_now("a").unwrap().is_none());
        assert!(cache.get_now("b").unwrap().is_none());
    }
}
