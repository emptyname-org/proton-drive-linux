//! Inode bookkeeping: the maps that give a remote node a stable kernel inode,
//! the open write handles, and the interval set that tracks which bytes of a
//! staged write are authored locally.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use pdfs_core::cache::StagedWrite;
use pdfs_core::db::Db;
use pdfs_core::{Access, access_for};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{MemberRole, Node, NodeKind};
use tracing::warn;

/// A node known to the filesystem, addressed by its kernel inode.
pub(crate) struct Entry {
    pub(crate) uid: NodeUid,
    pub(crate) parent: u64,
    pub(crate) node: Node,
    pub(crate) access: Access,
    pub(crate) lookup_count: u64,
    pub(crate) open_count: u32,
    pub(crate) unlinked: bool,
}

impl Entry {
    pub(crate) fn writable(&self) -> bool {
        self.access.writable()
    }
}

/// A set of non-overlapping `[start, end)` byte ranges, kept sorted and merged.
/// Tracks which bytes of a [`WriteHandle`]'s scratch file were authored locally
/// (vs. still living only in the remote base), so reads and the commit gap-fill
/// know which regions to pull from the network.
#[derive(Clone, Default)]
pub(crate) struct Intervals(pub(crate) Vec<(u64, u64)>);

impl Intervals {
    /// Mark `[start, end)` as authored, coalescing with any touching ranges.
    pub(crate) fn add(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        self.0.push((start, end));
        self.0.sort_by_key(|&(s, _)| s);
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.0.len());
        for &(s, e) in &self.0 {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        self.0 = merged;
    }

    /// Drop everything at or beyond `len` (a shrink/truncate).
    pub(crate) fn clip(&mut self, len: u64) {
        self.0.retain(|&(s, _)| s < len);
        for iv in &mut self.0 {
            iv.1 = iv.1.min(len);
        }
    }

    /// Split `[start, end)` into contiguous `(s, e, authored)` segments, in
    /// order. `authored == true` means the bytes live in the scratch file;
    /// `false` means they must come from the remote base (or are a hole).
    pub(crate) fn segments(&self, start: u64, end: u64) -> Vec<(u64, u64, bool)> {
        let mut out = Vec::new();
        let mut pos = start;
        for &(s, e) in &self.0 {
            if e <= start {
                continue;
            }
            if s >= end {
                break;
            }
            let ws = s.max(start);
            let we = e.min(end);
            if pos < ws {
                out.push((pos, ws, false));
            }
            out.push((ws, we, true));
            pos = we;
        }
        if pos < end {
            out.push((pos, end, false));
        }
        out
    }
}

/// State for a file opened for writing. Authored bytes are staged in an on-disk
/// scratch file (positional reads/writes) rather than RAM, so a multi-GiB write
/// never balloons the daemon. On flush/release the scratch file — gap-filled
/// from the remote base where untouched — is streamed up as one new revision,
/// since the SDK seals whole revisions rather than byte ranges.
pub(crate) struct WriteHandle {
    pub(crate) ino: u64,
    pub(crate) uid: NodeUid,
    /// Disk-backed staging buffer. Shared (`Arc`) so reads can use it without
    /// holding the state lock across I/O. Accessed positionally (`read_at`/
    /// `write_at`), so a clone never disturbs another's file offset.
    pub(crate) file: Arc<File>,
    /// Scratch file path, removed on release.
    pub(crate) path: PathBuf,
    /// Byte ranges authored into `file`. Everything else in `[0, len)` is base.
    pub(crate) written: Intervals,
    /// Logical file size (may exceed authored bytes after a truncate-extend).
    pub(crate) len: u64,
    /// Size of the remote base at open, for serving untouched ranges.
    pub(crate) base_size: u64,
    /// Modification time of the remote base at open, validating its block cache.
    pub(crate) base_mtime: i64,
    /// Server revision id of the remote base at open, if it had one. This is the
    /// stable identity the drain conflict-checks against: `base_mtime` alone
    /// drifts when the server re-stamps the *same* revision, which reads as a
    /// spurious conflict (see B16/B25). `None` for a file with no sealed remote
    /// revision, or one read through a surface that does not carry the id.
    pub(crate) base_revision_id: Option<String>,
    /// Whether anything diverged from the remote and needs an upload.
    pub(crate) dirty: bool,
    /// Number of file handles currently sharing this scratch state.
    pub(crate) open_count: usize,
}

/// A released write whose upload has not happened yet (offline.md Phase 3).
///
/// The bytes live in the content cache's staging dir and the intent lives in the
/// `pending_op` table; this pairs them in memory so a read can be served without
/// a database round trip.
#[derive(Clone)]
pub(crate) struct PendingRevision {
    /// Staged blob holding the written bytes.
    pub(crate) path: PathBuf,
    /// Which of those bytes are real, and what base the gaps refer to.
    pub(crate) meta: StagedWrite,
}

/// Mutable inode bookkeeping, guarded by a mutex because fuser drives the
/// `Filesystem` trait through `&self`.
pub(crate) struct State {
    /// inode -> node metadata.
    pub(crate) entries: HashMap<u64, Entry>,
    /// Dedupe inodes by node uid so a node keeps a stable inode across lookups.
    pub(crate) by_uid: HashMap<NodeUid, u64>,
    /// Cached directory listings: parent inode -> child inodes. Presence of a
    /// key means the directory has been enumerated.
    pub(crate) children: HashMap<u64, Vec<u64>>,
    pub(crate) next_ino: u64,
    /// Shared write state keyed by inode. Concurrent writers share the scratch file.
    pub(crate) active_writes: HashMap<u64, WriteHandle>,
    /// Maps file handle id (fh) to inode (ino). Read-only opens use fh 0 and
    /// have no entry here.
    pub(crate) handles: HashMap<u64, u64>,
    pub(crate) next_fh: u64,
    /// Resident inode attributes whose effective access changed during an
    /// intern/root refresh. Core drains this set and notifies the matching
    /// kernel only after releasing the State lock.
    pub(crate) access_changes: HashSet<u64>,
    /// Persisted shared-root authority, loaded once per inode space and updated
    /// on every explicit role/tombstone change. Descendant inheritance reads
    /// this map instead of issuing one SQLite query per interned inode.
    pub(crate) share_access: HashMap<NodeUid, Access>,
    /// Unified SQLite metadata cache. Every map mutation below writes through to
    /// it inside the `State` lock so the DB stays the authoritative copy across
    /// restarts (see plan.md P1).
    pub(crate) db: Arc<Db>,
}

impl State {
    pub(crate) fn require_writable(&self, ino: u64) -> Result<(), fuser::Errno> {
        match self.entries.get(&ino) {
            Some(entry) if entry.writable() => Ok(()),
            Some(_) => Err(fuser::Errno::EACCES),
            None => Err(fuser::Errno::ENOENT),
        }
    }

    pub(crate) fn access_by_uid(&self, uid: &NodeUid) -> Option<Access> {
        self.by_uid
            .get(uid)
            .and_then(|ino| self.entries.get(ino))
            .map(|entry| entry.access)
    }

    pub(crate) fn take_access_changes(&mut self) -> Vec<u64> {
        self.access_changes.drain().collect()
    }

    fn stored_share_access(&self, uid: &NodeUid) -> Option<Access> {
        self.share_access.get(uid).copied()
    }

    pub(crate) fn access_for_node(&mut self, parent: u64, node: &Node) -> Access {
        let parent_access = self
            .entries
            .get(&parent)
            .map_or(Access::Owner, |entry| entry.access);
        let stored = self.stored_share_access(&node.uid);

        // A persisted row is the offline authority. A live membership on a
        // node directly below an owned root identifies a newly discovered
        // share root and refreshes that row. Descendants inherit directly.
        let is_new_share_root = node.membership.is_some() && parent_access == Access::Owner;
        if stored.is_some() || is_new_share_root {
            let access = node.membership.as_ref().map_or_else(
                || stored.unwrap_or(Access::Viewer),
                |membership| access_for(membership.role_exact(), parent_access, true),
            );
            if let Err(error) = self.db.set_share_access(&node.uid, access) {
                warn!(uid = %node.uid, ?access, %error, "db set_share_access failed");
            } else {
                self.share_access.insert(node.uid.clone(), access);
            }
            return access;
        }
        parent_access
    }

    /// Record a role observed by an authority-bearing shared-root surface.
    ///
    /// Ordinary node enumeration, especially `enumerate_nodes_light` and old
    /// cached JSON, may omit membership and must continue to trust persisted
    /// `share_access`. P3 can call this method only when its share-root endpoint
    /// explicitly observed a role; explicit absence or an unknown wire role is
    /// represented by `None` and fails closed to Viewer.
    #[allow(dead_code)] // P3 consumes this explicit-provenance hook.
    pub(crate) fn record_observed_share_root_role(
        &mut self,
        uid: &NodeUid,
        role: Option<MemberRole>,
    ) -> Vec<u64> {
        let (root, parent_access) = match self.by_uid.get(uid).copied() {
            Some(root) => {
                let parent_access = self
                    .entries
                    .get(&root)
                    .and_then(|entry| self.entries.get(&entry.parent))
                    .map_or(Access::Owner, |entry| entry.access);
                (Some(root), parent_access)
            }
            None => (None, Access::Unknown),
        };
        let access = access_for(role, parent_access, true);
        if let Err(error) = self.db.set_share_access(uid, access) {
            warn!(%uid, ?access, %error, "db set observed share access failed");
        } else {
            self.share_access.insert(uid.clone(), access);
        }
        let Some(root) = root else {
            return Vec::new();
        };

        let mut changed = Vec::new();
        if let Some(entry) = self.entries.get_mut(&root)
            && entry.access != access
        {
            entry.access = access;
            changed.push(root);
        }
        changed.extend(self.recompute_descendant_access(root));
        self.access_changes.extend(changed.iter().copied());
        changed
    }

    fn resident_children(&self) -> HashMap<u64, Vec<u64>> {
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        for (&ino, entry) in &self.entries {
            if entry.parent != ino {
                children.entry(entry.parent).or_default().push(ino);
            }
        }
        children
    }

    /// Recompute all resident descendants after a root's effective access
    /// changes. A nested persisted share root remains its own authority.
    fn recompute_descendant_access(&mut self, root: u64) -> Vec<u64> {
        let mut changed = Vec::new();
        let mut queue = VecDeque::from([root]);
        let mut seen = HashSet::new();
        let children = self.resident_children();
        while let Some(parent) = queue.pop_front() {
            if !seen.insert(parent) {
                continue;
            }
            let parent_access = match self.entries.get(&parent) {
                Some(entry) => entry.access,
                None => continue,
            };
            for &child in children.get(&parent).into_iter().flatten() {
                let access = self
                    .entries
                    .get(&child)
                    .and_then(|entry| self.share_access.get(&entry.uid).copied())
                    .unwrap_or(parent_access);
                if let Some(entry) = self.entries.get_mut(&child)
                    && entry.access != access
                {
                    entry.access = access;
                    changed.push(child);
                }
                queue.push_back(child);
            }
        }
        changed
    }

    /// Force a known shared root and every resident descendant read-only.
    ///
    /// This intentionally ignores nested authorities: losing access to an
    /// outer tree must not leave a cached inner entry writable.
    pub(crate) fn downgrade_shared_subtree(&mut self, uid: &NodeUid) -> Vec<u64> {
        self.share_access.insert(uid.clone(), Access::Viewer);
        let Some(&root) = self.by_uid.get(uid) else {
            return Vec::new();
        };
        let mut changed = Vec::new();
        let mut queue = VecDeque::from([root]);
        let mut seen = HashSet::new();
        let children = self.resident_children();
        while let Some(ino) = queue.pop_front() {
            if !seen.insert(ino) {
                continue;
            }
            if let Some(entry) = self.entries.get_mut(&ino)
                && entry.access != Access::Viewer
            {
                entry.access = Access::Viewer;
                changed.push(ino);
            }
            queue.extend(children.get(&ino).into_iter().flatten().copied());
        }
        changed
    }

    /// Downgrade resident shared subtrees identified by persisted authority,
    /// live membership, or already-inherited non-owner access. Owned and device
    /// entries remain `Owner`.
    pub(crate) fn downgrade_known_shared_access(&mut self) -> Vec<u64> {
        for access in self.share_access.values_mut() {
            *access = Access::Viewer;
        }
        let roots: Vec<u64> = self
            .entries
            .iter()
            .filter(|entry| {
                let (_, entry) = entry;
                entry.access != Access::Owner
                    || entry.node.membership.is_some()
                    || self.share_access.contains_key(&entry.uid)
            })
            .map(|(&ino, _)| ino)
            .collect();
        let children = self.resident_children();
        let mut queue = VecDeque::from(roots);
        let mut seen = HashSet::new();
        let mut changed = Vec::new();
        while let Some(ino) = queue.pop_front() {
            if !seen.insert(ino) {
                continue;
            }
            if let Some(entry) = self.entries.get_mut(&ino)
                && entry.access != Access::Viewer
            {
                entry.access = Access::Viewer;
                changed.push(ino);
            }
            queue.extend(children.get(&ino).into_iter().flatten().copied());
        }
        changed
    }

    /// Rebuild effective access after cold hydration. The memo makes each
    /// parent edge resolve once, then every child is a direct inheritance.
    pub(crate) fn hydrate_access(&mut self) {
        fn resolve(
            state: &State,
            ino: u64,
            memo: &mut HashMap<u64, Access>,
            visiting: &mut HashSet<u64>,
        ) -> Access {
            if let Some(access) = memo.get(&ino) {
                return *access;
            }
            let Some(entry) = state.entries.get(&ino) else {
                return Access::Unknown;
            };
            if !visiting.insert(ino) {
                return Access::Unknown;
            }
            let stored = state.share_access.get(&entry.uid).copied();
            let access = if let Some(stored) = stored {
                stored
            } else if entry.parent == ino {
                Access::Owner
            } else if !state.entries.contains_key(&entry.parent) {
                entry
                    .node
                    .parent_uid
                    .as_ref()
                    .and_then(|parent_uid| state.share_access.get(parent_uid).copied())
                    .unwrap_or(Access::Unknown)
            } else {
                let parent_access = resolve(state, entry.parent, memo, visiting);
                if entry.node.membership.is_none() || parent_access != Access::Owner {
                    parent_access
                } else {
                    access_for(
                        entry
                            .node
                            .membership
                            .as_ref()
                            .and_then(|membership| membership.role_exact()),
                        parent_access,
                        true,
                    )
                }
            };
            visiting.remove(&ino);
            memo.insert(ino, access);
            access
        }

        let mut memo = HashMap::new();
        let mut visiting = HashSet::new();
        for ino in self.entries.keys().copied().collect::<Vec<_>>() {
            resolve(self, ino, &mut memo, &mut visiting);
        }
        for (ino, access) in memo {
            if let Some(entry) = self.entries.get_mut(&ino) {
                entry.access = access;
            }
        }
        let discovered: Vec<(NodeUid, Access)> = self
            .entries
            .values()
            .filter_map(|entry| {
                let parent_access = self
                    .entries
                    .get(&entry.parent)
                    .map_or(Access::Unknown, |parent| parent.access);
                (entry.node.membership.is_some()
                    && parent_access == Access::Owner
                    && !self.share_access.contains_key(&entry.uid))
                .then(|| {
                    (
                        entry.uid.clone(),
                        access_for(
                            entry
                                .node
                                .membership
                                .as_ref()
                                .and_then(|membership| membership.role_exact()),
                            parent_access,
                            true,
                        ),
                    )
                })
            })
            .collect();
        for (uid, access) in discovered {
            if let Err(error) = self.db.set_share_access(&uid, access) {
                warn!(%uid, ?access, %error, "db set hydrated share access failed");
            } else {
                self.share_access.insert(uid, access);
            }
        }
    }

    pub(crate) fn intern(&mut self, parent: u64, node: Node) -> u64 {
        if let Err(e) = self.db.upsert_node(&node) {
            warn!(uid = %node.uid, error = %e, "db upsert_node failed");
        }
        self.intern_mem(parent, node)
    }

    /// Allocate (or reuse) a stable inode for a node, updating the hot-cache maps
    /// only. Every caller owes the DB a write-through — see the callers below;
    /// the split exists so a batch can pay for one transaction instead of `n`.
    pub(crate) fn intern_mem(&mut self, parent: u64, node: Node) -> u64 {
        if let Some(&ino) = self.by_uid.get(&node.uid) {
            let access = self.access_for_node(parent, &node);
            let changed = self
                .entries
                .get(&ino)
                .is_some_and(|entry| entry.access != access);
            if let Some(e) = self.entries.get_mut(&ino) {
                e.node = node;
                e.parent = parent;
                e.access = access;
            }
            if changed {
                self.access_changes.insert(ino);
                let descendants = self.recompute_descendant_access(ino);
                self.access_changes.extend(descendants);
            }
            return ino;
        }
        let access = self.access_for_node(parent, &node);
        let ino = self.next_ino;
        self.next_ino += 1;
        self.by_uid.insert(node.uid.clone(), ino);
        self.entries.insert(
            ino,
            Entry {
                uid: node.uid.clone(),
                parent,
                node,
                access,
                lookup_count: 1,
                open_count: 0,
                unlinked: false,
            },
        );
        ino
    }

    /// Decrement lookup count for an inode and prune if lookup_count == 0 && open_count == 0 && unlinked.
    pub(crate) fn forget_lookup(&mut self, ino: u64, nlookup: u64) -> Option<(u64, String)> {
        if let Some(entry) = self.entries.get_mut(&ino) {
            entry.lookup_count = entry.lookup_count.saturating_sub(nlookup);
            if entry.lookup_count == 0 && entry.open_count == 0 && entry.unlinked {
                let uid = entry.uid.clone();
                return self.forget(&uid);
            }
        }
        None
    }

    /// Allocate (or reuse) a stable inode for a node that came *from* the
    /// database, which is why nothing is written back.
    pub(crate) fn intern_from_db(&mut self, parent: u64, node: Node) -> u64 {
        self.intern_mem(parent, node)
    }

    /// Allocate (or reuse) stable inodes for a whole listing, writing every node
    /// through in a single transaction.
    ///
    /// This is what keeps `ls` on a large folder quick: one commit for the
    /// listing, rather than one autocommit — and one fsync — per child.
    pub(crate) fn intern_batch(&mut self, parent: u64, nodes: Vec<Node>) -> Vec<u64> {
        if let Err(e) = self.db.upsert_nodes(&nodes) {
            warn!(error = %e, "db upsert_nodes failed");
        }
        nodes
            .into_iter()
            .map(|node| self.intern_mem(parent, node))
            .collect()
    }

    /// Check if a directory inode has any child nodes in memory or in the database.
    pub(crate) fn has_children(&self, parent: u64) -> bool {
        // A resident listing is authoritative even when empty. Falling through
        // from `Some([])` to SQLite lets an obsolete child row contradict the
        // listing `ensure_children` just established, so readdir says empty but
        // rmdir returns ENOTEMPTY forever.
        if let Some(kids) = self.children.get(&parent) {
            return !kids.is_empty();
        }
        if let Some(entry) = self.entries.get(&parent)
            && let Ok(has_kids) = self.db.has_children(&entry.uid)
        {
            return has_kids;
        }
        false
    }

    /// Check if `ancestor_ino` is `target_ino` or an ancestor of `target_ino` (for rename cycle prevention).
    pub(crate) fn is_ancestor_of(&self, ancestor_ino: u64, mut target_ino: u64) -> bool {
        if ancestor_ino == target_ino {
            return true;
        }
        let mut visited = std::collections::HashSet::new();
        visited.insert(target_ino);
        while let Some(entry) = self.entries.get(&target_ino) {
            let parent = entry.parent;
            if parent == ancestor_ino {
                return true;
            }
            if parent == 0 || !visited.insert(parent) {
                break;
            }
            target_ino = parent;
        }
        false
    }

    /// Forget a node or, if open handles exist (open_count > 0), mark it unlinked
    /// and remove it from parent children so lookups fail while open reads succeed.
    pub(crate) fn forget_or_unlink(&mut self, uid: &NodeUid) -> Option<(u64, String)> {
        if let Some(&ino) = self.by_uid.get(uid)
            && let Some(entry) = self.entries.get_mut(&ino)
            && entry.open_count > 0
        {
            entry.unlinked = true;
            if let Some(kids) = self.children.get_mut(&entry.parent) {
                kids.retain(|&k| k != ino);
            }
            return Some((entry.parent, entry.node.name.clone()));
        }
        self.forget(uid)
    }

    /// Remove a dentry while retaining both open inode state and the persisted
    /// node row. Queued trash needs the row as drain-time authority; if the node
    /// is open, POSIX also requires its inode to survive until the final close.
    pub(crate) fn unlink_mem(&mut self, uid: &NodeUid) -> Option<(u64, String)> {
        if let Some(&ino) = self.by_uid.get(uid)
            && let Some(entry) = self.entries.get_mut(&ino)
            && entry.open_count > 0
        {
            entry.unlinked = true;
            if let Some(kids) = self.children.get_mut(&entry.parent) {
                kids.retain(|&kid| kid != ino);
            }
            return Some((entry.parent, entry.node.name.clone()));
        }
        self.forget_mem(uid)
    }

    /// Forget a node entirely: drop its inode, its uid mapping, its own cached
    /// listing, its slot in its parent's listing, and its DB row. Returns
    /// `(parent_ino, name)` when the node was known, so the caller can notify
    /// the kernel.
    pub(crate) fn forget(&mut self, uid: &NodeUid) -> Option<(u64, String)> {
        if let Err(e) = self.db.delete_node(uid) {
            warn!(%uid, error = %e, "db delete_node failed");
        }
        self.forget_mem(uid)
    }

    /// Forget a resident node while retaining its persisted row.
    ///
    /// A queued trash uses the row as its drain-time access authority and only
    /// deletes it after the remote mutation lands.
    pub(crate) fn forget_mem(&mut self, uid: &NodeUid) -> Option<(u64, String)> {
        let ino = self.by_uid.remove(uid)?;
        let entry = self.entries.remove(&ino)?;
        self.children.remove(&ino);
        if let Some(kids) = self.children.get_mut(&entry.parent) {
            kids.retain(|&k| k != ino);
        }
        Some((entry.parent, entry.node.name))
    }

    /// Move a node to a new parent and/or name within the tree, writing it
    /// through like any other mutation.
    ///
    /// The online rename instead forgets the node and lets the destination
    /// re-enumerate, which is the cheaper way to stay honest about what the
    /// server did. A queued rename cannot: re-enumerating needs the network, and
    /// the server has not been told yet in any case — so this *is* the tree's
    /// new truth until the op drains (offline.md Phase 3b).
    pub(crate) fn rename_in_place(
        &mut self,
        ino: u64,
        new_parent: u64,
        new_parent_uid: &NodeUid,
        name: &str,
    ) {
        let parent_access = self
            .entries
            .get(&new_parent)
            .map_or(Access::Owner, |entry| entry.access);
        let access = self
            .entries
            .get(&ino)
            .and_then(|entry| self.share_access.get(&entry.uid).copied())
            .unwrap_or(parent_access);
        let Some(entry) = self.entries.get_mut(&ino) else {
            return;
        };
        let old_parent = entry.parent;
        entry.parent = new_parent;
        entry.access = access;
        entry.node.name = name.to_string();
        entry.node.parent_uid = Some(new_parent_uid.clone());
        let node = entry.node.clone();
        if old_parent != new_parent {
            if let Some(kids) = self.children.get_mut(&old_parent) {
                kids.retain(|&k| k != ino);
            }
            // Only if the destination is listed: pushing into a listing that was
            // never enumerated would invent a one-child folder.
            if let Some(kids) = self.children.get_mut(&new_parent)
                && !kids.contains(&ino)
            {
                kids.push(ino);
            }
        }
        if let Err(e) = self.db.upsert_node(&node) {
            warn!(uid = %node.uid, error = %e, "db upsert_node failed for a queued rename");
        }
        self.recompute_descendant_access(ino);
    }

    /// Drop a directory's cached child listing and mark it unlisted in the DB,
    /// so the next access re-enumerates instead of trusting a stale listing.
    ///
    /// The DB flag is cleared whether or not the listing was resident. A folder
    /// trimmed from the hot cache but still `listed` in the DB is exactly the
    /// case that needs invalidating — returning early there would leave
    /// `ensure_children` free to rebuild the stale listing from disk.
    pub(crate) fn invalidate_listing(&mut self, ino: u64) {
        self.children.remove(&ino);
        if let Some(e) = self.entries.get(&ino) {
            let uid = e.uid.clone();
            if let Err(err) = self.db.set_listed(&uid, false) {
                warn!(%uid, error = %err, "db set_listed(false) failed");
            }
        }
    }

    /// Settle the local state after a node has been moved (and possibly renamed)
    /// on the remote: rewrite it in place, and drop **both** directories'
    /// listings so each re-enumerates.
    ///
    /// Both, because each is stale for its own reason. The destination has
    /// gained a child it does not know about; the source has lost one. The
    /// source is the subtler half, and was audit A5: pruning the moved node from
    /// the source's in-memory children looks sufficient while that entry stays
    /// resident, but the source's DB row is left `listed = 1`. Once it is
    /// evicted from the hot cache, or the daemon restarts, `ensure_children`
    /// rebuilds the listing from the DB and declares it complete, so anything
    /// else that changed remotely under the source since is never seen.
    ///
    /// What this must *not* do is `forget` the node. Forgetting drops its
    /// `by_uid` mapping, so the re-enumeration hands it a fresh inode — while
    /// the kernel has already carried the renamed dentry over to the *old* one.
    /// Every lookup through that dentry then resolves to an inode `entries` no
    /// longer holds and fails `ENOENT`, so the renamed directory reads as
    /// missing (`ls` on it errors while `ls` of its parent lists it) until the
    /// entry TTL expires. Keeping the inode keeps the kernel's dentry valid.
    ///
    /// A pure rename (`from == to`) invalidates that one directory once.
    pub(crate) fn relocate(
        &mut self,
        ino: u64,
        from_parent: u64,
        to_parent: u64,
        to_parent_uid: &NodeUid,
        name: &str,
    ) {
        self.rename_in_place(ino, to_parent, to_parent_uid, name);
        self.invalidate_listing(to_parent);
        if from_parent != to_parent {
            self.invalidate_listing(from_parent);
        }
    }

    /// Update a file entry's recorded plaintext size so `getattr` reflects an
    /// in-progress write before the new revision is sealed.
    pub(crate) fn set_size(&mut self, ino: u64, size: u64) {
        if let Some(e) = self.entries.get_mut(&ino)
            && let NodeKind::File { claimed_size, .. } = &mut e.node.kind
        {
            *claimed_size = Some(size as i64);
        }
    }

    /// Update a file entry's modification time (epoch seconds).
    pub(crate) fn touch_mtime(&mut self, ino: u64, secs: i64) {
        if let Some(e) = self.entries.get_mut(&ino) {
            e.node.modification_time = secs;
        }
    }

    /// Record the size and mtime of a write that has been accepted but not yet
    /// uploaded, persisting them like any other node mutation.
    ///
    /// The write-through is what makes the new size outlive the process: until
    /// the op drains, the remote still holds the old revision (or, for a node
    /// created offline, nothing at all), so this row is the only record that the
    /// file is as long as the caller was told it is. Without it a restart serves
    /// the stale size and the file reads as truncated — or empty — while its
    /// bytes sit safely in staging (offline.md Phase 3).
    pub(crate) fn record_pending_write(&mut self, ino: u64, size: u64, mtime: i64) {
        self.set_size(ino, size);
        self.touch_mtime(ino, mtime);
        if let Some(e) = self.entries.get(&ino)
            && let Err(err) = self.db.upsert_node(&e.node)
        {
            warn!(uid = %e.uid, error = %err, "db upsert_node failed for a queued write");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proton_drive_rs::ShareMembership;
    use proton_drive_rs::proton_sdk::ids::{LinkId, ShareId, ShareMembershipId, VolumeId};

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    fn node(link: &str, parent: &str, name: &str, is_dir: bool) -> Node {
        Node {
            uid: uid(link),
            parent_uid: Some(uid(parent)),
            kind: if is_dir {
                NodeKind::Folder
            } else {
                NodeKind::File {
                    media_type: "text/plain".into(),
                    total_size_on_storage: 0,
                    active_revision_state: None,
                    active_revision_id: None,
                    content_sha1: None,
                    claimed_size: Some(0),
                    claimed_modification_time: None,
                }
            },
            name: name.into(),
            creation_time: 100,
            modification_time: 100,
            trashed: false,
            is_shared: false,
            is_shared_publicly: false,
            signature_email: None,
            membership: None,
            verification: Default::default(),
        }
    }

    fn membership(permissions: i32) -> ShareMembership {
        ShareMembership {
            share_id: ShareId::from("share"),
            membership_id: ShareMembershipId::from("membership"),
            permissions,
        }
    }

    /// A unique temp directory removed on drop; avoids a dev-dependency.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "pdfs-state-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `Db::open_in_memory` is `#[cfg(test)]` inside `pdfs-core`, so it does not
    /// exist for this crate's tests — a temp file is the equivalent here. The
    /// directory outlives the state because the state holds the open database.
    fn state() -> (State, TempDir) {
        let dir = TempDir::new();
        let db = Db::open(&dir.0.join("cache.db")).unwrap();
        let share_access = db.all_share_access().unwrap();
        let st = State {
            entries: HashMap::new(),
            by_uid: HashMap::new(),
            children: HashMap::new(),
            next_ino: 1,
            active_writes: HashMap::new(),
            handles: HashMap::new(),
            next_fh: 1,
            access_changes: HashSet::new(),
            share_access,
            db: Arc::new(db),
        };
        (st, dir)
    }

    /// Two folders, each holding one file, both listed — the state a move
    /// starts from. Returns `(state, src_ino, dst_ino)`.
    fn two_folders() -> (State, TempDir, u64, u64) {
        let (mut st, dir) = state();
        let src = st.intern(0, node("src", "root", "src", true));
        let dst = st.intern(0, node("dst", "root", "dst", true));
        let f = st.intern(src, node("f", "src", "f.txt", false));
        st.children.insert(src, vec![f]);
        st.children.insert(dst, vec![]);
        st.db.set_listed(&uid("src"), true).unwrap();
        st.db.set_listed(&uid("dst"), true).unwrap();
        (st, dir, src, dst)
    }

    /// Audit A5. Moving a file out of a folder leaves that folder's DB row
    /// claiming a complete listing, so once the hot cache drops it, remote
    /// changes under it stop being seen. `relocate` has to clear both sides.
    #[test]
    fn relocate_invalidates_the_source_as_well_as_the_destination() {
        let (mut st, _dir, src, dst) = two_folders();
        let f = st.by_uid[&uid("f")];
        st.relocate(f, src, dst, &uid("dst"), "f.txt");

        assert!(
            st.db.children_if_listed(&uid("dst")).unwrap().is_none(),
            "destination re-enumerates: it gained a child whose row was just deleted"
        );
        assert!(
            st.db.children_if_listed(&uid("src")).unwrap().is_none(),
            "source re-enumerates: its listing predates the move, and a stale \
             `listed` flag would hide every later remote change under it"
        );
        assert!(!st.children.contains_key(&src));
        assert!(!st.children.contains_key(&dst));
        assert_eq!(
            st.by_uid.get(&uid("f")),
            Some(&f),
            "the inode survives the move: the kernel has already pointed the \
             renamed dentry at it, so re-interning under a fresh one would make \
             every lookup through that dentry ENOENT"
        );
        assert_eq!(st.entries[&f].parent, dst);
    }

    /// A pure rename never leaves the directory, so there is one listing to
    /// drop, not two — and dropping it must not depend on the parents differing.
    #[test]
    fn relocate_within_one_directory_still_invalidates_it() {
        let (mut st, _dir, src, _dst) = two_folders();
        let f = st.by_uid[&uid("f")];
        st.relocate(f, src, src, &uid("src"), "renamed.txt");
        assert!(st.db.children_if_listed(&uid("src")).unwrap().is_none());
        assert!(!st.children.contains_key(&src));
        assert_eq!(st.by_uid.get(&uid("f")), Some(&f), "the inode is stable");
        assert_eq!(st.entries[&f].node.name, "renamed.txt");
    }

    /// The failure mode the fix exists for, stated directly: forgetting the node
    /// prunes it from the resident listing but leaves the DB claiming the source
    /// is fully enumerated. This pins *why* `relocate` cannot just call `forget`.
    #[test]
    fn forget_alone_leaves_the_source_claiming_a_complete_listing() {
        let (mut st, _dir, _src, _dst) = two_folders();
        st.forget(&uid("f"));
        let listed = st.db.children_if_listed(&uid("src")).unwrap();
        assert!(
            listed.is_some(),
            "forget does not clear the flag — which is exactly why relocate must"
        );
        assert!(
            listed.unwrap().is_empty(),
            "and the listing it would serve is the moved file's absence, \
             with no way to notice anything else changed"
        );
    }

    #[test]
    fn test_is_ancestor_of_hierarchy() {
        let (mut st, _dir) = state();
        let root = st.intern(0, node("root_id", "none", "root", true));
        let p1 = st.intern(root, node("p1_id", "root_id", "p1", true));
        let p2 = st.intern(p1, node("p2_id", "p1_id", "p2", true));
        let child = st.intern(p2, node("child_id", "p2_id", "child", true));

        assert!(st.is_ancestor_of(root, root), "self is ancestor");
        assert!(st.is_ancestor_of(root, p1), "root is ancestor of p1");
        assert!(
            st.is_ancestor_of(root, child),
            "root is ancestor of deep child"
        );
        assert!(st.is_ancestor_of(p1, child), "p1 is ancestor of deep child");
        assert!(
            !st.is_ancestor_of(child, root),
            "child is not ancestor of root"
        );
        assert!(!st.is_ancestor_of(child, p1), "child is not ancestor of p1");
    }

    #[test]
    fn test_state_has_children_mem_and_db() {
        let (mut st, _dir) = state();
        let folder = st.intern_mem(0, node("dir_uid", "none", "dir", true));
        assert!(!st.has_children(folder), "initially empty");

        // Memory-only children (using intern_mem so DB is not populated yet)
        let f1 = st.intern_mem(folder, node("file1_uid", "dir_uid", "file1.txt", false));
        st.children.insert(folder, vec![f1]);
        assert!(st.has_children(folder), "has child in memory");

        st.children.insert(folder, vec![]);
        assert!(!st.has_children(folder), "cleared memory children");

        // DB children
        st.db
            .upsert_node(&node("file2_uid", "dir_uid", "file2.txt", false))
            .unwrap();
        assert!(
            !st.has_children(folder),
            "a resident empty listing overrides stale DB children"
        );
        st.children.remove(&folder);
        assert!(st.has_children(folder), "has child in db");
    }

    #[test]
    fn share_root_access_is_persisted_and_children_inherit_in_constant_time() {
        let (mut st, _dir) = state();
        let owned_root = st.intern(0, node("root", "none", "My Files", true));
        let mut shared = node("shared", "root", "Shared", true);
        shared.membership = Some(membership(6));
        let shared_ino = st.intern(owned_root, shared);
        let child = st.intern(
            shared_ino,
            node("shared-child", "shared", "child.txt", false),
        );

        assert_eq!(st.entries[&shared_ino].access, Access::Editor);
        assert_eq!(st.entries[&child].access, Access::Editor);
        assert_eq!(
            st.db.share_access(&uid("shared")).unwrap(),
            Some(Access::Editor)
        );
    }

    #[test]
    fn persisted_share_access_controls_offline_hydration() {
        let (mut st, _dir) = state();
        let root_node = node("root", "none", "My Files", true);
        let shared_node = node("offline-share", "root", "Offline share", true);
        let child_node = node("offline-child", "offline-share", "nested folder", true);
        let leaf_node = node("offline-leaf", "offline-child", "child.txt", false);
        for node in [&root_node, &shared_node, &child_node, &leaf_node] {
            st.db.upsert_node(node).unwrap();
        }
        st.db
            .set_share_access(&uid("offline-share"), Access::Viewer)
            .unwrap();
        st.share_access = st.db.all_share_access().unwrap();

        // Reconstruct in deliberately non-topological order, matching the
        // database hydration path that first allocates every inode and only
        // then materializes entries.
        let owned_root = 1;
        let shared = 2;
        let child = 3;
        let leaf = 4;
        for (node, ino) in [
            (&root_node, owned_root),
            (&shared_node, shared),
            (&child_node, child),
            (&leaf_node, leaf),
        ] {
            st.by_uid.insert(node.uid.clone(), ino);
        }
        for (ino, parent, node) in [
            (leaf, child, leaf_node),
            (child, shared, child_node),
            (shared, owned_root, shared_node),
            (owned_root, owned_root, root_node),
        ] {
            st.entries.insert(
                ino,
                Entry {
                    uid: node.uid.clone(),
                    parent,
                    node,
                    access: Access::Unknown,
                    lookup_count: 1,
                    open_count: 0,
                    unlinked: false,
                },
            );
        }
        st.next_ino = 5;
        st.hydrate_access();

        assert_eq!(st.entries[&owned_root].access, Access::Owner);
        assert_eq!(st.entries[&shared].access, Access::Viewer);
        assert_eq!(st.entries[&child].access, Access::Viewer);
        assert_eq!(st.entries[&leaf].access, Access::Viewer);
        assert_eq!(
            st.require_writable(shared).unwrap_err().code(),
            libc::EACCES
        );
        assert_eq!(st.require_writable(child).unwrap_err().code(), libc::EACCES);
        assert_eq!(st.require_writable(leaf).unwrap_err().code(), libc::EACCES);
        assert!(st.require_writable(owned_root).is_ok());
    }

    #[test]
    fn reinterned_share_root_propagates_a_viewer_downgrade() {
        let (mut st, _dir) = state();
        let owned_root = st.intern(0, node("root", "none", "My Files", true));
        let mut shared_node = node("shared", "root", "Shared", true);
        shared_node.membership = Some(membership(6));
        let shared = st.intern(owned_root, shared_node.clone());
        let child = st.intern(shared, node("child", "shared", "folder", true));
        let leaf = st.intern(child, node("leaf", "child", "leaf.txt", false));
        assert_eq!(st.entries[&leaf].access, Access::Editor);

        shared_node.membership = Some(membership(4));
        assert_eq!(st.intern(owned_root, shared_node), shared);

        for ino in [shared, child, leaf] {
            assert_eq!(st.entries[&ino].access, Access::Viewer);
            assert_eq!(st.require_writable(ino).unwrap_err().code(), libc::EACCES);
        }
        assert_eq!(
            st.db.share_access(&uid("shared")).unwrap(),
            Some(Access::Viewer)
        );
    }

    #[test]
    fn missing_light_membership_keeps_persisted_share_authority() {
        for persisted in [Access::Viewer, Access::Editor] {
            let (mut st, _dir) = state();
            let owned_root = st.intern(0, node("root", "none", "My Files", true));
            st.db.set_share_access(&uid("shared"), persisted).unwrap();
            st.share_access = st.db.all_share_access().unwrap();
            let shared = st.intern(
                owned_root,
                node("shared", "root", "Shared without membership", true),
            );
            let child = st.intern(
                shared,
                node("child", "shared", "Light child without membership", false),
            );

            assert_eq!(st.entries[&shared].access, persisted);
            assert_eq!(st.entries[&child].access, persisted);
            assert_eq!(st.db.share_access(&uid("shared")).unwrap(), Some(persisted));
        }
    }

    #[test]
    fn explicit_missing_share_role_fails_closed_and_marks_kernel_attrs() {
        let (mut st, _dir) = state();
        let owned_root = st.intern(0, node("root", "none", "My Files", true));
        st.db
            .set_share_access(&uid("shared"), Access::Editor)
            .unwrap();
        let shared = st.intern(owned_root, node("shared", "root", "Shared", true));
        let child = st.intern(shared, node("child", "shared", "child.txt", false));
        st.take_access_changes();

        let changed = st.record_observed_share_root_role(&uid("shared"), None);

        assert_eq!(st.entries[&shared].access, Access::Viewer);
        assert_eq!(st.entries[&child].access, Access::Viewer);
        assert_eq!(
            st.db.share_access(&uid("shared")).unwrap(),
            Some(Access::Viewer)
        );
        assert!(changed.contains(&shared));
        assert!(changed.contains(&child));
        let pending = st.take_access_changes();
        assert!(pending.contains(&shared));
        assert!(pending.contains(&child));
    }

    #[test]
    fn restored_editor_access_marks_root_and_descendants_for_invalidation() {
        let (mut st, _dir) = state();
        let owned_root = st.intern(0, node("root", "none", "My Files", true));
        let mut shared_node = node("shared", "root", "Shared", true);
        shared_node.membership = Some(membership(4));
        let shared = st.intern(owned_root, shared_node.clone());
        let child = st.intern(shared, node("child", "shared", "child.txt", false));
        st.take_access_changes();

        shared_node.membership = Some(membership(6));
        assert_eq!(st.intern(owned_root, shared_node), shared);

        assert_eq!(st.entries[&shared].access, Access::Editor);
        assert_eq!(st.entries[&child].access, Access::Editor);
        let changed = st.take_access_changes();
        assert!(changed.contains(&shared));
        assert!(changed.contains(&child));
    }

    #[test]
    fn revoked_root_tombstone_keeps_resident_and_persisted_descendants_read_only() {
        let (mut st, _dir) = state();
        let owned_root = st.intern(0, node("root", "none", "My Files", true));
        let mut shared_node = node("shared", "root", "Shared", true);
        shared_node.membership = Some(membership(6));
        let shared = st.intern(owned_root, shared_node);
        let child = st.intern(shared, node("child", "shared", "folder", true));
        let leaf = st.intern(child, node("leaf", "child", "leaf.txt", false));

        st.db
            .set_share_access(&uid("shared"), Access::Viewer)
            .unwrap();
        let changed = st.downgrade_shared_subtree(&uid("shared"));
        assert!(changed.contains(&shared));
        assert!(changed.contains(&child));
        assert!(changed.contains(&leaf));
        st.forget(&uid("shared"));

        assert!(!st.entries.contains_key(&shared));
        assert_eq!(st.entries[&child].access, Access::Viewer);
        assert_eq!(st.entries[&leaf].access, Access::Viewer);
        assert_eq!(st.require_writable(leaf).unwrap_err().code(), libc::EACCES);
        assert_eq!(
            st.db.effective_node_access(&uid("leaf")).unwrap(),
            Some(Access::Viewer)
        );

        // Restart-shaped reconstruction: the deleted root is absent from the
        // inode map, so its direct child is attached to the orphan inode. Cold
        // hydration must still follow the persisted parent UID to the retained
        // share_access tombstone.
        let child_node = st.entries[&child].node.clone();
        let leaf_node = st.entries[&leaf].node.clone();
        st.entries.clear();
        st.by_uid.clear();
        st.children.clear();
        st.by_uid.insert(child_node.uid.clone(), child);
        st.by_uid.insert(leaf_node.uid.clone(), leaf);
        st.entries.insert(
            leaf,
            Entry {
                uid: leaf_node.uid.clone(),
                parent: child,
                node: leaf_node,
                access: Access::Unknown,
                lookup_count: 1,
                open_count: 0,
                unlinked: false,
            },
        );
        st.entries.insert(
            child,
            Entry {
                uid: child_node.uid.clone(),
                parent: 0,
                node: child_node,
                access: Access::Unknown,
                lookup_count: 1,
                open_count: 0,
                unlinked: false,
            },
        );
        st.hydrate_access();

        assert_eq!(st.entries[&child].access, Access::Viewer);
        assert_eq!(st.entries[&leaf].access, Access::Viewer);
        assert_eq!(st.require_writable(leaf).unwrap_err().code(), libc::EACCES);
    }

    #[test]
    fn global_access_loss_downgrades_only_recorded_shared_subtrees() {
        let (mut st, _dir) = state();
        let owned_root = st.intern(0, node("root", "none", "My Files", true));
        let owned_child = st.intern(owned_root, node("owned", "root", "Owned", true));
        let mut shared_node = node("shared", "root", "Shared", true);
        shared_node.membership = Some(membership(6));
        let shared = st.intern(owned_root, shared_node);
        let shared_child = st.intern(shared, node("child", "shared", "child.txt", false));

        st.db.downgrade_all_share_access().unwrap();
        let changed = st.downgrade_known_shared_access();

        assert!(changed.contains(&shared));
        assert!(changed.contains(&shared_child));
        assert_eq!(st.entries[&shared].access, Access::Viewer);
        assert_eq!(st.entries[&shared_child].access, Access::Viewer);
        assert_eq!(st.entries[&owned_root].access, Access::Owner);
        assert_eq!(st.entries[&owned_child].access, Access::Owner);
    }

    #[test]
    fn global_access_loss_visits_a_deep_shared_tree_once_per_changed_inode() {
        let (mut st, _dir) = state();
        let owned_root = st.intern(0, node("root", "none", "My Files", true));
        let mut shared_node = node("shared", "root", "Shared", true);
        shared_node.membership = Some(membership(6));
        let shared = st.intern(owned_root, shared_node);
        let mut parent = shared;
        for index in 0..1_024 {
            let link = format!("deep-{index}");
            let parent_link = st.entries[&parent].uid.link_id.to_string();
            parent = st.intern(parent, node(&link, &parent_link, &link, true));
        }
        st.db.downgrade_all_share_access().unwrap();

        let changed = st.downgrade_known_shared_access();

        let unique: HashSet<u64> = changed.iter().copied().collect();
        assert_eq!(changed.len(), 1_025);
        assert_eq!(unique.len(), changed.len());
        assert_eq!(st.entries[&owned_root].access, Access::Owner);
        assert!(
            st.entries
                .values()
                .all(|entry| { entry.uid == uid("root") || entry.access == Access::Viewer })
        );
    }

    #[test]
    fn unknown_membership_permissions_fail_closed() {
        let (mut st, _dir) = state();
        let owned_root = st.intern(0, node("root", "none", "My Files", true));
        let mut shared = node("shared", "root", "Shared", true);
        shared.membership = Some(membership(38));
        let shared = st.intern(owned_root, shared);
        assert_eq!(st.entries[&shared].access, Access::Viewer);
    }
}
