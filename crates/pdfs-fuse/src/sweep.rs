//! Background conflict sweep.
//!
//! A `(sync-conflict <ts>)` copy is only ever a fallback: the drain and the sync
//! engine cut one when they cannot prove a queued write is safe to apply, so the
//! user never loses bytes. Most of those copies turn out to be byte-identical to
//! the live file — a spurious conflict from a re-stamped revision (B16/B25) or a
//! first-sync of an already-identical file. This sweep reconciles them after the
//! fact:
//!
//! - **Identical** to the live sibling (same size *and* same content SHA-1): the
//!   copy is redundant, so it is trashed and logged as a [`ActivityKind::Trash`].
//! - **Divergent** (different bytes) or **orphaned** (no live sibling): left in
//!   place and surfaced once as an [`ActivityKind::Conflict`], because only the
//!   user can decide which version wins.
//!
//! Identity is proved without downloading anything: the SHA-1 comes from the
//! revision's decrypted extended attributes, which enumeration already reads. A
//! copy whose sibling has no recorded SHA-1 is treated as divergent — the sweep
//! never removes a file it cannot prove is a duplicate.

use std::collections::HashMap;
use std::time::Duration;

use pdfs_core::control::ActivityKind;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::{debug, info, warn};

use super::{Core, node_content_sha1, node_size};

/// Time between sweeps. Conflicts are rare and non-urgent; a slow cadence keeps
/// the sweep off the hot path while still cleaning up within a few minutes.
const CONFLICT_SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Delay before the first sweep, so it does not compete with the burst of work a
/// fresh mount does at startup (hydrate, recover, initial enumeration).
const CONFLICT_SWEEP_WARMUP: Duration = Duration::from_secs(30);

/// Strip a ` (sync-conflict <stamp>)` (optionally `-<n>`) marker from `name`,
/// returning the original file name it was a copy of. `None` when `name` is not a
/// conflict copy. Inverse of the `conflict_name` / `conflict_path_with_suffix`
/// formats, so the two halves of the product agree on what a conflict is.
pub(crate) fn conflict_base_name(name: &str) -> Option<String> {
    const MARKER: &str = " (sync-conflict ";
    let start = name.find(MARKER)?;
    let after = start + MARKER.len();
    let close_rel = name[after..].find(')')?;
    let close = after + close_rel;
    let inner = &name[after..close];
    // The token is `<digits>` or `<digits>-<digits>`; reject anything else so a
    // file that merely happens to contain the phrase is not mistaken for a copy.
    let mut parts = inner.splitn(2, '-');
    let stamp = parts.next().unwrap_or_default();
    let suffix = parts.next();
    let is_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !is_digits(stamp) || suffix.is_some_and(|s| !is_digits(s)) {
        return None;
    }
    let mut base = String::with_capacity(start + name.len() - close);
    base.push_str(&name[..start]);
    base.push_str(&name[close + 1..]);
    Some(base)
}

impl Core {
    /// Sweep for stale conflict copies forever, on its own thread. See the module
    /// docs for the identical/divergent policy.
    pub(crate) fn run_conflict_sweep_loop(&self) {
        std::thread::sleep(CONFLICT_SWEEP_WARMUP);
        loop {
            self.sweep_conflicts_once();
            std::thread::sleep(CONFLICT_SWEEP_INTERVAL);
        }
    }

    /// One sweep pass over every node the daemon currently knows.
    fn sweep_conflicts_once(&self) {
        let stored = match self.db.load_all() {
            Ok(s) => s,
            Err(e) => {
                warn!(error = ?e, "conflict sweep: load nodes failed");
                return;
            }
        };
        let nodes: Vec<_> = stored.into_iter().map(|s| s.node).collect();

        // Index live (non-trashed) files by (parent, name) for sibling lookup.
        let mut by_parent_name: HashMap<(String, &str), &_> = HashMap::new();
        for node in &nodes {
            if node.trashed || node.is_folder() {
                continue;
            }
            let parent = node
                .parent_uid
                .as_ref()
                .map(|u| u.to_string())
                .unwrap_or_default();
            by_parent_name.insert((parent, node.name.as_str()), node);
        }

        let online = self.online.load(std::sync::atomic::Ordering::Relaxed);
        for node in &nodes {
            if node.trashed || node.is_folder() {
                continue;
            }
            let Some(base) = conflict_base_name(&node.name) else {
                continue;
            };
            let parent = node
                .parent_uid
                .as_ref()
                .map(|u| u.to_string())
                .unwrap_or_default();
            let sibling = by_parent_name.get(&(parent, base.as_str())).copied();

            match sibling {
                Some(sib) if is_identical(node, sib) => {
                    // Redundant duplicate — remove it, but only while online (the
                    // remote trash is the point). Offline, it waits for a later pass.
                    if online {
                        self.remove_conflict_copy(&node.uid, &node.name, &base);
                    }
                }
                Some(_) => {
                    self.flag_conflict(&node.uid, &node.name, format!("differs from {base}"))
                }
                None => self.flag_conflict(
                    &node.uid,
                    &node.name,
                    format!("no original {base} to reconcile against"),
                ),
            }
        }
    }

    /// Trash a conflict copy proven identical to its live sibling, and unhook it
    /// locally — the same sequence the interactive unlink path uses after a trash.
    fn remove_conflict_copy(&self, uid: &NodeUid, name: &str, base: &str) {
        match self
            .rt
            .block_on(self.client.trash_nodes(std::slice::from_ref(uid)))
        {
            Ok(()) => {}
            Err(e) if super::is_gone(&e) => {}
            Err(e) => {
                warn!(%uid, name, error = %e, "conflict sweep: trashing duplicate failed");
                return;
            }
        }
        self.state.lock().forget_or_unlink(uid);
        self.discard_queued_ops(uid);
        self.cache.evict(uid);
        self.evict_reader(uid);
        if let Err(e) = self.db.delete_node(uid) {
            warn!(%uid, error = ?e, "conflict sweep: db delete after trash failed");
        }
        self.invalidate_trash();
        self.conflict_notified.lock().remove(uid);
        self.log_activity(
            ActivityKind::Trash,
            name,
            format!("auto-removed duplicate of {base}"),
            true,
        );
        info!(%uid, name, base, "conflict sweep: removed identical conflict copy");
    }

    /// Surface a conflict copy that needs the user's attention, at most once per
    /// daemon run so the activity feed is not spammed each pass.
    fn flag_conflict(&self, uid: &NodeUid, name: &str, detail: String) {
        if !self.conflict_notified.lock().insert(uid.clone()) {
            return;
        }
        debug!(%uid, name, detail, "conflict sweep: flagged divergent conflict copy");
        self.log_activity(ActivityKind::Conflict, name, detail, false);
    }
}

/// Whether two file nodes provably hold the same bytes: equal size and equal,
/// present content SHA-1. A missing digest on either side is not proof, so the
/// sweep treats it as divergent rather than risk removing a real difference.
fn is_identical(a: &proton_drive_rs::Node, b: &proton_drive_rs::Node) -> bool {
    if node_size(a) != node_size(b) {
        return false;
    }
    match (node_content_sha1(a), node_content_sha1(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_conflict_names_and_leaves_others_alone() {
        assert_eq!(
            conflict_base_name("test_export (sync-conflict 1784898786).xml").as_deref(),
            Some("test_export.xml")
        );
        assert_eq!(
            conflict_base_name("README (sync-conflict 42)").as_deref(),
            Some("README")
        );
        // The sync engine's de-duplicated suffix form.
        assert_eq!(
            conflict_base_name("notes (sync-conflict 1700-3).txt").as_deref(),
            Some("notes.txt")
        );
        // Not a conflict copy.
        assert_eq!(conflict_base_name("report.xml"), None);
        // Looks close but the token is not a stamp.
        assert_eq!(conflict_base_name("x (sync-conflict abc).txt"), None);
    }

    #[test]
    fn a_dotted_stem_keeps_its_inner_dots() {
        assert_eq!(
            conflict_base_name("archive.tar (sync-conflict 7).gz").as_deref(),
            Some("archive.tar.gz")
        );
    }
}
