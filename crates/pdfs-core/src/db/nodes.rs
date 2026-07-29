//! Node rows: the persisted mirror of the remote tree, plus the trigram
//! full-text index over names and paths that backs `Request::Search`.

use std::collections::HashSet;

use rusqlite::{OptionalExtension, Transaction, params};

use super::Db;
use crate::{Access, Result};
use proton_drive_rs::proton_sdk::ids::{LinkId, NodeUid, VolumeId};
use proton_drive_rs::{Node, NodeKind};

use super::utils::{TRIGRAM_MIN, collect_pairs, like_escape, pair, path_of};

pub struct StoredNode {
    pub node: Node,
    pub listed: bool,
}

/// One full-text search match: the stored [`Node`] plus its mountpoint-relative
/// path (`/`-joined, root excluded) so the front-end can navigate to or open it.
pub struct SearchHit {
    pub node: Node,
    pub path: String,
}

pub struct PublishedSharedRoot {
    pub node: Node,
    pub access: Access,
}

impl Db {
    pub fn upsert_node(&self, node: &Node) -> Result<()> {
        self.upsert_nodes(std::slice::from_ref(node))
    }

    /// Write-through a batch of nodes as one transaction — a whole directory
    /// listing, typically. Otherwise identical to [`upsert_node`](Self::upsert_node),
    /// which is the single-node case of it.
    ///
    /// The commit count is the point: SQLite autocommits every statement that is
    /// not in an explicit transaction, so interning a folder of a thousand
    /// children row-by-row cost a thousand fsyncs, and `ls` waited for all of
    /// them.
    pub fn upsert_nodes(&self, nodes: &[Node]) -> Result<()> {
        if nodes.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        for node in nodes {
            upsert_node_tx(&tx, node)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Return visible direct children without consulting the parent's `listed`
    /// flag. Synthetic shared listings use this to serve the last completed
    /// snapshot while offline, including after a live event expired its TTL.
    pub fn visible_children(&self, parent: &NodeUid) -> Result<Vec<Node>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT node_json FROM nodes
              WHERE parent_uid = ?1 AND trashed = 0 AND node_json IS NOT NULL
              ORDER BY name, uid",
        )?;
        let rows = stmt.query_map([parent.to_string()], |row| row.get::<_, String>(0))?;
        let mut nodes = Vec::new();
        for row in rows {
            nodes.push(serde_json::from_str(&row?)?);
        }
        Ok(nodes)
    }

    /// Atomically publish one completed `Shared with me` root listing.
    ///
    /// Roots absent from `accepted` and every persisted descendant are
    /// tombstoned, not deleted. Accepted roots omitted from materialization keep
    /// their visible snapshot but are downgraded to Viewer until membership is
    /// verified again. Both cases preserve queued operations and staged writes.
    pub fn publish_shared_roots(
        &self,
        parent: &NodeUid,
        accepted: &[NodeUid],
        roots: &[PublishedSharedRoot],
    ) -> Result<Vec<NodeUid>> {
        let parent = parent.to_string();
        let present: HashSet<String> = accepted.iter().map(NodeUid::to_string).collect();
        let materialized: HashSet<String> = roots
            .iter()
            .map(|published| published.node.uid.to_string())
            .collect();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;

        let existing = direct_child_uids_tx(&tx, &parent)?;
        let removed: Vec<String> = existing
            .into_iter()
            .filter(|uid| !present.contains(uid))
            .collect();

        for published in roots {
            if !present.contains(&published.node.uid.to_string()) {
                continue;
            }
            upsert_node_tx(&tx, &published.node)?;
            upsert_share_access_tx(&tx, &published.node.uid.to_string(), published.access)?;
        }
        for uid in present.difference(&materialized) {
            upsert_share_access_tx(&tx, uid, Access::Viewer)?;
        }
        for root in &removed {
            tombstone_subtree_tx(&tx, root)?;
            upsert_share_access_tx(&tx, root, Access::Viewer)?;
        }
        tx.execute(
            "UPDATE nodes SET listed = 1 WHERE uid = ?1",
            params![parent],
        )?;
        tx.commit()?;

        Ok(removed
            .into_iter()
            .filter_map(|uid| parse_node_uid(&uid))
            .collect())
    }

    /// Atomically withdraw a deleted foreign subtree and deny stale handles.
    ///
    /// Node rows and queued operations remain so retries and staged writes keep
    /// their references. FTS and completed-listing state are withdrawn for the
    /// whole subtree, and the deleted UID becomes a fail-closed authority.
    pub fn tombstone_foreign_subtree(&self, uid: &NodeUid) -> Result<()> {
        let uid = uid.to_string();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tombstone_subtree_tx(&tx, &uid)?;
        upsert_share_access_tx(&tx, &uid, Access::Viewer)?;
        tx.commit()?;
        Ok(())
    }

    /// Publish a completed foreign-folder listing from its authoritative UID
    /// list and the subset that materialized successfully. Accepted-but-omitted
    /// children retain their previous snapshot; only absent UIDs are tombstoned.
    pub fn publish_foreign_children(
        &self,
        parent: &NodeUid,
        accepted: &[NodeUid],
        materialized: &[Node],
    ) -> Result<Vec<NodeUid>> {
        let parent = parent.to_string();
        let present: HashSet<String> = accepted.iter().map(NodeUid::to_string).collect();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let removed: Vec<String> = direct_child_uids_tx(&tx, &parent)?
            .into_iter()
            .filter(|uid| !present.contains(uid))
            .collect();
        for node in materialized {
            if present.contains(&node.uid.to_string()) {
                upsert_node_tx(&tx, node)?;
            }
        }
        for uid in &removed {
            tombstone_subtree_tx(&tx, uid)?;
        }
        tx.execute(
            "UPDATE nodes SET listed = 1 WHERE uid = ?1",
            params![parent],
        )?;
        tx.commit()?;
        Ok(removed
            .into_iter()
            .filter_map(|uid| parse_node_uid(&uid))
            .collect())
    }

    /// Persist the synthetic root and its Viewer authority only when its
    /// presentation state changed. Returning `false` guarantees no node/FTS
    /// write occurred, which keeps every cached root lookup O(1).
    pub fn ensure_virtual_root(&self, node: &Node) -> Result<bool> {
        let uid = node.uid.to_string();
        let desired_parent = node.parent_uid.as_ref().map(NodeUid::to_string);
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let stored: Option<(Option<String>, String, i64, i64)> = tx
            .query_row(
                "SELECT parent_uid, name, is_dir, trashed FROM nodes WHERE uid = ?1",
                params![uid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let access: Option<String> = tx
            .query_row(
                "SELECT access FROM share_access WHERE root_uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .optional()?;
        let unchanged = stored.is_some_and(|(parent, name, is_dir, trashed)| {
            parent == desired_parent
                && name == node.name
                && is_dir == 1
                && trashed == node.trashed as i64
        }) && access.as_deref() == Some(Access::Viewer.as_db_str());
        if unchanged {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO share_access (root_uid, access) VALUES (?1, ?2)
             ON CONFLICT(root_uid) DO UPDATE SET access = excluded.access",
            params![uid, Access::Viewer.as_db_str()],
        )?;
        upsert_node_tx(&tx, node)?;
        tx.commit()?;
        Ok(true)
    }

    /// Drop a node row (delete or trash from the hot cache). Children rows are
    /// not cascaded here; the daemon forgets a whole subtree node-by-node.
    pub fn delete_node(&self, uid: &NodeUid) -> Result<()> {
        let uid = uid.to_string();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        // Read the rowid before dropping the node: it is the search index's key
        // (B12), and it is gone once the row is. A node with no row leaves
        // nothing to unindex — the index is keyed off `nodes`, so it cannot
        // hold an entry the table never had.
        let rowid: Option<i64> = tx
            .query_row(
                "SELECT rowid FROM nodes WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .optional()?;
        tx.execute("DELETE FROM nodes WHERE uid = ?1", params![uid])?;
        if let Some(rowid) = rowid {
            tx.execute("DELETE FROM nodes_fts WHERE rowid = ?1", params![rowid])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Atomically retire a landed trash operation and its retained authority.
    ///
    /// Queued trash keeps the node row so drain-time permission checks can
    /// resolve its shared-tree access. Removing the op first could resurrect
    /// that row after a crash; removing the row first could strand a now
    /// unauthorizable op. One transaction closes both windows.
    pub fn complete_trash_op(&self, op_id: i64, uid: &NodeUid) -> Result<()> {
        let uid = uid.to_string();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let rowid: Option<i64> = tx
            .query_row(
                "SELECT rowid FROM nodes WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .optional()?;
        tx.execute("DELETE FROM nodes WHERE uid = ?1", params![uid])?;
        if let Some(rowid) = rowid {
            tx.execute("DELETE FROM nodes_fts WHERE rowid = ?1", params![rowid])?;
        }
        tx.execute("DELETE FROM pending_op WHERE id = ?1", params![op_id])?;
        tx.commit()?;
        Ok(())
    }

    /// Check if a folder node has any non-trashed children in the database.
    pub fn has_children(&self, parent_uid: &NodeUid) -> Result<bool> {
        let uid_str = parent_uid.to_string();
        let conn = self.conn.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE parent_uid = ?1 AND trashed = 0",
            params![uid_str],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Resolve `uid` relative to an ancestor node.
    ///
    /// This is used when a remote subtree has its own local sync mount: the
    /// sync folder stores the ancestor UID, while search results identify the
    /// selected descendant. Returning `None` for an unrelated or incomplete
    /// chain keeps callers from accidentally joining a Drive-wide path onto the
    /// wrong mountpoint. The ancestor itself resolves to the empty path.
    pub fn path_relative_to(&self, ancestor_uid: &str, uid: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "WITH RECURSIVE anc(uid, parent_uid, name, depth) AS (
               SELECT uid, parent_uid, name, 0 FROM nodes WHERE uid = ?1
               UNION ALL
               SELECT n.uid, n.parent_uid, n.name, anc.depth + 1
               FROM nodes n JOIN anc ON n.uid = anc.parent_uid
               WHERE anc.depth < 1024
             )
             SELECT uid, name FROM anc ORDER BY depth",
        )?;
        let rows = stmt.query_map(params![uid], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut parts = Vec::new();
        for row in rows {
            let (current_uid, name) = row?;
            if current_uid == ancestor_uid {
                parts.reverse();
                return Ok(Some(parts.join("/")));
            }
            parts.push(name);
        }
        Ok(None)
    }

    /// Full-text search over node names, newest schema's trigram index giving
    /// substring (not just prefix) matches. Returns up to `limit` non-trashed
    /// hits, each with its mountpoint-relative path resolved. Queries shorter
    /// than [`TRIGRAM_MIN`] fall back to a `LIKE` scan since trigram indexes
    /// nothing below 3 chars.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock();
        let rows: Vec<(String, String)> = if query.chars().count() < TRIGRAM_MIN {
            let pat = format!("%{}%", like_escape(query));
            let mut stmt = conn.prepare(
                "SELECT node_json, uid FROM nodes
                 WHERE name LIKE ?1 ESCAPE '\\' AND trashed = 0 AND node_json IS NOT NULL
                 ORDER BY name LIMIT ?2",
            )?;
            collect_pairs(stmt.query_map(params![pat, limit as i64], pair)?)?
        } else {
            // Escape double quotes and quote each term, then combine with AND so
            // all terms must match but can appear in any order or position.
            let phrase = query
                .split_whitespace()
                .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            let mut stmt = conn.prepare(
                "SELECT n.node_json, n.uid
                 FROM nodes_fts f JOIN nodes n ON n.rowid = f.rowid
                 WHERE f.name MATCH ?1 AND n.trashed = 0 AND n.node_json IS NOT NULL
                 ORDER BY f.rank LIMIT ?2",
            )?;
            collect_pairs(stmt.query_map(params![phrase, limit as i64], pair)?)?
        };

        let mut hits = Vec::with_capacity(rows.len());
        for (json, uid) in rows {
            let node: Node = serde_json::from_str(&json)?;
            let path = path_of(&conn, &uid)?;
            hits.push(SearchHit { node, path });
        }
        Ok(hits)
    }

    /// Return a bounded, deliberately broad candidate pool for fuzzy ranking.
    /// Unlike [`search`](Self::search), trigram terms are ORed across both the
    /// basename and parent path, so a typo can still share enough trigrams to
    /// enter the pool. Final relevance ordering belongs to the caller.
    pub fn search_candidates(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock();
        let terms = candidate_trigrams(query);
        let lane_limit = limit.div_ceil(2);
        let mut rows: Vec<(String, String)> = if terms.is_empty() {
            let pat = format!("%{}%", like_escape(query));
            let mut stmt = conn.prepare(
                "SELECT node_json, uid FROM nodes
                 WHERE name LIKE ?1 ESCAPE '\\' AND trashed = 0 AND node_json IS NOT NULL
                 ORDER BY name LIMIT ?2",
            )?;
            collect_pairs(stmt.query_map(params![pat, limit as i64], pair)?)?
        } else {
            let expression = terms.join(" OR ");
            let mut stmt = conn.prepare(
                "SELECT n.node_json, n.uid
                   FROM nodes_fts f JOIN nodes n ON n.rowid = f.rowid
                  WHERE nodes_fts MATCH ?1 AND n.trashed = 0 AND n.node_json IS NOT NULL
                  ORDER BY f.rank LIMIT ?2",
            )?;
            collect_pairs(stmt.query_map(params![expression, lane_limit as i64], pair)?)?
        };
        // A short substitution can destroy every trigram (`vedio` vs `video`).
        // Feed the scorer an additional indexed same-initial lane so it can
        // recover those candidates without scanning the full node table.
        if !terms.is_empty()
            && let Some(first) = query.chars().next()
        {
            let prefix = format!("{}%", like_escape(&first.to_string()));
            let mut stmt = conn.prepare(
                "SELECT node_json, uid FROM nodes
                 WHERE name COLLATE NOCASE LIKE ?1 ESCAPE '\\'
                   AND trashed = 0 AND node_json IS NOT NULL
                 ORDER BY name COLLATE NOCASE LIMIT ?2",
            )?;
            let fallback =
                collect_pairs(stmt.query_map(params![prefix, lane_limit as i64], pair)?)?;
            for row in fallback {
                if !rows.iter().any(|(_, uid)| uid == &row.1) {
                    rows.push(row);
                }
            }
            rows.truncate(limit);
        }
        rows.into_iter()
            .map(|(json, uid)| {
                Ok(SearchHit {
                    node: serde_json::from_str(&json)?,
                    path: path_of(&conn, &uid)?,
                })
            })
            .collect()
    }

    /// Mark (or unmark) a folder's child listing as complete. A listed folder
    /// rehydrates its `children` map on mount even when empty; an unlisted one
    /// re-enumerates from the remote on next access.
    pub fn set_listed(&self, uid: &NodeUid, listed: bool) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE nodes SET listed = ?2 WHERE uid = ?1",
            params![uid.to_string(), listed as i64],
        )?;
        Ok(())
    }

    /// Load every persisted node for cold-start hydration of the `State` maps.
    pub fn load_all(&self) -> Result<Vec<StoredNode>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT node_json, listed FROM nodes WHERE node_json IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            let json: String = row.get(0)?;
            let listed: i64 = row.get(1)?;
            Ok((json, listed != 0))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (json, listed) = row?;
            let node: Node = serde_json::from_str(&json)?;
            out.push(StoredNode { node, listed });
        }
        Ok(out)
    }

    /// Load one persisted node back by uid. Used to recover the My Files root
    /// when the API is unreachable, so the mount can still serve the cached tree
    /// (offline.md Phase 1).
    pub fn node_by_uid(&self, uid: &str) -> Result<Option<Node>> {
        let conn = self.conn.lock();
        let json: Option<String> = conn
            .query_row(
                "SELECT node_json FROM nodes WHERE uid = ?1 AND node_json IS NOT NULL",
                params![uid],
                |r| r.get(0),
            )
            .optional()?;
        match json {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    /// Read the persisted incremental-sync cursor (a `DriveEventId`), if any.
    /// The daemon resumes from this on restart instead of reseeding to the
    /// server head, so changes made while unmounted are still applied (P2).
    pub fn children_if_listed(&self, parent: &NodeUid) -> Result<Option<Vec<Node>>> {
        let conn = self.conn.lock();
        let listed: Option<i64> = conn
            .query_row(
                "SELECT listed FROM nodes WHERE uid = ?1",
                params![parent.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if listed != Some(1) {
            return Ok(None);
        }
        let mut stmt = conn.prepare(
            "SELECT node_json FROM nodes
             WHERE parent_uid = ?1 AND node_json IS NOT NULL AND trashed = 0",
        )?;
        let rows = stmt.query_map(params![parent.to_string()], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for json in rows {
            out.push(serde_json::from_str(&json?)?);
        }
        Ok(Some(out))
    }

    // --- Content-cache LRU index (P4) -------------------------------------
    //
    // Replaces the per-eviction `read_dir` scans in `ContentCache`. Each cached
    // blob/block carries one row keyed by its on-disk filename (`cache_key`),
    // tagged with `kind` ('blob' | 'block') so the two byte budgets stay
    // separate. `last_accessed` (unix seconds) is the LRU key. The daemon owns
    // the on-disk cache and rebuilds this index from disk on open, then keeps it
    // in sync on every store/read/evict, so it is authoritative for eviction.
}

fn upsert_node_tx(tx: &Transaction<'_>, node: &Node) -> Result<()> {
    let json = serde_json::to_string(node)?;
    let uid = node.uid.to_string();
    tx.execute(
        "INSERT INTO nodes
           (uid, parent_uid, name, is_dir, size, mtime, trashed, node_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(uid) DO UPDATE SET
           parent_uid = excluded.parent_uid,
           name       = excluded.name,
           is_dir     = excluded.is_dir,
           size       = excluded.size,
           mtime      = excluded.mtime,
           trashed    = excluded.trashed,
           node_json  = excluded.node_json",
        params![
            uid,
            node.parent_uid.as_ref().map(|u| u.to_string()),
            node.name,
            node.is_folder() as i64,
            node_size(node),
            node.modification_time,
            node.trashed as i64,
            json,
        ],
    )?;
    // FTS5 has no UPSERT. Folder moves also change every descendant path, so
    // refresh the whole affected subtree while keeping rowid deletes indexed.
    let rowid: i64 = tx.query_row(
        "SELECT rowid FROM nodes WHERE uid = ?1",
        params![uid],
        |row| row.get(0),
    )?;
    let mut affected = vec![rowid];
    if node.is_folder() {
        let mut stmt = tx.prepare(
            "WITH RECURSIVE descendants(rowid, uid) AS (
               SELECT rowid, uid FROM nodes WHERE parent_uid = ?1
               UNION ALL
               SELECT n.rowid, n.uid FROM nodes n
                 JOIN descendants d ON n.parent_uid = d.uid
             ) SELECT rowid FROM descendants",
        )?;
        for descendant in stmt.query_map(params![uid], |row| row.get(0))? {
            affected.push(descendant?);
        }
    }
    for affected_rowid in affected {
        tx.execute(
            "DELETE FROM nodes_fts WHERE rowid = ?1",
            params![affected_rowid],
        )?;
        let indexed: Option<(String, String)> = tx
            .query_row(
                "SELECT name, uid FROM nodes WHERE rowid = ?1",
                params![affected_rowid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((name, indexed_uid)) = indexed
            && node_is_indexable_tx(tx, &indexed_uid)?
        {
            let path = path_of(tx, &indexed_uid)?;
            tx.execute(
                "INSERT INTO nodes_fts (rowid, name, path) VALUES (?1, ?2, ?3)",
                params![affected_rowid, name, path],
            )?;
        }
    }
    Ok(())
}

fn direct_child_uids_tx(tx: &Transaction<'_>, parent: &str) -> Result<Vec<String>> {
    let mut stmt = tx.prepare("SELECT uid FROM nodes WHERE parent_uid = ?1")?;
    let rows = stmt.query_map([parent], |row| row.get::<_, String>(0))?;
    let mut uids = Vec::new();
    for row in rows {
        uids.push(row?);
    }
    Ok(uids)
}

fn node_is_indexable_tx(tx: &Transaction<'_>, uid: &str) -> Result<bool> {
    let indexable: i64 = tx.query_row(
        "WITH RECURSIVE ancestors(uid, parent_uid, trashed, path) AS (
           SELECT uid, parent_uid, trashed, char(31) || uid || char(31)
             FROM nodes WHERE uid = ?1
           UNION ALL
           SELECT n.uid, n.parent_uid, n.trashed,
                  a.path || n.uid || char(31)
             FROM ancestors a JOIN nodes n ON n.uid = a.parent_uid
            WHERE instr(a.path, char(31) || n.uid || char(31)) = 0
         )
         SELECT CASE
           WHEN COUNT(*) = 0 THEN 0
           WHEN MAX(trashed) != 0 THEN 0
           WHEN MAX(CASE WHEN parent_uid IS NULL THEN 1 ELSE 0 END) = 0 THEN 0
           ELSE 1
         END
         FROM ancestors",
        [uid],
        |row| row.get(0),
    )?;
    Ok(indexable == 1)
}

fn tombstone_subtree_tx(tx: &Transaction<'_>, root_uid: &str) -> Result<()> {
    let rows = {
        let mut stmt = tx.prepare(
            "WITH RECURSIVE subtree(rowid, uid, node_json) AS (
               SELECT rowid, uid, node_json FROM nodes WHERE uid = ?1
               UNION
               SELECT n.rowid, n.uid, n.node_json FROM nodes n
                 JOIN subtree s ON n.parent_uid = s.uid
             )
             SELECT rowid, uid, node_json FROM subtree",
        )?;
        let mapped = stmt.query_map([root_uid], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut rows = Vec::new();
        for row in mapped {
            rows.push(row?);
        }
        rows
    };
    for (rowid, uid, json) in rows {
        let json = match json {
            Some(json) => {
                let mut node: Node = serde_json::from_str(&json)?;
                node.trashed = true;
                Some(serde_json::to_string(&node)?)
            }
            None => None,
        };
        tx.execute(
            "UPDATE nodes SET trashed = 1, listed = 0, node_json = ?2 WHERE uid = ?1",
            params![uid, json],
        )?;
        tx.execute("DELETE FROM nodes_fts WHERE rowid = ?1", [rowid])?;
    }
    Ok(())
}

fn upsert_share_access_tx(tx: &Transaction<'_>, uid: &str, access: Access) -> Result<()> {
    tx.execute(
        "INSERT INTO share_access (root_uid, access) VALUES (?1, ?2)
         ON CONFLICT(root_uid) DO UPDATE SET access = excluded.access",
        params![uid, access.as_db_str()],
    )?;
    Ok(())
}

fn parse_node_uid(value: &str) -> Option<NodeUid> {
    let (volume, link) = value.split_once('~')?;
    Some(NodeUid::new(VolumeId::from(volume), LinkId::from(link)))
}

/// Unique quoted character trigrams suitable for an FTS5 OR expression.
pub(super) fn candidate_trigrams(query: &str) -> Vec<String> {
    let chars: Vec<char> = query.to_lowercase().chars().collect();
    let mut terms = Vec::new();
    for window in chars.windows(TRIGRAM_MIN) {
        if window.iter().all(|c| c.is_whitespace()) {
            continue;
        }
        let raw: String = window.iter().collect();
        let quoted = format!("\"{}\"", raw.replace('"', "\"\""));
        if !terms.contains(&quoted) {
            terms.push(quoted);
        }
    }
    terms
}

/// Effective plaintext size of a node for the indexed `size` column: the
/// claimed size when known, else the on-storage size; folders are 0.
pub(super) fn node_size(node: &Node) -> i64 {
    match &node.kind {
        NodeKind::Folder => 0,
        NodeKind::File {
            total_size_on_storage,
            claimed_size,
            ..
        } => claimed_size.unwrap_or(*total_size_on_storage).max(0),
    }
}
