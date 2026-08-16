# Architecture audit — performance, locking, robustness (2026-08-15)

Scope: `pdfs-fuse`, `pdfs-core`, and `proton-sdk-rs` as consumed by the mount.
Method: five parallel deep-read passes (locks, read path, write path, DB/IPC, SDK
transport), each finding re-verified against the tree before landing here.

Line numbers are as of commit `b37ab78`.

---

## Implementation status (2026-08-15, uncommitted working tree)

| Landed | Item |
|---|---|
| ✅ | DB5 depth-capped `is_pinned`, with a cycle regression test |
| ✅ | DB6 daemon `set_write_timeout`; every client-supplied `limit` clamped |
| ✅ | T7 `"http2"` added to the SDK's reqwest features (`h2` now in its lock file) |
| ✅ | D1 sweep no longer discards queued ops after the trash round trip |
| ✅ | D2 `path == blob` guard shared by all three pending-entry drops (`Core::release_pending`) |
| ✅ | D3 `ENOSPC` propagated from `write`/`fsync`/staging, `emergency_evict` wired up, scratch marked durable before any staging error |
| ✅ | D4 `record_pending_write` returns its failure; `release` fails with `ENOSPC` |
| ✅ | D5 `rescue_scratch` moves a blob with an unreadable sidecar to `recovery/` unaccompanied instead of fabricating one |
| ✅ | F2 `open` write path on `Lane::Transfer`, staged blob duplicated by reflink (`ContentCache::clone_or_copy`) |
| ✅ | F3 `fsync` on `Lane::Transfer` |
| ✅ | F5 mount registry lock released before FUSE teardown + join |
| ✅ | F6 kernel notifications batched under the State lock, flushed after it (`NotifyBatch`) |
| 🟡 | T6 `Filesystem::init` negotiates `max_readahead`/`max_background`/`PARALLEL_DIROPS`/`CACHE_SYMLINKS`; `blksize` now 1 MiB; `statfs` implemented against the account quota. **`n_threads`/`clone_fd`, `ATOMIC_O_TRUNC`, `READDIRPLUS` and `FOPEN_KEEP_CACHE` deliberately deferred** — each needs a handler or ordering change first (see `spawn_session`) |
| ✅ | T4 LRU touches buffered in memory, flushed in one transaction (and before every eviction pass) |
| ✅ | T5 no fsync per cached block; `cached_block` validates by length instead |
| 🟡 | DB8 three of the four indexes. `idx_pending_op_due` is **not** wanted: `next_due_op` orders by rowid and already exits on the first row, and offering the planner a `next_attempt_at` index makes it sort — `next_due_op_does_not_scale_with_queue_length` measures the regression. `activity_time` dropped |
| ✅ | DB9 `cache_size`, `mmap_size`, `temp_store=MEMORY`, `wal_autocheckpoint` |

Second batch (uncommitted on top of `e396c6b`):

| Landed | Item |
|---|---|
| ✅ | DB10 `flock(LOCK_EX\|LOCK_NB)` single-writer lock in `Db::open`; a `SQLITE_CORRUPT`/`NOTADB` file is quarantined (`cache.db.corrupt-<ts>`, WAL sidecars too) and rebuilt |
| ✅ | DB7 `VACUUM` and the deep `integrity_check` ack-then-background (`Core::start_vacuum`, `Core::start_or_read_integrity_check`, visible as jobs); `Response::CacheReport` gained `integrity_running`, `pdfs doctor` polls for the verdict |
| ✅ | DB7 `local_finish_scan` no longer rebuilds the whole FTS index: `local_upsert_batch` maintains `local_fts` incrementally (valid because `path` is the key and `name` derives from it), migration V23 does one last rebuild |
| ✅ | §5 `recover_fsynced_writes` no longer `read_dir`s per idle pass (`ContentCache::recovery_pending`) |
| ✅ | §5 `own_sealed_revs` persisted (migration V24, `own_sealed_rev` table, 7-day TTL) — a restart no longer reopens the B70-layer-B fork window |
| ✅ | §5 staging reconcile at mount (`Core::reconcile_staging`): staged blobs no queued op refers to are re-queued, or named in the log when they cannot be addressed |
| ✅ | §5 queue/staging reporting — `Response::Status` gained `parked_uploads`, `failing_ops`, `failing_error`, `staged_bytes`, `staged_oldest_secs`; `pdfs status` prints them when non-zero |

Third batch (uncommitted on top of `7ad9231`):

| Landed | Item |
|---|---|
| ✅ | F1 `queue_revision` no longer gap-fills from the network. It stages an incomplete blob (`fill_gaps_cached` resolves only what the local caches already hold, capped at 64 MiB) and `drain_revision` finishes it off the dispatch loop |
| ✅ | F1 a partial write over an undrained one is merged instead of refused: `merge_over_pending` folds the superseded blob's authored ranges in and inherits its `base_mtime`/`base_size`/`based_on`. `release` keeps its inherited ordering, which is now affordable because only local I/O is left on it |
| ✅ | F4 size-upgrade waiters no longer occupy a pool thread: `getattr`/`lookup` park their `Reply` on `SizeWaitQueue`, served by one thread that exits when nothing is parked. `getattr` no longer needs a `Lane::Meta` job at all |
| ✅ | F4 upgrade threads bounded by `MAX_SIZE_UPGRADES` (8 folders in flight); past the cap a `stat` answers provisionally, the same fallback the deadline takes |

Corrected while implementing: the audit's claim that F1's `fill_gaps` bypasses
the block cache is wrong — it calls `read_range_remote` with `cache_blocks =
true`, which consults the whole-file blob and then the block cache before the
network. What made `release` slow was going to the network at all, not doing so
uncachedly.

Fourth batch (uncommitted, same working tree):

| Landed | Item |
|---|---|
| ✅ | T3 the in-memory ring (`StreamRing` → `BlockRing`, now a true LRU) sits in front of **both** block paths, not just the disk-bypassing stream. A block read off disk is promoted into it, so the other ~31 kernel reads of that block cost nothing |
| ✅ | T1 `BlockFlight`: `(uid, mtime, size, bidx)` single-flight over block fetches, so demand read, kernel read-ahead and prefetch share one download+decrypt. The leader's claim is a drop guard, so a cancelled `JoinSet` leaves no key nobody will publish; a follower whose leader vanished fetches for itself |
| ✅ | T2 `Core::prefetch_ahead` on both paths: per-file sequential detection (`Prefetch::window`), depth 2→8 doubling per sequential read, 0 after a seek, skipping blocks already in the ring or on disk (`ContentCache::has_block`), bounded by its own `PREFETCH_BUDGET` permits with `try_acquire_owned` so it never queues ahead of a demand read |
| ✅ | The block-cache disk write moved off the read's critical path (`spawn_blocking` inside `read_block`) |

Corrected while implementing: T3's premise is half stale — `ContentCache::read_range`
already `pread`s only the requested window, and T4/T5 removed the per-read SQLite
write and the fsync. What remained, and is what the ring fixes, is `cached_block`
reading and allocating the whole 4 MiB for each 128 KiB the kernel asks for.
T1's fix is client-side rather than in the SDK's `RevisionReader` as the audit
suggested: the SDK ships from a separate repo through crates.io, and the flight
map needs the client's `(mtime, size)` validity pair to be safe across revisions.

Fifth batch (uncommitted on top of `e48c4bb`):

| Landed | Item |
|---|---|
| ✅ | DB3 `nodes.path` is persisted (migration V25, backfilled by the same walk V21 used). `path_of` reads a column; `walk_path_of` stays as the fallback for a row written before it existed |
| ✅ | DB3 a folder upsert whose name, parent, path and trashed flag are all unchanged — i.e. every re-listing — no longer touches its subtree at all. `re_listing_a_folder_does_not_scale_with_its_subtree` measures it |
| ✅ | DB3 when the subtree *has* moved, one recursive walk carries both the new path and whether anything above is trashed, so a descendant costs two trivial statements instead of a `path_of` walk plus an ancestor walk of its own (`reindex_subtree_tx`) |
| ✅ | DB4 `sync_folder_list` and the pin lookup are hoisted out of the per-candidate loop (`Core::search_roots`, `Core::pinned_set`); `SearchRoots::resolve` maps a hit to its mountpoint by string arithmetic over the path the index already carries, with no query at all |
| ✅ | DB2 a pool of read-only connections (`ReadPool`, `Db::read`), and every `SELECT`-only method routed at it. `a_read_does_not_wait_for_the_write_connection` pins the property |
| ✅ | DB1 `State` no longer writes to SQLite. Mutations queue a `DbWrite` in an outbox that `StateGuard` applies **after** releasing the inode lock — so interning a 5000-child listing no longer commits a transaction with the whole mount frozen |

Corrected while implementing: DB4's "filter by score *before* decorating" was
already true — `relevance_score(…)?` short-circuits ahead of the `SearchHit`
construction. What actually cost the keystroke was `sync_folder_list` plus a
`path_relative_to` walk per (hit, sync folder) pair, and `is_pinned`'s ancestor
CTE per hit. DB3's suggested `path` column is here, but its companion suggestion
to skip the reindex on an unchanged name/parent needed the trashed flag in the
comparison too: a folder being trashed has to take its subtree out of the index,
and the old code only did that by accident, through reindexing unconditionally.

DB1 is done for writes, not reads: `State::has_children` still reads the DB
under the inode lock. That read now goes to the pool, so it no longer contends
with a writer, and moving it would mean changing what `rmdir` is allowed to
assume.

Sixth batch (on top of `433e206`):

| Landed | Item |
|---|---|
| ✅ | §5 parallel drain — `DRAIN_WORKERS = 3` threads over one queue, sharing it through `pending_op.claimed_at` (migration V26) and `Db::claim_next_due_op`. Select-and-claim is one transaction, and the query excludes any op whose uid another worker holds, so ordering still holds per node without holding it globally. Worker 0 keeps the idle chores. Stale claims are cleared in `Db::open`, which the single-writer `flock` makes sound |
| ✅ | §5 adaptive debounce — `Core::revision_debounce` widens the grace period toward the node's last measured upload wall-time, clamped to `[2s, 60s]`; only a *completed* upload counts as a measurement, so a fault cannot widen it permanently |
| ✅ | §5 cancel on supersede — `Core::cancel_upload` sets a per-node flag that the upload's `CountingReader` refuses the SDK's next block on. Signalled by `enqueue_staged_write`, `queue_trash` and `discard_queued_ops`, i.e. every path that either replaces those bytes or unlinks them. A cancelled upload returns `Ok` rather than a retry: its row and its blob already belong to the write that cancelled it |

Corrected while implementing: the audit's "a claim column and N=3–4 workers" needed
one thing it does not mention — with several workers, a supersede or a trash can
now delete a row that is *in flight*, which single-threading used to make
impossible. That is what makes the cancellation flag part of the parallel-drain
change rather than a separate throughput nicety.

Seventh batch — T8, in `proton-sdk-rs` (uncommitted there; reaches this client
only through a crates.io publish, so nothing here changes until that lands):

| Landed | Item |
|---|---|
| ✅ | T8 token lock — `Inner::tokens` is a `std::sync::RwLock<Arc<Tokens>>` read per request, with a separate `Inner::refresh` async mutex held across the refresh call. An ordinary request no longer queues behind an in-flight `auth/v4/refresh`; `an_ordinary_request_does_not_wait_for_an_in_flight_refresh` pins it against a deliberately slow loopback refresh |
| ✅ | T8 crypto off the reactor — the three remaining inline spots are on the blocking pool: `create_public_link`'s bcrypt + SRP verifier, `decrypt_invitation_name`'s share-key S2K, and the public-link handshake's `generate_proofs` + `unlock_share_key` (`public_link.rs::blocking`) |
| ✅ | T8 `list_blocks` pipelining — pages after the first are fetched concurrently in a window ramping 2→6, page offsets derived positionally (which the existing contiguity check already requires). A 4 GiB file is ~6 round trips instead of 21 |
| ✅ | T8 one 4 MiB copy per block removed — `ContentKey::decrypt_block` returns `Bytes` via `LiteralData::into_bytes` instead of `data().to_vec()`; `decrypt_block_blocking`, `digest_and_decrypt_block_blocking` and `RevisionReader::block_plaintext` carry `Bytes` through |
| n/a | T8's last bullet is client-side only: the SDK already exposes `RevisionReader::block_sizes()`, so consulting it instead of the hardcoded `BLOCK_SIZE = 1 << 22` is a change in this repo, not that one |

Note: `decrypt_block` returning `Bytes` is a breaking change to the SDK's public
crypto surface, so the publish that carries T8 needs the version bump to match.

---

## 0. The one-sentence version

The subsystem designs are sound and unusually well documented; the defects are
almost all **placement** — correct work happening on the wrong thread, under the
wrong lock, or at the wrong granularity. Three exceptions destroy user bytes.

Two structural facts explain most of the severity:

1. **fuser's session loop is non-concurrent** and `Config::n_threads` is unset →
   `unwrap_or(1)` (`fuser-0.17.0/src/session.rs:254`). One thread reads
   `/dev/fuse`. Anything a handler does inline stalls *every* request on the
   mount, including the 11 idle workers, which have nothing to receive.
2. **One `Mutex<Connection>`** (`pdfs-core/src/db/mod.rs:73`) for the whole
   daemon, and `State` (`lib.rs:305`) is one mutex over the entire inode space —
   and `State`'s methods write through to the DB *while the caller holds it*.
   The `State → Db` edge turns a microsecond lock into a multi-second one.

Fixing (1) and (2) is worth more than every micro-optimisation below combined.

---

## 1. Data loss — fix first

### D1. Sweep deletes staged blobs after a network round trip
`sweep.rs:169` — `discard_queued_ops` (`lib.rs:2610`) deletes the op row *and*
unlinks the staged blob, and runs *after* the `trash_nodes` call at
`sweep.rs:150-153`. The B71 interlock `duplicate_still_removable`
(`sweep.rs:193`) runs *before* it. A `close(2)` landing in that window stages
bytes that the sweep then destroys. Only survives today because `SweepMode`
defaults to report-only.

Fix: drop `discard_queued_ops` from `remove_conflict_copy` entirely. Unhook
locally and let the drain discover the node is trashed — `revision_conflict`
(`drain.rs:615`) already routes that to `keep_as_conflict_copy`, the correct
non-destructive outcome.

### D2. Conflict paths evict a `pending` entry that may belong to a newer write
`drain.rs:713` (`keep_as_conflict_copy`), `drain.rs:751` (`abandon_to_staging`)
both do `self.pending.lock().remove(uid)` unconditionally. `drain_revision`
deliberately does not — `drain.rs:890` removes only when `op_in_mem.path ==
blob`, precisely because a supersede may have replaced the entry mid-flight.

Consequences of the missing guard: reads fall back to a stale remote; the next
`remote_baseline` (`lib.rs:2195`) forks a spurious `(sync-conflict)` copy; and
the incomplete-write interlock at `lib.rs:2246` is silently disarmed, letting an
incomplete write gap-fill from a revision that no longer describes the file.

Fix: copy `drain_revision`'s `path == blob` guard into both sites.

### D3. No ENOSPC path anywhere
`cache.rs:891` `emergency_evict` has **zero callers** (verified by grep across
the workspace). `write(2)` returns `EIO`, not `ENOSPC` (`filesystem.rs:379`).
Worse: `stage_write` failing on a full disk leaves the scratch file with no
durability sidecar, so `discard_unmarked_scratch` deletes the only copy at next
mount — the one place the "never delete on a failure path" invariant breaks.

Fix: call `emergency_evict` on `StorageFull`, propagate `ENOSPC`, and
`mark_scratch_durable` before returning any staging error.

### D4. `record_pending_write` failure is only a `warn!`
`state.rs:925-931`. Its own doc comment (`:918-923`) says this row is the only
record that the file is as long as the caller was told. On ENOSPC the daemon
acknowledges the write and loses the size. Fix: fail the `release` with `ENOSPC`.

### D5. `rescue_scratch` fabricates a sidecar for a corrupt one
`cache.rs:836-861` invents `uid: "recovered~<name>"`, `complete: true`. That uid
parses, so `recover_fsynced_writes` enqueues an op against a nonexistent node
that retries forever, and `complete: true` is a lie that stops any gap-fill.
Fix: move the blob to `recovery/` *without* a sidecar — `recovered_writes`
already retains sidecar-less blobs for a human, which is the right outcome.

---

## 2. Whole-mount freezes (single dispatch thread)

### F1. `release(2)` gap-fills over the network, inline
`filesystem.rs:687` → `lib.rs:2113` → `reads.rs:616` → `rt.block_on`. Closing a
2 GB file after a 4 KB edit downloads ~500 blocks with the dispatch loop stopped:
no `stat`, no `ls`, no `open`, no `read` anywhere on the mount. The comment at
`filesystem.rs:674-686` already concedes the placement is wrong.

Fix: the staged blob is allowed to be incomplete (`complete: false` is
first-class, and `drain_revision` at `drain.rs:851-869` already gap-fills off the
loop). Skip `fill_gaps` in `queue_revision` for partial writes; add the per-node
staging-in-flight barrier that `read`/`open` wait on, then move `finish_release`
to `Lane::Transfer`. Also: `fill_gaps` uses `read_range_remote`, bypassing the
block cache — a cache-aware fill makes editing a recently-read file ~free.

### F2. `open(2)` copies the whole staged blob, inline
`filesystem.rs:180` `std::fs::copy` of a possibly multi-GB pending blob, on the
dispatch thread. Fix: reflink (`FICLONE` → `copy_file_range` → `fs::copy`), and
route the write-open path through `Lane::Transfer`.

### F3. `fsync(2)` runs `sync_all` + sidecar write inline
`filesystem.rs:602`, `:641`. Apps that fsync per record stall the mount at their
barrier rate. No ordering constraint — the kernel waits for the reply either way.
Fix: `Lane::Transfer`.

### F4. Size-upgrade waiters occupy every worker in both lanes for 10 s
`filesystem.rs:75-88`, `:1069-1077` queue a `Lane::Meta` job that blocks on a
condvar up to `SizeUpgrade::WAIT = 10s` (`lib.rs:4292`). This defeats the premise
of the lane split (`workers.rs:39-52`: "metadata jobs are short"). Single-flight
is per folder, so `ls -l` across N cold folders fills all 11 threads. Also
`lib.rs:1801` spawns an unbounded thread per folder.

Fix: register the `Reply` on a completion list instead of blocking a worker; bound
the upgrade threads.

### F5. `mounts` lock held across FUSE teardown + thread join
`devices.rs:539`, `devices.rs:870`. Edition-2024 if-let chains keep the
`MutexGuard` alive through the body, so the lock spans `abort_fuse_connection` +
`umount_and_join`. If the secondary session is inside F1's gap-fill, the join
waits it out while the shutdown path blocks on the same lock → systemd SIGKILLs
mid-unmount, stranding the endpoint that `abort_fuse_connection` exists to avoid.
Fix: `let taken = self.mounts.lock().remove(&id);` then drop before teardown.

### F6. Notifier called while holding the State lock
`lib.rs:706-710` `for_each_mount`. `for_each_state` (`:689`) and
`flush_access_changes` (`:715`) both document and honour the opposite rule.
`inval_inode` → `invalidate_inode_pages2` can block on pages that this daemon's
own workers must fill — and those workers are blocked on the State lock the
notifier holds. `background.rs:226-234` walks every cached directory this way in
one hold. Fix: collect inodes under the lock, notify after dropping it.

---

## 3. Throughput

### T1. No single-flight on block fetches — ~2× bandwidth and CPU, steady state
`reads.rs:475-526`. Nothing registers "(uid, mtime, size, bidx) in flight". With
`FUSE_ASYNC_READ` on, the demand read and the kernel's readahead land in the same
4 MiB block for 31 of every 32 reads, and both fetch and decrypt it in full.

The codebase already has the pattern (`ReaderSlot::Pending`, `reads.rs:164-277`)
and the SDK has `SingleFlight` used for node/parent-key loads. Best fixed in the
SDK's `RevisionReader` so every consumer benefits.

### T2. No prefetch for anything under 256 MiB
`reads.rs:541` calls `stream_readahead` only when `!cache_blocks`. Everything
else is a strictly serial block chain: fetch, serve 32 kernel reads, stall.
Derived ceiling on a 100 ms RTT link ≈ 24 MB/s against ~125 MB/s available. The
SDK's 10-way block concurrency is never exercised.

Fix: per-(ino, fh) sequential detection, prefetch depth ramping 2→8, reset on a
non-sequential offset. Give prefetch its own permit budget or `try_acquire` so it
never delays a demand read (SDK's semaphore is flat, `client.rs:126`).

### T3. Warm cached reads re-read and re-allocate 4 MiB per 128 KiB
`cache.rs:570-585`: per kernel read, 2 opens + a JSON parse + a SQLite write txn
+ a 4 MiB pread + a 4 MiB alloc. 128 MiB of traffic to deliver 4 MiB. ~60 MB/s
ceiling on a fully cached file.

Fix: serve the disk-cache path through an in-memory block LRU (the `StreamRing`
shape, currently used only when `cache_blocks == false`); fold `Meta` into the
block filename to kill the sidecar read; `pread` only the requested window.

### T4. A SQLite write on the read hot path
`cache.rs:581` `cache_accessed` = `UPDATE cache_entries SET last_accessed` under
the global connection mutex, 32× per 4 MiB read, contending with every `lookup`
and `readdir`. Reintroduces one layer down exactly the contention the `Lane::Meta`
reservation exists to prevent. Also `filesystem.rs:297` `is_pinned` per read.

Fix: batch LRU touches in memory, flush periodically; cache the pin bit.

### T5. `fsync` per cached block on the cold path
`cache.rs:605` `sync_all()` inside `store_block`, inline on the thread the kernel
read is waiting on. The cache is disposable and `cached_block` doesn't even
validate length. Fix: drop the fsync, validate by length like `valid_blob` does.

### T6. Kernel/mount caps left at fuser defaults
No `Filesystem::init` impl at all. `mount.rs:257-273` sets only
`FSName`/`Subtype`/`DefaultPermissions`.

| Knob | Now | Should be |
|---|---|---|
| `n_threads` / `clone_fd` | 1 / false | 4 / true (`FUSE_DEV_IOC_CLONE`, Linux 4.5+) |
| `max_background` | 16 | 64 |
| `max_readahead` | 128 KiB | 4 MiB (match `BLOCK_SIZE`) |
| `FUSE_PARALLEL_DIROPS` | off | on — kernel serialises concurrent lookups per dir |
| `FUSE_DO_READDIRPLUS` + `_AUTO` | off, no `readdirplus` impl | on — `ls -l` is readdir + N lookups today |
| `FUSE_ATOMIC_O_TRUNC`, `CACHE_SYMLINKS` | off | on |
| `FOPEN_KEEP_CACHE` | never set (`filesystem.rs:145`, `:214`) | set — the event poller already does the `inval_inode` this requires |
| `attr.blksize` | `512` (`filesystem.rs:1149`) | `1 << 20` — coreutils size their buffers from it |
| `statfs` | not implemented | implement — `df` and free-space preflights see garbage |

### T7. HTTP/2 is compiled out of the SDK
`proton-sdk-rs/Cargo.toml:27` uses `default-features = false` and omits
`http2`, which is in reqwest 0.13's default set. Verified: `h2` appears **zero**
times in `Cargo.lock`. Every concurrent block download pays its own TCP+TLS
handshake instead of multiplexing. **One-line fix, add `"http2"`.**

### T8. Other SDK hot spots
- Token mutex held across the refresh network call (`http.rs:503-524`); every
  ordinary request takes the same lock just to *read* the token. Worst case ~2 min
  of total API stall. Fix: `ArcSwap<Tokens>` for reads, separate refresh mutex.
- Three inline S2K/PGP decrypts on the reactor (`client.rs:1376-1378`, `:1281`,
  `:6092`) — tens of ms each, while the rest of the crate correctly uses
  `crypto::blocking`.
- `list_blocks` pages 50 at a time, sequentially (`transport.rs:152`): a 4 GiB
  file is 21 serial round trips before the first byte.
- ~3 full 4 MiB copies per block delivered (`content.rs:350-366`, `revision.rs:361`).
- Client hardcodes `BLOCK_SIZE = 1 << 22` instead of consulting the SDK's
  `block_sizes()`; any revision with a different geometry doubles fetch count.

---

## 4. Database and IPC

### DB1. Break the `State → Db` edge
`state.rs:614`, `:688`, `:772`, `:854`, `:928`, `:281` all write to SQLite from
inside `State` methods, under the whole-mount inode lock. `lib.rs:1665-1695`
commits a 5000-row listing that way. `intern_batch`/`intern_from_db` already
prove the split works — extend it to the rest, and write through after `drop(st)`.

### DB2. Single connection throws away WAL's concurrent readers
Add a small pool of read-only connections with the same PRAGMAs and route every
`SELECT`-only method at them. The module's own justification (`mod.rs:6-7`,
"FUSE callbacks already serialize behind the State lock") stopped being true when
reads moved onto an 11-thread pool.

### DB3. A folder upsert re-indexes its whole subtree
`nodes.rs:532-608`. Renaming a 10k-descendant folder ≈ 50k statements, ~20k
recursive CTEs, holding both the State and DB locks. Fixes, in order: persist a
`path` column so `path_of` stops being a CTE; skip the reindex when name and
parent are unchanged (every re-listing pays it today); batch the FTS refresh;
hoist `node_is_indexable_tx` out of the per-descendant loop.

### DB4. Search is ~6000 statements per keystroke
`lib.rs:3064` takes up to 1000 candidates, then per candidate runs `path_of`
(CTE), `is_pinned` (CTE), and `mounted_search_path` → `sync_folder_list()` +
`path_relative_to` per sync folder (CTE). Fix: hoist `sync_folder_list` out of
the loop, batch pins via the existing `pinned_uids`, filter by score *before*
decorating, and persist `nodes.path`.

### DB5. `is_pinned`'s CTE has no depth cap and no cycle guard
`pins.rs:81-88`. Every other walk in the codebase caps (`utils.rs:56`,
`nodes.rs:312`, `nodes.rs:643`, `migrations.rs:580`). A `parent_uid` cycle —
which `nodes.rs:567-570` treats as data the API can hand you — makes `UNION ALL`
non-terminating *while holding the DB mutex*. Unrecoverable mount hang, one-line
fix.

### DB6. IPC can wedge the daemon two ways
- No `set_write_timeout` on the daemon side (`pdfs-fuse/src/control.rs:108` sets
  only the read timeout; the *client* side gets both, `pdfs-core/src/control.rs:1390`).
  64 clients that never read their reply exhaust `MAX_CONTROL_HANDLERS`
  permanently.
- Client-supplied `limit` goes unclamped into `SQL LIMIT ?` (`control.rs:349`,
  and `ListActivity`/`PhotoTimeline`/`AlbumPhotos`), with no cap on response size.
  One request materialises the whole account under the mutex.

### DB7. Long handlers that don't ack-then-background
`VACUUM` (`maintenance.rs:183`), deep `integrity_check`, and `local_finish_scan`
(`db/local.rs:79-95`, a whole-index FTS trigram rebuild) all run under the
connection mutex — freezing the mount for tens of seconds. Every *other* long
handler already uses the ack-then-background pattern (`control.rs:452`, `:638`,
`:707`).

### DB8. Missing indexes (one migration)
```sql
CREATE INDEX idx_sync_entry_remote_uid  ON sync_entry(remote_uid);
CREATE INDEX idx_pending_op_due         ON pending_op(next_attempt_at, id);
CREATE INDEX idx_local_files_scan_gen   ON local_files(scan_gen);
CREATE INDEX idx_nodes_parent_live      ON nodes(parent_uid) WHERE trashed = 0;
```
`idx_pending_op_due` matters more than it looks: B70's tempfile parking puts long
runs of parked ops at the head of the queue, and `next_due_op`'s claim that it
"exits on the first row" (`ops.rs:462-465`) stops holding.
`activity_time` (`migrations.rs:302`) indexes `time DESC` while both queries order
by `id` — dead weight on every insert.

### DB9. PRAGMAs
Set and correct: `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys`,
`journal_size_limit`, `busy_timeout`. Missing: `cache_size` (still the 2 MiB
default, against a `node_json`-heavy schema), `mmap_size`, `temp_store=MEMORY`
(every `ORDER BY … COLLATE NOCASE` and every recursive CTE spills to a file
today), `wal_autocheckpoint` (the committing thread — often the dispatch loop —
runs the checkpoint synchronously).

### DB10. Robustness gaps
No `SQLITE_CORRUPT`/`SQLITE_NOTADB` handling anywhere; a corrupt `cache.db` means
the daemon won't start. It is a pure cache — rename it aside and rebuild, loudly.
No single-instance lock: a hand-run `pdfs mount` alongside the systemd unit gives
two writers. Add `flock(LOCK_EX|LOCK_NB)` in `Db::open`.

---

## 5. Sequential drain, and other throughput ceilings

- **The drain is one thread, one op** (`drain.rs:130-191`, `next_due_op` is
  `ORDER BY id LIMIT 1`) while the bulk uploader is 4-way (`upload.rs:7`). A
  10 GiB upload blocks every queued rename and small write behind it. Fix: a claim
  column and N=3–4 workers; ordering only needs to hold per uid.
- **A permanently failing op** never wedges the queue (the backoff is correct and
  never dropping the op is right) but steals 30 s of the only drain thread every
  5 minutes forever, invisibly. Surface `attempts`/`last_error` in `Status`.
- **`recover_fsynced_writes` does a `read_dir` on every idle drain iteration**
  (`drain.rs:139`) for a directory that is empty ~always.
- **No adaptive debounce.** A file saved slower than 2 s but faster than its
  upload never catches up. Scale the debounce toward the last measured upload
  wall-time per uid, and cancel an in-flight upload on supersede.
- **Nothing bounds `staging/`.** Parked transient creates (`ops.rs:85`) are parked
  forever; three abandoned 4 GiB browser downloads leave 12 GiB with no expiry and
  no way for the user to see it. Report staging bytes + parked count + oldest age.
- **`own_sealed_revs` (`lib.rs:383`) is in-memory and unbounded** — a restart
  re-opens the B70 conflict hole. One column persists it.
- **Orphaned staging blobs are unrecoverable by tooling.** Three paths stage the
  only copy and return `EIO` with no durable reference (`lib.rs:2250`, `:2276`,
  `:2297`); `hydrate_pending` walks the op table, `recover_fsynced_writes` walks
  `recovery/`, nobody walks `staging/`. Add a startup reconcile.

---

## 6. Confirmed sound — don't spend time here

Reader/key caching is single-flighted and LRU-bounded per file (`reads.rs:164-277`);
no per-block key derivation. Block decryption is off the reactor
(`revision.rs:38`). `readdir`/`lookup` are genuinely not N+1 — one uid
enumeration plus one batched `enumerate_nodes_light`, with S2K deferred to a
chunked single-flight. TTL=30 s is fine and could go higher given active
invalidation. **No lock-order inversion anywhere**; no `std::sync::Mutex`, no
`.lock().unwrap()` outside tests. Worker queues are unbounded *by design* and the
reasoning is right — backpressure onto the dispatch loop is the thing being
avoided. `write(2)` takes no lock and does no fsync. `fallocate`'s `PUNCH_HOLE`
branch correctly marks the range authored so gap-fill won't refill it. Migrations
are single-transaction with a hard newer-schema refusal. `enqueue_op`'s supersede
is transactional and cannot orphan the previous blob. `fsync` durability
semantics are correct and correctly documented.

---

## 7. Suggested order

1. **One-line safety** — depth-cap `is_pinned` (DB5), `set_write_timeout` (DB6),
   clamp IPC `limit`s (DB6), add `"http2"` (T7).
2. **Stop losing bytes** — D1, D2, D3, D4, D5.
3. **Off the dispatch loop** — `n_threads=4`+`clone_fd` (T6), F1, F2, F3, F5, F6.
4. **Break `State → Db`** (DB1) and stop the indexer monopolising the connection
   (DB7).
5. **Cheap kernel/DB tuning batch** — rest of T6, T5, T4, DB8, DB9.
6. **Real throughput** — T1 (block single-flight, ideally in the SDK), T3
   (in-memory block cache), T2 (sequential prefetch).
7. **Structural** — persist `nodes.path` (DB3/DB4), parallel drain, adaptive
   debounce, staging reconcile + reporting.
