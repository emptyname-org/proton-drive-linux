//! Remote-event invalidation and local-index background workers.

use super::*;

/// Drop `uid` from one mount's tree and tell that mount's kernel session the
/// entry is gone. Shared by the trash and delete paths, which differ only in
/// which event carried them.
///
/// Returns whether this mount actually held the node, so the caller can evict
/// its content blob once if *any* mount did.
fn forget_and_notify(st: &mut State, notifier: Option<&Notifier>, uid: &NodeUid) -> bool {
    // Capture the inode before `forget` clears the uid mapping.
    let child = st.by_uid.get(uid).copied();
    let Some((parent, name)) = st.forget(uid) else {
        return false;
    };
    if let Some(notifier) = notifier {
        match child {
            Some(child) => {
                let _ = notifier.delete(INodeNo(parent), INodeNo(child), OsStr::new(&name));
            }
            None => {
                let _ = notifier.inval_entry(INodeNo(parent), OsStr::new(&name));
            }
        }
    }
    true
}

/// Apply one remote event to the local cache and notify the kernel so it drops
/// any stale cached metadata/data for the affected inodes.
///
/// The cache is authoritative-by-absence: dropping a directory's `children`
/// entry forces the next `lookup`/`readdir` to re-enumerate from the remote, so
/// most events only need to invalidate listings rather than re-fetch eagerly.
///
/// Applied to **every** mounted inode space, not just the primary one. This task
/// is per-daemon while node state is per-mount, and a `DriveEvent` names a uid,
/// not a mount — so a file trashed from another device has to be withdrawn from
/// whichever of our sessions is showing it, which for anything under a sync
/// folder is a [`Core::fork_state`] fork rather than `core.state`. Reaching for
/// the primary state alone left forks serving a deleted file indefinitely, the
/// read-side twin of `docs/BUGS.md` B74. Inode numbers are per-mount, so each
/// mount must be notified through its **own** channel; that pairing is what
/// [`Core::for_each_mount`] exists to preserve.
fn apply_event(core: &Core, event: &DriveEvent, dirty: &mut DirtyParents) {
    match event {
        DriveEvent::NodeUpdated {
            node_uid,
            parent_node_uid,
            is_trashed,
            ..
        } => {
            // A node we owe an upload for is *ahead* of the remote, not behind
            // it: this event is almost always the echo of our own empty-file
            // create, and re-fetching would replace the size and mtime of the
            // write we just accepted with the stale revision's — making a file
            // that was copied in seconds ago read as empty until its upload
            // lands (offline.md Phase 3).
            //
            // A write that has already drained is the same story one step later:
            // the drain brought the tree level with the revision it sealed, so
            // the feed's report of that revision has nothing to add, and acting
            // on it evicts the very bytes we uploaded. Claimed unconditionally,
            // even for a trash, so an echo we no longer care about does not
            // linger to suppress a later foreign change.
            let echo = core.take_self_change(node_uid);
            let ours = !*is_trashed && (echo || core.pending.lock().contains_key(node_uid));
            if ours {
                debug!(uid = %node_uid, "ignoring remote event for a node with a queued write");
            }
            let mut had_node = false;
            core.for_each_mount(|st, notifier| {
                if *is_trashed {
                    // Trashing makes a node vanish from its parent listing.
                    had_node |= forget_and_notify(st, notifier, node_uid);
                } else if !ours && let Some(&ino) = st.by_uid.get(node_uid) {
                    // Known node changed: drop its cached attrs/data (and
                    // listing if it is a directory) so the next access
                    // re-fetches. Its content blob may now be stale too.
                    had_node = true;
                    st.invalidate_listing(ino);
                    if let Some(notifier) = notifier {
                        let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
                    }
                }
            });
            // Once, not once per mount: the content cache is per-daemon.
            if had_node {
                core.cache.evict(node_uid);
            }
            // A create, rename or move-in shows up as a change to the parent
            // listing too, which is recorded for the end of the batch rather
            // than acted on here — see [`DirtyParents`].
            if let Some(parent_uid) = parent_node_uid {
                match ours {
                    true => dirty
                        .ours
                        .entry(parent_uid.clone())
                        .or_default()
                        .insert(node_uid.clone()),
                    false => dirty.foreign.insert(parent_uid.clone()),
                };
            }
        }
        DriveEvent::NodeDeleted { node_uid, .. } => {
            core.cache.evict(node_uid);
            core.for_each_mount(|st, notifier| {
                forget_and_notify(st, notifier, node_uid);
            });
        }
        // Continuity or scope was lost: our cached listings may be arbitrarily
        // stale, so drop every listing and tell the kernel to forget all
        // metadata. Inodes stay stable; dirs simply re-enumerate on next access.
        DriveEvent::ContinuityLost { .. } | DriveEvent::ScopeAccessLost { .. } => {
            warn!("event continuity lost; dropping all cached listings, resyncing lazily");
            core.for_each_mount(|st, notifier| {
                let dirs: Vec<u64> = st.children.keys().copied().collect();
                for &ino in &dirs {
                    st.invalidate_listing(ino);
                    if let Some(notifier) = notifier {
                        let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
                    }
                }
            });
        }
        // No substantive local change; the cursor advance is handled by the
        // caller persisting the event id.
        DriveEvent::CursorAdvanced { .. } | DriveEvent::SharedWithMeUpdated { .. } => {}
    }
}

/// The folders one batch of events changed, collected so the listings are
/// dropped once at the end of the batch rather than once per event.
///
/// Both halves of that matter. A burst of uploads produces one event per file
/// all naming the *same* folder, and dropping the listing per event made every
/// interleaved lookup re-enumerate that folder from the API — quadratic in its
/// size, and slow enough to stall a busy mount. Splitting foreign changes from
/// our own then removes the re-enumeration entirely for the common case: a file
/// this daemon created is already in the tree under the right parent, so there
/// is nothing for a re-enumeration to discover.
///
/// Neither is a weakening. The state after the batch is what it always was.
#[derive(Default)]
struct DirtyParents {
    /// Folders changed by someone else. Their listings must go: an event names
    /// no name, so a rename inside a folder is indistinguishable from a write to
    /// a file in it, and only re-enumerating can tell us which happened.
    foreign: HashSet<NodeUid>,
    /// Folders changed by this daemon, and the children the changes were about.
    /// Checked per mount — a mount that already lists every one of those
    /// children under that parent keeps its listing.
    ours: HashMap<NodeUid, HashSet<NodeUid>>,
}

/// Whether this mount's cached listing of `parent_uid` already accounts for
/// `child_uid`, so a change we made to that child leaves nothing to re-read.
///
/// "Nothing cached" counts as current: an absent listing is re-enumerated on the
/// next `readdir` regardless, so there is nothing to drop.
fn listing_accounts_for(st: &State, parent_uid: &NodeUid, child_uid: &NodeUid) -> bool {
    let Some(&parent) = st.by_uid.get(parent_uid) else {
        return true;
    };
    let Some(children) = st.children.get(&parent) else {
        return true;
    };
    let Some(&child) = st.by_uid.get(child_uid) else {
        return false;
    };
    children.contains(&child) && st.entries.get(&child).is_some_and(|e| e.parent == parent)
}

/// Drop the cached listing of every folder a batch of events changed, in every
/// mount that holds one, so the next `readdir`/`lookup` re-enumerates it.
///
/// Runs strictly after the whole batch is applied: doing it per event would
/// leave a window where a concurrent `readdir` re-populates a listing that a
/// later event in the same batch then has to drop again.
fn flush_dirty_parents(core: &Core, dirty: &DirtyParents) {
    if dirty.foreign.is_empty() && dirty.ours.is_empty() {
        return;
    }
    core.for_each_mount(|st, notifier| {
        let drop_listing = |st: &mut State, parent_uid: &NodeUid| {
            let Some(&parent) = st.by_uid.get(parent_uid) else {
                return;
            };
            st.invalidate_listing(parent);
            if let Some(notifier) = notifier {
                let _ = notifier.inval_inode(INodeNo(parent), 0, 0);
            }
        };
        for parent_uid in &dirty.foreign {
            drop_listing(st, parent_uid);
        }
        for (parent_uid, children) in &dirty.ours {
            if dirty.foreign.contains(parent_uid) {
                continue;
            }
            // One child this mount has not placed is enough: the listing it is
            // serving is missing something the user just made.
            if children
                .iter()
                .all(|child| listing_accounts_for(st, parent_uid, child))
            {
                continue;
            }
            drop_listing(st, parent_uid);
        }
    });
}

/// Poll the remote event cursor forever, applying each batch to the shared
/// state. Resumes from the cursor persisted in the DB so changes made while
/// unmounted are applied; only a first-ever mount seeds from the server head.
/// The cursor is persisted after every batch. Runs as a Tokio task; returns
/// only on fatal error.
///
/// Takes the whole `Core` rather than the primary mount's pieces: an event has
/// to be applied to every mounted inode space, and the registry that enumerates
/// them lives on the `Core` (see [`apply_event`]).
pub(super) async fn run_event_sync(
    client: ProtonDriveClient,
    scope: DriveEventScopeId,
    core: Core,
) {
    let db = core.db.clone();
    let mut cursor: Option<DriveEventId> = match db.get_event_cursor() {
        // Resume: pick up exactly where the last run left off.
        Ok(Some(saved)) => Some(DriveEventId::from(saved)),
        // First mount: a `None` cursor yields a single `CursorAdvanced` at the
        // server head; persist it so the next restart resumes instead of
        // reseeding (which would skip everything that changed offline).
        // Seeding needs the network, and this task also runs on mounts that
        // started offline (offline.md Phase 1) — so retry rather than giving up,
        // which used to disable live sync for the life of the daemon.
        Ok(None) => {
            let mut delay = ONLINE_PROBE_MIN;
            loop {
                match client.enumerate_events(&scope, None).await {
                    Ok(events) => {
                        let head = events.last().map(|e| e.id().clone());
                        let Some(c) = &head else {
                            break None;
                        };
                        if let Err(e) = db.set_event_cursor(c.as_str()) {
                            warn!(error = %e, ?delay, "persist seed cursor failed; retrying");
                            tokio::time::sleep(delay).await;
                            delay = (delay * 2).min(ONLINE_PROBE_MAX);
                            continue;
                        }
                        break Some(c.clone());
                    }
                    Err(e) => {
                        warn!(error = %e, ?delay, "seed event cursor failed; retrying");
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(ONLINE_PROBE_MAX);
                    }
                }
            }
        }
        Err(e) => {
            error!(error = %e, "read persisted cursor failed; live sync disabled");
            return;
        }
    };
    info!(?cursor, "event sync started");

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let events = match client.enumerate_events(&scope, cursor.as_ref()).await {
            Ok(events) => events,
            Err(e) => {
                warn!(error = %e, "event poll failed; retrying after interval");
                continue;
            }
        };
        if events.is_empty() {
            continue;
        }
        debug!(count = events.len(), "applying remote events");
        // Folders whose listing this batch invalidates, applied once at the end
        // rather than per event — see [`DirtyParents`]. Every `break` below falls
        // through to that flush, so an interrupted batch still publishes what it
        // did apply.
        let mut dirty = DirtyParents::default();
        for event in &events {
            // Converge the SDK's own caches (folder keys, entity cache) on the
            // server before applying the event to our tree. Without this, a node
            // re-keyed/moved by another client keeps a stale key in the SDK for
            // the life of the daemon (SDK plan #9). `apply_event` only touches
            // our FUSE state, so nothing else does this.
            if let Err(e) = client.invalidate_caches_for_event(event).await {
                warn!(error = %e, "sdk cache invalidation for event failed; retaining cursor for retry");
                break;
            }
            apply_event(&core, event, &mut dirty);
            let applied = event.id().clone();
            if let Err(e) = db.set_event_cursor(applied.as_str()) {
                warn!(error = %e, "persist event cursor failed; retaining prior cursor for retry");
                break;
            }
            // Advance only after this exact event is durably acknowledged. If a
            // later event fails, polling resumes from here and replays nothing
            // that was not already made visible locally.
            cursor = Some(applied);
        }
        flush_dirty_parents(&core, &dirty);
    }
}

/// Keep the local-file index fresh for the launcher prompt's "This computer"
/// results. Rebuilds the index whenever it is older than [`LOCAL_INDEX_TTL`],
/// then sleeps; runs on its own thread for the life of the daemon.
///
/// The walk is the one part of the daemon that touches the wider filesystem, so
/// it is deliberately kept off every hot path: it never runs on a FUSE or
/// control-socket thread, and it excludes the mountpoint (walking it would fault
/// every remote node in through FUSE, defeating on-demand hydration).
pub(super) fn run_local_index(
    db: Arc<Db>,
    indexing: Arc<AtomicBool>,
    transfers: Arc<TransferRegistry>,
    mountpoint: PathBuf,
) {
    loop {
        let age = db.local_indexed_at().ok().flatten();
        let stale =
            age.is_none_or(|at| now_secs().saturating_sub(at) >= LOCAL_INDEX_TTL.as_secs() as i64);
        if stale {
            scan_local_once(&db, &indexing, &transfers, &mountpoint);
        }
        std::thread::sleep(LOCAL_INDEX_CHECK);
    }
}

/// Walk `$HOME` once and replace the local-file index with what it finds.
/// Batches stream straight into SQLite, so peak memory is one batch — not the
/// whole home directory.
///
/// Reports itself as a job: the first scan after a fresh install walks the whole
/// home directory, and `indexing` alone only tells the launcher prompt to say
/// "still indexing" — nothing else showed that the daemon was busy.
fn scan_local_once(
    db: &Db,
    indexing: &AtomicBool,
    transfers: &Arc<TransferRegistry>,
    mountpoint: &Path,
) {
    let dirs = match AppDirs::new() {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "local index: cannot resolve app dirs");
            return;
        }
    };
    let Some(home) = dirs.home_dir() else {
        warn!("local index: cannot resolve home directory");
        return;
    };
    let generation = match db.local_begin_scan() {
        Ok(g) => g,
        Err(e) => {
            warn!(error = %e, "local index: cannot open scan generation");
            return;
        }
    };

    let excludes = localindex::default_excludes(mountpoint, &dirs.state_dir(), &dirs.cache_dir());
    indexing.store(true, Ordering::Relaxed);
    let started = Instant::now();

    // The walk has no idea how many files it will find, so the job counts what it
    // has seen and stays indeterminate.
    let job = transfers.begin_job("Indexing this computer");
    job.detail("Scanning your files");
    let walked = localindex::scan(&[home], &excludes, |batch| {
        if let Err(e) = db.local_upsert_batch(generation, &batch) {
            warn!(error = %e, "local index: batch write failed");
        }
    });

    // Prune what this scan did not see and rebuild the FTS index over the rest,
    // even if some batches failed — a partial index still beats none.
    job.detail("Building the search index");
    match db.local_finish_scan(generation, now_secs()) {
        Ok(indexed) => info!(
            walked,
            indexed,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "local index rebuilt"
        ),
        Err(e) => warn!(error = %e, "local index: finish failed"),
    }
    indexing.store(false, Ordering::Relaxed);
}
