# Bugs & side findings

Running tracker for bugs and loose ends spotted while working on something else —
so they don't get lost when the session that found them ends.

Conventions:

- **Open** / **Fixed (unverified)** / **Fixed** / **Won't fix**
- "Fixed (unverified)" means the code change is in but nobody has driven the real
  flow against it yet. It stays that way until someone does.
- Record how it was *found*, not just what it is. The repro is the expensive part.

---

## B78 — Profile backup fails: Cannot create file at the root of a device

**Status:** Open
**Found:** 2026-07-29, user reported `upload profile: proton api error NotEnoughPermissions (http 422): Cannot create file at the root of a device`

The background task that backs up the machine's profile (sync folder mappings, pins, cache budget) currently attempts to write `profile.json` directly to the device root node (`device.root_uid`) on the Drive backend. The Proton Drive API has started enforcing a restriction that files cannot be created directly at the root of a device; they must be placed inside a folder.

**Consequence:** The upload is rejected with HTTP 422. The daemon retries when changes happen but the profile is never successfully backed up. Local changes (pinning, adding a sync folder) still take effect locally, but they will not be restorable on a new machine.

**Planned Fix:** Match the original `RECOVERY.md` spec by putting it in a `.proton-drive-linux` folder within the device root.
- `pdfs-fuse/src/profile.rs:save_profile`: Look up (or create) the `.proton-drive-linux` folder under the device root, and upload `profile.json` there instead.
- `pdfs-fuse/src/profile.rs:load_profile`: Check for `PROFILE_FILE_NAME` inside `.proton-drive-linux` first, falling back to the device root to allow older, existing profiles to be restored until they are overwritten.

---

## B1 — FUSE rename loses the file (data loss)

**Status:** Fixed (verified 2026-07-20)
**Found:** 2026-07-19, user reported `mv file.mkv dir/` on the mount deleted the file
**Verified:** the exact repro below on the new daemon — `mv` returns 0, the file is
present in the destination listing and reads back its content. Previously it was in
neither directory.
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `Filesystem::rename`

`rename` ended with:

```rust
st.forget(&uid);
st.children.remove(&newparent);   // memory only
```

`forget()` deletes the node's **DB row** (`db.delete_node`). `children.remove()`
only drops the **in-memory** listing, leaving `nodes.listed = 1` on the
destination folder. The next `ls` goes through `ensure_children`, which takes the
DB fast path (`children_if_listed`, lib.rs:722) and rebuilds the destination from
the database — where the node no longer has a row.

Net effect: file gone from the source, absent from the destination, and
`rename(2)` returned **0**. Silent loss with a success code.

**Severity is worse than "stale cache":** `listed` is a **DB** column, so the bad
state survives a daemon restart. Nothing clears it on its own — the affected
folder keeps serving the listing that omits the file indefinitely, until an event
invalidation or a manual refresh happens to hit it. Confirmed on the repro
folders, which still read `listed=1` with no child rows minutes later.

The data itself is believed intact server-side (the move succeeded; only the
local view is wrong), but that was **not** verifiable without a daemon restart —
see the status line.

Also hits plain in-place renames (`mv a.txt b.txt`), same mechanism — the parent
is both source and destination there.

The control-socket `Core::move_to` path was always correct; it used
`invalidate_listing`, which clears the DB flag as well. The FUSE path didn't.

**Fix:** use the same helper.

```rust
st.forget(&uid);
st.invalidate_listing(newparent);
```

**Repro:**

```
cd <mount> && mkdir d && echo hi > f.txt && mv f.txt d/
# renameat2(...) = 0, but f.txt is in neither directory,
# not in trash, and absent from the `nodes` table
```

**Note on `invalidate_listing`:** it early-returns when the folder isn't in the
in-memory `children` map, so it won't clear a stale DB `listed` flag in that
case. Safe on this path (both `lookup_child` and the explicit
`ensure_children(newparent)` guarantee the listing is resident), but it's a sharp
edge for other callers. See B4.

---

## B2 — The reported .mkv went to trash, the repro vanished entirely

**Status:** Partly resolved — `mv` exonerated; trash origin still unattributed
**Found:** 2026-07-19, while chasing B1

### What was settled (2026-07-19)

**`mv` never trashes.** Three repros on the mount, all with the old (pre-B1-fix)
daemon:

| repro | node state | destination | result |
|---|---|---|---|
| `zz-claude-repro-src.txt` | create still queued | fresh folder | vanished, **not** trashed |
| `zz-b2-settled.txt` | settled, real uid, landed | listed folder | vanished, **not** trashed |
| `zz-b3 Movie Title.mkv` | settled | folder named as the file's stem (the user's exact shape) | vanished, **not** trashed |

So B1 fully explains the *disappearance*, and nothing in the rename path
trashes. The `.mkv` reaching the trash has a different cause.

**The listing the user acted on was almost certainly stale.** B1/B4 make stale
listings *persistent* — `listed = 1` lives in the DB and survives restarts — so
`ls` showing the file at 15:57 is not evidence it still existed remotely. This
is the most likely reading: it had been trashed earlier and the mount kept
showing it.

**Ruled out:**

- *Sync engine.* `reconcile_folder` gates on `mode == "mirror"`; `~/Videos` was
  `ondemand` from 15:39:27. All three sync trash sites log, and no such rows exist.
- *Log pruning hiding the evidence.* `ACTIVITY_KEEP = 2000`; the table is at the
  cap but its oldest row is Jul 17 18:03, so the whole window is covered. The
  `.mkv`'s uid appears exactly once — an `Upload` at 06:07:56 — and never again.
- *Conflict machinery.* `keep_as_conflict_copy` uploads a new file under an
  alternate name; it never trashes the original.
- *Eviction through the mount.* Was the best theory: `mirror→ondemand` calls
  `evict_dir_contents`, which would issue one `unlink(2)` per file, and FUSE
  `unlink` trashes remotely. But `apply_sync_folder_mode` evicts **before**
  `spawn_ondemand_mount`, so the deletes hit the plain local directory. Dead.
- *Control-socket delete.* `CtlRequest::Delete` logs on both success and failure.

**What remains:** `trash_child` (backing FUSE `unlink`/`rmdir`) was the only
trash path that wrote no activity row — so an ordinary `rm` on the mount, at any
point after 15:39:27 when `~/Videos` became a FUSE mount, would produce exactly
what we see and leave no trace. Note there is also a *trashed folder* of the same
name (`Evangelion 1.11 - You Are (Not) Alone`, `is_dir=1`, content mtime
04:57:50), which suggests this mkdir-and-move dance had been attempted earlier —
plausibly followed by a manual cleanup while B1 was making files appear to vanish.

That is a hypothesis, not a finding. It is not provable from the evidence that
survives.

### Fix applied

`trash_child` now logs to the activity feed on both success (`"trashed from the
mount"`) and failure, matching every other trash site. The next occurrence will
be attributable — which is the part that actually mattered here.

### If it recurs

The activity log is now the first place to look. `trash` rows carry no
trashed-at timestamp (the `mtime` column is the node's *content* mtime — for the
`.mkv` it read 06:07:56, matching its upload, not its deletion), so the activity
feed is the only ordering evidence there is.

Two different endings for what looked like the same operation, so B1 may not be
the whole story:

- User's `Evangelion 1.11 - You Are (Not) Alone.mkv` → **trash**, full size
  (1768670449) intact, recoverable via `pdfs restore`.
- Clean repro (`zz-claude-repro-src.txt`) → **gone entirely**. Not in trash, no
  `nodes` row, nothing in the search index.

B1 explains the second. It does not explain a node reaching the trash — nothing
in the FUSE `rename` path calls `trash_nodes`.

Leading theory: the trash came from the sync-conflict machinery, which *is*
logged (`ActivityKind::Trash` rows exist with `(sync-conflict <ts>)` details).
The `~/Videos` folder is registered as a sync folder in `ondemand` mode, and had
just been switched to `ondemand` shortly before. `reconcile_folder` gates on
`mode == "mirror"`, so it should have been inert — worth confirming that gate
actually held, and that no pass was already in flight across the switch.

Note the file was a **conflict-copy sibling** of two files already carrying
`-003` / `-004` suffixes, so the conflict path had definitely been active in that
directory.

**Next step:** find the trash event's origin. The activity log had no row at the
15:58 mv, which points away from a logged (control-socket / sync) path — but the
timestamp bug in B3 made that table hard to read, so re-check with correct
scaling before trusting the absence.

---

## B3 — ~~Activity log timestamps written in seconds, read as milliseconds~~

**Status:** Not a bug — investigator error, kept as a record
**Found / retracted:** 2026-07-19

Originally filed because every activity row rendered as `1970-01-21 …`. That was
an artifact of the ad-hoc debugging query, which divided by 1000; the code never
does. Seconds are consistent end to end:

- writer — `log_activity` uses `now_secs()` (`pdfs-fuse/src/lib.rs`)
- schema / `activity_list` — pass the value through untouched
- reader — `activity_time` calls `glib::DateTime::from_unix_local(secs)`

The correct manual query, for next time:

```sql
select datetime(time,'unixepoch','localtime'), kind, target, detail, ok
from activity order by time desc limit 40;
```

Worth noting because the bad query is what made the activity log look empty
around the reported `mv` — which is evidence B2 leans on. That absence has since
been re-checked with correct scaling and it does hold: no row at 15:58.

---

## B4 — `invalidate_listing` silently skipped non-resident folders

**Status:** Fixed (verified 2026-07-20, via B1's repro — the FUSE rename path that
depends on this helper now behaves correctly end to end)
**Found:** 2026-07-19, reviewing the B1 fix
**Where:** `crates/pdfs-fuse/src/state.rs:257`

```rust
if self.children.remove(&ino).is_none() {
    return;              // never clears the DB `listed` flag
}
```

The early return assumed "not in the hot cache ⇒ nothing to invalidate", which
stopped being true once listings became DB-backed. A folder trimmed from the
in-memory map but still `listed = 1` in the DB could not be invalidated at all —
callers thought they had dropped the listing, and `ensure_children` would happily
rebuild the stale one.

**Fix:** drop the early return; always clear the flag. Costs one redundant
`UPDATE` when nothing was cached.

`Core::refresh_dir` had been hand-rolling a workaround for exactly this (clearing
the DB flag itself, then reaching into `state.children` directly, with a comment
explaining why it couldn't use the helper). It now just calls
`invalidate_listing`. **Behaviour change worth knowing:** `refresh_dir` used to
propagate a DB write failure to the caller of `CtlRequest::Refresh`; it now warns
and reports success, matching every other invalidation site.

**Test:** `a_deleted_child_leaves_a_listed_parent_serving_a_stale_listing` in
`pdfs-core/src/db/tests.rs` pins the B1/B4 mechanism at the DB layer — a deleted
child plus a still-`listed` parent yields a listing that silently omits it.

---

## B5 — `ls -l` costs a network round trip per file (thumbnail xattr probes)

**Status:** Fixed (verified 2026-07-20)
**Found:** 2026-07-19, investigating "`exa -l` is slow in the mounts, `exa` is fast"

### Reproduced (2026-07-19)

All runs cold (35 s wait to clear the 30 s attr TTL). `exa` needs only `readdir`;
`exa -l` stats every entry, so the delta is per-entry metadata cost:

| mount | entries | `exa` | `exa -l` | delta per entry |
|---|---|---|---|---|
| primary `~/ProtonDrive/Installer` | 34 | 10 ms | 11 ms | **0.03 ms** |
| on-demand `~/Documents` | 45 | 27 ms | 54 ms | **0.6 ms** |
| on-demand `~/Videos/[Reaktor] FMA …` | 65 | 73 ms | 193 ms | **1.85 ms** |

### Cause (2026-07-20): a network round trip per file per xattr name

`strace -f -T` on a cold `exa -l` of the 65-file `.mkv` directory:

```
lgetxattr(".../E01 ....mkv", "user.proton.thumbnail", NULL, 0) = -1 ENODATA <0.186435>
```

129 `lgetxattr` and 258 `llistxattr` calls for 65 entries, each `lgetxattr`
~186 ms. Three things compounded:

1. **`listxattr` advertised `user.proton.thumbnail` + `user.proton.preview` for
   every file**, regardless of whether that file could have one. An xattr-aware
   lister then asks for each advertised name — two `getxattr` per entry.
2. **`Core::thumbnail` cached only success.** `download_thumbnail` returning
   `None` — the normal answer for a `.mkv` — was never remembered, so every
   listing re-asked the API and was re-told nothing, forever.
3. **`getxattr` ran inline on fuser's dispatch loop**, not on the `Workers`
   pool. `lookup` and `readdir` had been moved off it (PERF #1.0); this one was
   missed. So each 186 ms miss stalled *every* other op on the mount, which is
   why the concurrency `exa` does have bought nothing.

**The mount-kind correlation was a red herring.** It tracks file *type*, not the
fork: `~/Videos` is `.mkv` (Proton generates no thumbnail — always a miss),
`~/ProtonDrive/Installer` is installers whose probes were already warm. The
`fork_state` theory in the previous write-up is wrong; forked and primary mounts
run the same code and neither is at fault. Worth noting as a lesson — three
measurements on different directories looked like a mount-kind effect because
nobody had varied file type independently.

### Fix

- `listxattr` advertises the names only for `image/*` and `video/*` media types.
  `getxattr` still honours an explicit request for an unadvertised name, so
  nothing becomes unreachable. Everything else — documents, installers, archives
  — now costs zero round trips per listing.
- `Core::thumbnail` remembers misses in `no_thumbnail`, keyed `(uid, type)` and
  validated against the node's mtime exactly as the positive side is. Bounded by
  clearing at `MAX_THUMBNAIL_MISSES` (8192).
- `getxattr` hands off to `Lane::Meta`, so a miss can never block the dispatch
  loop.

A video directory still pays its misses once per revision (image/video is where
a thumbnail plausibly exists, so the probe is legitimate) — but in parallel, and
never again.

### Verified (2026-07-20)

Cold runs (40 s wait each) on the restarted daemon:

| measurement | before | after |
|---|---|---|
| `~/Videos/…FMA…` (65 `.mkv`) cold `exa -l` | 193 ms | **14 ms** |
| `~/Documents` (45 entries) cold `exa -l` | 54 ms | **13 ms** |
| `user.proton.*` probes over the 65 `.mkv` | 130 | **0** |
| `~/Documents` probe latency, 1st listing | 124–395 ms | 124–395 ms |
| `~/Documents` probe latency, 2nd listing | 124–395 ms | **0.26–0.67 ms** |

Each part of the fix is separately visible in the traces:

- **The advertising gate.** The `.mkv` directory now issues *zero* `user.proton.*`
  probes; the only remaining `lgetxattr` is exa's own `security.selinux` at
  ~0.3 ms. Note this means Proton does **not** report those files as `video/*` —
  the gate is excluding them by media type, not because they are known to lack a
  thumbnail. A video whose media type is generic no longer advertises one even if
  it has it; `getxattr` still serves an explicit request, so it stays reachable.
  Worth revisiting if thumbnails ever go missing in a file manager.
- **The negative cache.** `~/Documents` holds 10 `.png` files, correctly still
  advertised, so it probes 20 times on both passes — but the second pass answers
  in 0.26–0.67 ms instead of going to the wire. That is the whole point: the
  probes that are legitimate stop being expensive after the first.
- **The worker handoff.** Pass 1 on `~/Documents` shows interleaved
  `<... lgetxattr resumed>` lines across several tids, i.e. the misses now
  overlap instead of serializing behind the dispatch loop.

### Secondary finding (still open): `lookup` is O(n) per name

**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `serve_lookup`

`serve_lookup` linear-scans the parent's children comparing names, and with no
`readdirplus` that is one `lookup` per child — O(n²) name comparisons under the
global state lock for a listing.

Real, but **not** what made this slow: it costs the same on both mount kinds and
at 65 entries a linear scan is nanoseconds against a 186 ms round trip. Two
candidate optimizations were considered and deliberately *not* applied, and that
judgement still holds now the real cause is known:

- **Per-directory `name -> ino` map.** A second structure that must stay in sync
  with `children` — the two-halves-of-one-cache shape that caused B1 and B4.
- **`readdirplus`** (`fuser` 0.17 supports it; we return `ENOSYS`). The right
  shape long-term, but it is a rewrite of the directory-read path and it would
  have hidden this bug rather than explained it.

### Measurement notes for next time

- Cold requires a 35 s wait per run (30 s attr TTL). Warm runs show ~6 ms
  regardless and prove nothing.
- `strace -c` on `exa` **needs `-f`** — exa is multithreaded, and without it the
  trace captures only main-thread startup (3 `statx` calls, no `getdents`).
- `-T` (per-call durations) is what cracked this; `-c` summaries attribute time
  across threads in a way that hid the 186 ms constant.

---

## B6 — Daemon sets no file modes: `control.sock` is an unguarded authority

**Status:** Fixed (verified 2026-07-20)
**Found:** 2026-07-19, while writing the plaintext-at-rest threat model in
`docs/ARCHITECTURE.md` §8. Not from a report — from checking a claim before
asserting it in a doc. I had written "their default 0700 permissions protect
them", went to verify, and found we set no modes at all.
**Where:** `crates/pdfs-fuse/src/mount.rs` (`UnixListener::bind(control_socket)`),
`crates/pdfs-core/src/config.rs` (`create_dir_all` on state/cache dirs)

`grep -rn "set_permissions\|from_mode" crates/pdfs-core/src crates/pdfs-fuse/src`
returns exactly one hit, and it is `shell.rs` setting `0755` on a generated
script. Nothing else sets a mode. So:

- state and cache directories are created at `0777 & ~umask` — typically `0755`
- `control.sock` is bound at the same, typically `0755`

Anything that can connect to the socket drives the daemon with its authenticated
session: enumerate the tree, read file contents, upload, trash, create public
share links. No credential is required — the keyring is never consulted, because
the daemon already holds the session.

The only thing preventing this today is that `~/.cache` and `~/.local/state` are
conventionally `0700`. That is a property of the user's system, not something we
establish or check. It does not hold on a machine with a permissive umask, a
group-shared home, or a home restored from a backup that flattened modes.

**Severity:** low on a single-user desktop, real on a shared or multi-user host.
It is a privilege boundary rather than a data-at-rest issue, which is what makes
it worth more than the cache-plaintext point it was found next to.

**Fix:** `chmod 0600` on the socket immediately after `bind` (before the listener
thread starts accepting), and `0700` on the state and cache directories at
creation. Both are a few lines. Worth also asserting the socket mode in a test,
since a regression here is silent.

**Fixed 2026-07-19:** `AppDirs::ensure` now sets `0700` on the state, cache, and
config directories on *every* start (not just at creation, so an existing
permissive directory is tightened), and `config::restrict_socket` sets `0600` on
both the control socket and the tray socket immediately after `bind`. A socket
whose mode cannot be set takes the daemon down rather than serving unguarded;
the tray's is best-effort, since it only guards single-instancing. Unit tests in
`config.rs` assert both modes.

**Verified 2026-07-20** on the restarted daemon — all three directories now read
`drwx------` and both sockets `srw-------`, against `drwxr-xr-x` / `srwxr-xr-x`
before.

**Measured exposure at the time of the fix:** the directories really were
`drwxr-xr-x` and the live socket `srwxr-xr-x`, but `~/.cache` and `~/.local`
were both `0700` on this machine, so nothing was reachable in practice. The bug
was a latent dependency on those parents, not a live hole. Removing the
dependency is the point.

---

## B7 — renamed directory reads as missing until the entry TTL expires

**Status:** Fixed (verified 2026-07-20)
**Verified:** `mkdir "zz-b7 Old Name"` with a file inside, `mv` to a new name, then
an immediate `ls` and `cat` through the new name — both succeed with no wait. The
directory used to return ENOENT until the entry TTL expired.
**Found:** 2026-07-19, user renamed a folder on the mount with `mv`. `ls` of the
parent listed the new name, but `ls <newname>/` returned ENOENT — repeatedly, so
not a one-shot race. It self-healed on its own a few minutes later, and the
files were all intact: this is a visibility bug, not data loss.
**Where:** `crates/pdfs-fuse/src/state.rs`, `State::relocate`

**Cause:** the online rename path ended with `relocate`, which called `forget` on
the moved node. `forget` drops the node's `by_uid` mapping, so when the
invalidated parent listing re-enumerated, `intern_mem` allocated it a **fresh**
inode. But the kernel had already carried the renamed dentry over to the *old*
inode number, and it holds that dentry for the entry TTL. Every lookup, getattr
and opendir through it resolved to an inode `entries` no longer held:

    entries.get(old_ino) -> None -> ENOENT

`readdir` of the parent went the other way — it walks `children` and reports the
*new* inode — which is exactly why the directory listed fine but could not be
entered. Once the TTL expired the kernel re-looked-up the name, got the new
inode, and everything worked again.

**Fix:** `relocate` now rewrites the node in place (`rename_in_place`) and
invalidates both parents' listings, instead of forgetting it. The inode is
preserved, so the kernel's dentry stays valid and the re-enumeration reuses the
same `by_uid` slot. Both `relocate` tests now assert inode stability; that
property was untested, which is how this got through the B1 fix.

**Note:** dropping the `forget` also stops the moved node's DB row from being
deleted and re-created on every move — the row is updated instead, which is what
the B1 fix was working around from the other side.

---

## B8 — no way to complete a CAPTCHA, so a gated sign-in is unrecoverable

**Status:** Implemented, partially verified — the bridge is proven, a real gate is not
**Found:** 2026-07-19, user hit `proton api error Unknown (http 422): For
security reasons, please complete CAPTCHA` and had no way to answer it.
**Where:** SDK `api.rs`/`http.rs`/`session.rs`, `pdfs-core/src/auth.rs`,
`pdfs-gui/src/app/pages/verify.rs`

**Cause:** three gaps stacked, none of which is a bug on its own.

1. `ResponseCode` had no `9001`, so the gate deserialized to `Unknown` — which
   is why the error read as an opaque failure rather than a recoverable prompt.
2. Nothing read `Details.HumanVerificationToken`, and the client never sent the
   `x-pm-human-verification-token{,-type}` headers the retry needs.
3. No UI could render Proton's hosted verification page.

**Fix:** `HumanVerification` (challenge) + `HumanVerificationCredential`
(answer) in the SDK, header plumbing on the session-less login calls, and
`ProtonApiSession::begin_verified`. `pdfs-core` promotes a solvable gate to
`Error::HumanVerificationRequired` and `auth::login_interactive` re-runs the
login with the earned token — the retry lives in core because the gated attempt
burns its SRP handshake, which no front-end should have to know. The GUI hosts
the page in a `WebKitWebView` (webkit6 0.4, pairs with the existing gtk4 0.9)
and bridges the page's `postMessage` to a script message handler.

**Deliberately not done:** `email`/`sms` verification methods (the token arrives
out of band, so the webview cannot complete them — such a gate stays a plain API
error rather than opening a page the user cannot finish), HV on *authenticated*
endpoints (only the login path is plumbed), and re-gating of the retry (a second
challenge means the token was rejected; looping would trap the user).

**Verified:** the WebKit bridge end-to-end with a throwaway probe — handler
registers, the page's `postMessage` arrives, non-completion messages are
filtered, the completion token round-trips. Unit tests cover 9001 parsing, the
challenge/`Details` shape, URL escaping, and message filtering.

**Not verified:** a real gated login against verify.proton.me. It cannot be
triggered on demand, so the exact message shape Proton's page posts is taken
from its documented contract, not observed. If verification appears to hang with
the page solved, that is the first thing to check — `extract_token` in
`verify.rs` is the single place that decides what counts as completion.

**Note:** the SDK half shipped as proton-sdk / proton-drive-rs **0.1.11**. The
workspace requirement was widened to `"0.1"` at the same time, so later 0.1.x
releases are picked up by `cargo update` without an edit here.

---

## B9 — Enter did not open the selected result in the launcher

**Status:** Fixed (verified)
**Found:** 2026-07-19, user reported Enter doing nothing in `pdfs-prompt` while
the arrow keys and Escape worked normally.
**Where:** `crates/pdfs-gui/src/prompt.rs`, the window key controller

**Cause:** key-event *phase*, not key handling. `gtk4::Entry` — really its inner
`GtkText` — binds Return to its own `activate` and consumes it. The launcher's
`EventControllerKey` is attached to the **window** in the default **bubble**
phase, so the focused entry saw Return first and the window handler was never
reached. Escape and Up/Down worked precisely because `GtkText` has no bindings
for them and they bubbled up as intended.

That asymmetry is the tell: when some keys reach a window-level controller and
others silently don't, the ones that don't are being claimed by the focused
widget.

**Fix:** Return moved off the window controller and onto
`entry.connect_activate`. Enter while focus is in the results list was already
handled by `row_activated`, so the two together cover both focus positions.

**Rejected alternative:** setting the window controller to
`PropagationPhase::Capture`. It fixes the symptom by putting the handler ahead
of the entry — but ahead of it for *every* keystroke, not just Return, which
puts text input and IME composition behind a handler that has no business
seeing them. The narrow fix has no such blast radius.

**Verified:** live. Pressing Enter logged `opening path=…`, the daemon hydrated
the file, and `xdg_open` ran.

---

## B10 — GLib critical when the launcher closes over an in-flight open

**Status:** Open (side finding)
**Found:** 2026-07-19, seen in `pdfs-prompt` output immediately after a
successful Enter-to-open while verifying B9.

```
GLib-GIO-CRITICAL: g_list_store_remove: assertion '!g_sequence_iter_is_end (it)' failed
```

**What is known:** it fires right after the `opening path=…` log line, i.e.
during `xdg_open` + `window.close()`. It is a warning, not a crash — the open
itself succeeded.

**What it is not:** the launcher's own row handling. `Section::set_rows` walks
`GtkListBox` children (`first_child`/`remove`) and touches no `GListStore`, so
the failing store belongs to GTK/libadwaita internals, most likely something
teardown-ordering related in the app/window bookkeeping as the window is closed
while a launch is still settling.

**Why it was not chased:** it appeared while verifying an unrelated fix and has
no user-visible effect. Worth revisiting if the launcher ever misbehaves on
close (a hang, a lost open, or a stale window), since an assertion firing during
teardown is exactly the shape of bug that later turns into one of those.

**Where to start:** run `pdfs-prompt` under `G_DEBUG=fatal-criticals` to turn
the warning into an abort and get a real backtrace naming the store.

## B11 — a file moved while its create is still queued reads as empty

**Status:** Fixed (verified 2026-07-20)
**Found:** 2026-07-20, while verifying B1 on the restarted daemon.

The B1 repro is `echo hi > f.txt && mv f.txt d/`, i.e. the move lands while the
create is still queued for upload. Immediately after the `mv`, the file is
present in the destination listing — B1 is genuinely fixed — but stats as **0
bytes** and `cat` returns nothing:

```
-rw-r--r-- 1 narl narl 0 Jul 20 00:29 zz-b1-dst/zz-b1-src.txt
```

A few seconds later the same file reads correctly (`3` bytes, `hi`). A control
file created and read *without* an intervening `mv` was correct immediately.

**Cause:** `State::intern_mem` replaces an existing entry's node wholesale
(`e.node = node`). A move invalidates both parents' listings (that is B7's fix,
and it is correct), so the next `ls` re-enumerates — and the node that comes back,
from the remote or from its DB row, carries the size of the revision the *server*
holds. For a file whose write is still queued that is the pre-write size, usually
0. Interning it reverts the optimistic size `record_pending_write` had stamped.

**Why an empty read rather than a short one:** a file that stats as 0 bytes gets
**no `read` from the kernel at all**. `read_range` would have served the staged
blob quite happily; it is never asked. So the file reads as empty for as long as
the stale size stands, and "empty file" is indistinguishable from "file whose
contents were lost" to whatever is reading.

`Core::hydrate` already solved exactly this for the *restart* case — it stamps
each pending write's size onto the node as entries materialize. The protection
was simply missing on every live re-enumeration path.

**Fix:** `Core::stamp_pending_sizes` re-applies the optimistic size to a batch of
nodes, called from both arms of `ensure_children` (the DB fast path and the
network path) before the state lock is taken. It snapshots the pending map first
and returns: no site in the daemon holds `pending` and `state` at once, and this
is not the place to become the first — `hydrate` established that pattern for the
same reason.

Scoped to `ensure_children` deliberately. The other `intern` sites are
single-node and all authoritative by construction (mkdir, create, an upload that
just landed); `drain.rs`'s post-upload adoption is *supposed* to take the
server's node, and already handles the queued case through `rebaseline_pending`.

**Tests:** `pending_size_tests` in `pdfs-fuse/src/lib.rs` covers the queued file
keeping its size, a settled sibling keeping the server's, folders being left
alone, and an empty pending map changing nothing.

**Verified 2026-07-20** on the rebuilt daemon: `echo hi > f.txt && mv f.txt d/`
then an immediate `ls -l` and `cat` gives 3 bytes and `hi`, against 0 bytes and
empty output before.

---

## B12 — cold enumeration is slow per entry, and goes superlinear past ~500

**Status:** Both causes fixed and verified (2026-07-20); attr-invalidation follow-up unverified
**Found:** 2026-07-20, measuring improvements.md P2.8 ("pipeline
`enumerate_nodes_detail`") before implementing it, to decide whether the
bottleneck was network or crypto.

### Measured (cold; `pdfs refresh <dir>` then a timed `ls -1`)

| entries | time | per entry |
|---|---|---|
| 1–9 | 350–550 ms | fixed cost |
| 34 | 629 ms | — |
| 147 | 1.92 s | 12.5 ms |
| 251 | 3.18 s | 12.4 ms |
| 484 | 6.25 s | 12.5 ms |
| **793** | **38.8 s** | **48.9 ms** |

The 793 figure reproduces to ±100 ms across three runs. Marginal cost from 484 to
793 is **106.7 ms/entry**, 8.5× the baseline.

`perf record` against the running daemon shows **~90 % of cycles inside the
`pdfs` binary** in both cases, and sample counts matching wall time (1022 samples
≈ 5.1 s, 7538 ≈ 37.9 s at 199 Hz) — so this is CPU-bound throughout, not waiting
on the network. Symbols are unavailable (the installed binary is stripped), which
is what blocks attribution.

### Cause 1 (found, ~half the baseline): every FTS row is deleted by a full scan

`nodes_fts` declares `uid` as **UNINDEXED**:

```sql
CREATE VIRTUAL TABLE nodes_fts USING fts5(uid UNINDEXED, name, tokenize='trigram')
```

and `Db::upsert_nodes` (`pdfs-core/src/db/nodes.rs`) deletes by exactly that
column on every node, because "FTS5 has no UPSERT":

```sql
DELETE FROM nodes_fts WHERE uid = ?1
```

An UNINDEXED FTS5 column is not searchable, so the predicate cannot use an index.
`EXPLAIN QUERY PLAN` confirms it:

```
`--SCAN nodes_fts VIRTUAL TABLE INDEX 0:
```

One full scan of the FTS index per node written. Measured on a `.backup` copy of
the live 171 MB DB (17 443 indexed nodes):

| deletes | time | per delete |
|---|---|---|
| 484 | 3.09 s | 6.4 ms |
| 793 | 5.26 s | 6.6 ms |

So **~6.5 ms of the 12.5 ms per-entry baseline is this one statement**, and it
gets worse for every user as their node count grows — the scan is over the whole
index, so the cost of writing *any* listing scales with the size of the *account*.
It is also paid by every sync pass and every event-driven refresh, not just `ls`.

**Fix direction:** delete by rowid instead. FTS5 deletes by rowid efficiently, so
map `nodes.rowid` to the FTS rowid and drop the `uid` column from the index
(or keep it UNINDEXED purely for retrieval). Needs a schema migration on a live
171 MB DB, so it wants its own change rather than being smuggled into this one.

### Cause 2 (the dominant one): an S2K key derivation per file, to list a folder

**Symbols came from the local unstripped build, not a new install.** `strip`
preserves both the build id and the text addresses, so when `/usr/bin/pdfs` and
`target/release/pdfs` reported the *same* build id, the stripped binary was just
a copy of the local one and `perf buildid-cache -a target/release/pdfs` was
enough to symbolise a recording of the running daemon. Worth remembering — it
turned a "needs another install + restart" into a five-second step.

Hot functions over a 793-entry cold enumerate:

| % | symbol |
|---|---|
| 64.4 | `sha2::sha256::x86::digest_blocks` |
| 6.0 | `<D as digest::DynDigest>::update` |
| 5.7 | `sqlite3VdbeExec` (cause 1, above) |
| 4.0 | `sha2::sha256::compress256` |
| 1.5 | `pgp::types::s2k::StringToKey::derive_key` |

**~74 % of the listing is SHA-256 inside PGP's S2K**, i.e. the per-file node-key
unlock that `build_node` does under `NodeDetail::Full`. `derive_key` itself shows
1.5 % because the time lands in its SHA leaf. Nothing downloads during an `ls`, so
PGP is the only plausible source of that SHA.

### The "cliff past ~500" framing was wrong

That was an inference from a single folder, not a measurement. An S2K's cost is
set by the iteration count in the key packet, which is chosen by *the client that
uploaded the file* — so it varies per file, not per listing size. Every folder
measured sits at 12–16 ms/entry except `Music/aC_ID.dll` at 49 ms, and
`Music/ELDEN RING SOUNDTRACK` (67 files, same parent tree) is ~16 ms/entry, so it
is not "Music files are expensive" either. A 4× jump for a 1.6× change in n fits
"those files were uploaded by a client using a costlier S2K" far better than a
size threshold. n and folder identity are confounded in the data — there was only
ever one folder above 500 entries.

Left unresolved deliberately: the fix below removes the per-file S2K from the
listing path entirely, so which of the two explanations held stopped mattering.
To settle it anyway, log the S2K iteration count per file across one enumerate of
each folder.

### Fix: enumerate cheap, upgrade sizes in the background

`ensure_children` now calls `enumerate_nodes_light`, which skips the file
node-key unlock (folders are still unlocked — their keys are what the children
decrypt with). The listing is served from that immediately.

`Light` returns no `claimed_size`, and `ls -l` wants sizes, so a naive split would
just move the same S2K onto the first `stat`. Instead `Core::spawn_size_upgrade`
fetches the full nodes for that folder on a worker, batched, after the listing has
been answered, and adopts *only* the size — re-interning wholesale would clobber a
name or parent that a concurrent rename/move had changed. It is single-flight per
folder, because a `stat` of one entry in a fresh listing means a `stat` of all of
them. Queued writes keep their optimistic size through it, via the same
`stamp_pending_sizes` that B11 added.

Three entry points cover every way a provisional listing can appear: the network
enumeration, the DB fast path (rows persisted before an upgrade ran), and
`getattr` itself (a listing restored by `hydrate` on mount, which `ensure_children`
returns early for).

**Known tradeoff — sizes are provisional until the upgrade lands.** `node_size`
falls back to `total_size_on_storage`, the ciphertext size, so a `stat` in that
window reads slightly *too large*. Reads are unaffected: the revision reader
carries its own authoritative size. Deliberately not the B11 shape — that
reported **0**, which made the kernel skip reads entirely; too-large is cosmetic
and self-corrects.

**The window was longer than "a round trip", and that took a correction.** The
daemon has the real sizes quickly, but the *kernel* keeps the provisional attrs
for the full 30 s entry TTL, so `ls -l` kept reporting them long after the DB was
right. This nearly caused a misdiagnosis during verification: two successive
`ls -l` runs agreed with each other and both disagreed with the truth by a
constant +59 bytes, which reads exactly like "the upgrade never ran". Querying the
DB directly is what separated "not computed" from "computed but not visible".

Closed by having `spawn_size_upgrade` call `notifier.inval_inode` for the inodes
it corrected, after the DB write so a provoked re-`getattr` cannot race the
persistence. That needed a `Notifier` on `Core` (a `OnceLock`, since the session
is built *from* the `Core`), and a fresh one per on-demand fork — each fork has
its own inode space, so notifying through the primary mount's channel would name
inodes that session has never heard of.

### Verified (2026-07-20)

| folder | before | after |
|---|---|---|
| `Music/aC_ID.dll` (793) | 38.8 s | **4.48 s** (8.7×) |
| `InstantUpload/Camera` (484) | 6.25 s | **2.91 s** (2.1×) |
| `Pictures/old` (417) | 5.10 s | **2.25 s** (2.3×) |

Schema v15 migrated the live 171 MB DB on start (17 443 rows indexed). All 793
sizes converge to exactly their pre-change values, checked against a `.backup`
copy taken before any of this landed. The +59-byte ciphertext delta is a precise
probe for a provisional size, and is what the attr-invalidation follow-up should
be tested with.

### Original cause-2 ruling-out (kept — all still true)

Backing cause 1 out of the totals leaves per-node non-FTS cost at **6.2 ms at
n=484 but 42 ms at n=793**. Something gets ~7× more expensive per node, sharply,
somewhere around 500–800 entries.

Ruled out so far:

- *Chunking / network.* `MAX_BATCH_COUNT = 150`, so cost would rise in visible
  steps at 150/300/450; it is flat at 12.5 ms/entry straight through 484. The
  marginal cost would imply ~16 s per POST, which is absurd.
- *Composition.* Both the 484 and 793 folders are 100 % files, no subfolders.
- *`FOLDER_KEY_CACHE_CAP` (512).* Suspicious number, but `folder_keys` only holds
  *folder* keys and these children are all files, so nothing is inserted during
  the walk. `resolve_parent_key_ctx` caches each ancestor it derives, and all
  children share one parent, so it is a hit after the first child.
- *The entity cache.* `InMemoryCacheRepository` is `HashMap`-backed, O(1) `set`.
- *FTS (cause 1).* Linear — 3.1 s → 5.3 s where the cliff is 6.1 s → 38.8 s.

**Next step:** symbol-level profile. Needs an unstripped `pdfs` installed and the
daemon restarted, then `perf record -p <pid>` across a 793-entry enumerate. Every
cheaper avenue above has been spent. Note a second daemon is **not** an option
for this: Proton refresh tokens are single-use, so a parallel session would fight
the running daemon for them.

**Do not implement improvements.md P2.8 before this is understood.** Pipelining
would parallelize the cliff rather than remove it, and at 793 entries the cliff is
33 s of the 38.8 s — far more than concurrency could win back.

---

## B13 — `rename` over an existing destination fails instead of replacing it

**Status:** Fixed (unverified — needs a real rsync against the rebuilt daemon)
**Found:** 2026-07-20, user ran `rsync -rauLP ~/ProtonDrive/Music/... ~/Music/`
(both are protondrive mounts) and every single file failed at the end of its
transfer:

```
rsync: [receiver] rename ".../.Buunshin - heimwee (Original Mix).wav.b0akm3"
    -> ".../Buunshin - heimwee (Original Mix).wav": Input/output error (5)
```

**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `Filesystem::rename`

Daemon log, one per failed file:

```
ERROR pdfs_fuse: rename failed uid=… error=proton api error AlreadyExists (http 422):
    A file or folder with that name already exists
WARN  pdfs_fuse::drain: pending upload failed; will retry uid=… attempts=1
    error=proton api error DoesNotExist (http 422): File or folder not found
```

**Cause:** `rename(2)` is specified to *atomically replace* an existing
destination. Our handler just calls `client.rename_node`, and Proton refuses a
name that already exists — so the 422 becomes a blanket `EIO`. Nothing in the
path ever looks up the destination name.

This breaks every write-to-temp-then-rename tool, which is most of them: rsync,
editors doing atomic saves, `mv -f`, package managers. The failure is at the
*end* of the transfer, so the bytes are uploaded and then thrown away — the user
pays full upload cost for nothing.

**Second-order damage:** the failed rename leaves the temp node behind and its
queued upload then fails with `DoesNotExist` and retries forever. So each failed
file also leaves a poisoned entry in the drain queue.

**Fix:** `rename` now looks up `newname` under `newparent` and, if it resolves to
a different node, removes it before the API call.

The order is forced by Proton: the name has to be free before `rename_node` will
take it, so it is trash-then-rename, which means the operation is **not atomic**.
`Core::remove_replaced` does the trashing and `Core::restore_replaced` puts the
node back if the rename then fails — on every failing path, including the queued
ones. If the restore *also* fails the node stays in the trash and says so
loudly, because the alternative is a file the user believes was merely renamed
sitting somewhere they will not look.

Deliberate details:

- **Refusals happen before anything is trashed.** `check_replaceable` is a pure
  function for exactly this reason: every `Err` it returns is a case where the
  destination must survive, and a mistake there turns a refusal into deletion.
  `EISDIR` / `ENOTDIR` when the two ends disagree about being a directory, and
  `ENOTEMPTY` for a non-empty destination directory — Proton trashes a folder
  with its whole subtree, so allowing that would discard every file underneath
  without ever naming them.
- **`RenameFlags` is now read** (it was `_flags`). `RENAME_NOREPLACE` returns
  `EEXIST` instead of replacing; `RENAME_EXCHANGE` returns `EINVAL`, since no
  Proton primitive swaps two names and emulating it would leave a window in
  which one of them does not exist.
- A node whose own create is still queued never reached the server, so replacing
  it just discards its queued ops — no API call, and it works offline.
- Renaming a node onto its own name stays a no-op rather than a self-replace.

**Tests:** `replace_tests` in `pdfs-fuse/src/lib.rs` covers all five decisions.
The trash/restore half needs a live server and is not covered.

**Repro:** `cd <mount> && echo a > x && echo b > y && mv y x` → was EIO, should
now succeed with `x` holding `b`.

---

## B14 — provisional (ciphertext) sizes make rsync read short and abort the file

**Status:** Fixed (unverified — needs a cold folder on the rebuilt daemon)
**Found:** 2026-07-20, same rsync run as B13. Alongside the receiver errors, the
sender failed to read its own source files:

```
rsync: [sender] read errors mapping "/home/narl/ProtonDrive/Music/…/
    Buunshin - i think i feel... (Original Mix).wav": No data available (61)
```

**Where:** `node_size` (`pdfs-fuse/src/lib.rs:3480`) + `Core::spawn_size_upgrade`

**Cause:** this is B12's known tradeoff surfacing as a real transfer failure.
Until the background size upgrade lands, `node_size` falls back to
`total_size_on_storage` — the **ciphertext** size, which is larger than the
plaintext (measured at +59 bytes on this account). B12 called that "cosmetic and
self-corrects" because it only affects `ls -l`.

It does not only affect `ls -l`. `ENODATA` (61) is what **rsync sets on a short
read**: `map_ptr` in `fileio.c` asks for the bytes `stat` promised, gets fewer,
and marks the mapping `ENODATA`. So a reader that trusts `st_size` — rsync,
anything using `mmap`, `sendfile`, or a sized `read` loop — sees a truncated
file and errors out. That is the same class of bug as B11, just from the other
direction: B11 reported **too small** (0) so the kernel skipped reads entirely;
this reports **too large** so reads run off the end.

The 30 s attr TTL makes the window much wider than the upgrade's own latency
(B12 documented this), so a cold `rsync` of a large tree is essentially
guaranteed to hit it.

**Why it looked fine afterwards:** re-listing the same directory now shows every
size correct, because the upgrade has long since landed. The bug is only visible
cold. Reproducing it needs `pdfs refresh <dir>` immediately before the read.

**Fix:** a provisional size is never published. `getattr` on a file whose
`claimed_size` is unknown now resolves it *before* replying, instead of replying
with the ciphertext size and upgrading afterwards.

The cost is one batched round trip per folder, not one per file: `ls -l` is one
`getattr` per entry, and they collapse onto a single upgrade. B12's split is
still doing its job — a plain `ls` never reaches this path at all, which is
where the 8.7× came from.

That required making the upgrade *awaitable*, which is most of the change:

- `size_upgrades` went from `HashSet<u64>` to `HashMap<u64, Arc<SizeUpgrade>>`,
  a condvar per folder. Followers wait on it; there may be hundreds for one
  folder, so it releases all of them.
- **The leader does the fetch on its own thread rather than handing it to a
  worker.** This is the part to not undo: `getattr` waits on `Lane::Meta`, so a
  leader that queued its fetch onto that same lane could have a wide enough
  `ls -l` fill the lane with threads waiting for a job that can never be
  scheduled.
- `SizeUpgrade::WAIT` caps the wait at 10 s, falling back to the provisional
  size. A `stat` that never returns is worse than one that is briefly wrong.
- `upgrade_sizes` owns the single-flight bookkeeping and has one exit path;
  `apply_size_upgrade` holds the body it used to inline, so a failed fetch still
  releases the waiters and leaves the folder retryable.

**Tests:** `size_upgrade_tests` covers the follower being released, a waiter
arriving after `finish` (it checks the flag, not the notification, which it
would miss), and all waiters waking.

**Not covered, worth watching:** `Lane::Meta` can now hold waiting `getattr`s.
There is no deadlock — leaders never queue their own work — but a cold `ls -l`
across many folders at once could occupy the lane. If metadata ops start
feeling sticky under heavy cold listing, this is the first suspect.

### Verified (2026-07-20) — correct, but expensive, and not completely closed

Two further holes turned up during verification; both are fixed, and the second
is the one that mattered.

1. **`upgrade_sizes_for_parent` early-returned** when the parent listing was not
   resident in `children` — and a rename invalidates exactly that, so a freshly
   renamed file always landed in the dead branch. `stat` read 67 bytes for a
   16-byte file, and `cat` returned the 16 real bytes followed by 51 NULs. The
   same shape as B4: an early return that assumed the hot cache was
   authoritative. Now falls back to resolving the single node, keyed by its own
   inode.
2. **`lookup` replies with attrs and a TTL too.** With no `readdirplus`, `ls -l`
   is one `lookup` per entry and *zero* `getattr` calls — so fixing only
   `getattr` fixed a path `ls -l` never takes. This is worth remembering: the
   first fix looked right and did nothing for the reported symptom.
   `serve_lookup` now resolves as well, taking an `off_loop` flag because
   `lookup`'s warm path runs on the dispatch loop where it may not block (B5).

| measurement | before fix | after fix |
|---|---|---|
| cold `ls -l`, 7-file wav folder | ciphertext sizes | **exact settled sizes** |
| renamed file `stat` / `cat` | 67 B, 51 NULs | **16 B, clean** |
| cold plain `ls`, 793 entries | 4.48 s | **4.58 s** (B12 intact) |
| cold `ls -l`, 793 entries | ~4.5 s, wrong sizes | **85.8 s**, 785/793 correct |

### The cost, and the part still open

**A cold `ls -l` of a 793-entry folder went from ~4.5 s to 85.8 s.** That is not
a flaw in the batching — it is B12's cause 2 arriving on schedule. A real
`claimed_size` requires the per-file node-key unlock, i.e. one S2K per file, and
that work is single-threaded. B12 removed it from the *listing* path; asking for
sizes puts it back, because sizes are what it produces. Plain `ls` is untouched,
which is why the split is still worth having.

**8 of 793 entries still came back provisional**, caught by diffing the cold
listing against a settled one. Those are `SizeUpgrade::WAIT` timeouts: the leader
takes ~80 s for the batch and a waiter gives up at 10 s. So the bug is rarer but
not gone — it now needs a folder large enough that the batch outruns the cap.

### Per-chunk wakeup (done, unverified)

The timeout gap is closed by releasing waiters **per chunk** instead of once at
the end. `SizeUpgrade` carries a generation counter; `run_size_upgrade` fetches
in `SIZE_UPGRADE_CHUNK` (150, the SDK's own `MAX_BATCH_COUNT`, so one chunk is
one request), applies each chunk, and bumps the generation. A waiter re-checks
whether *its own* node now has a real size and returns if so — it no longer
waits on the other 792.

Two structural points worth keeping:

- **The batch runs on its own thread, not the `Workers` pool.** Callers wait on
  `Lane::Meta`, so queueing the batch there could let a wide `ls -l` fill the
  lane with threads waiting for a job that can never be scheduled.
  `Lane::Transfer` would swap that deadlock for starvation behind bulk reads.
  One short-lived thread per folder, bounded by the single-flight, avoids both.
- **`wait_for` evaluates its predicate with no `SizeUpgrade` lock held.** The
  predicate reads `state`, and the applying thread holds `state` before it
  signals; taking them in the other order would close the cycle.

This does **not** make a full cold `ls -l` faster — every size still has to be
computed. It makes each individual `stat` return after its own chunk rather than
after the whole folder, and removes the provisional-size fallback that the 10 s
cap was producing.

**Tests:** `size_upgrade_tests` covers the resolving chunk releasing a waiter,
an unrelated chunk *not* releasing one, `finish` releasing an unresolved waiter,
the already-resolved and post-`finish` arrivals, all waiters waking, and the
timeout backstop still firing.

---

## Plan: parallelise the per-file S2K (the 85 s)

This is the only item that reduces the work rather than redistributing it, and
it is the same bottleneck as improvements.md PERF #5 — approached from sizes
rather than from cold navigation. Filed here because B12/B14 are what produced
the measurements it needs.

**The claim to be tested first, before any code.** B12's profile of a 793-entry
cold enumerate attributed **64.4 % of cycles to `sha2::sha256::x86::digest_blocks`
and ~74 % overall to SHA-256 inside PGP's S2K**, i.e. the per-file node-key
unlock in `build_node` under `NodeDetail::Full`. If that still holds for the
*size upgrade* path specifically — it is the same `enumerate_nodes` call, so it
should — then the work is CPU-bound, embarrassingly parallel across files, and
currently serialised on one thread.

**Step 0 — confirm, do not assume.** Re-profile against the size-upgrade path
now that it is the thing on the critical path:

```
perf buildid-cache -a target/release/pdfs      # symbols; see B12's note on strip
perf record -p <daemon pid> -- sleep 90
# meanwhile: pdfs refresh Music/aC_ID.dll && ls -l Music/aC_ID.dll
```

Expect the same SHA-256 leaf. If it is *not* dominant, stop — the rest of this
plan is aimed at the wrong thing.

**Step 1 — establish the ceiling.** The upgrade is `enumerate_nodes` over 793
uids. Measure single-node unlock cost directly (log the S2K iteration count and
elapsed time per file across one folder — this also settles the question B12 left
open, whether the 49 ms/entry folder is expensive because of *n* or because the
uploading client chose a costlier S2K). Ideal speedup is bounded by core count
and by how uneven those per-file costs are.

**Step 2 — move the crypto off the async runtime.** The decryption currently runs
inline in the SDK's `enumerate_nodes` future. Blocking CPU work on a Tokio worker
starves everything else sharing that runtime. Wrap the per-node unlock in
`spawn_blocking`, or hand the whole chunk to a `rayon` pool and await the result.
This is the change PERF #5 describes and is a **`proton-drive-rs` change, not a
`pdfs` one** — which is why it was deferred: it needs an SDK release, and the
workspace pins `"0.1"` so a `cargo update` picks it up.

**Step 3 — parallelise across files within a chunk.** 150 nodes per chunk, each
independent once the parent key is resolved. `resolve_parent_key_ctx` caches the
ancestor key and all children of a folder share one parent, so the parallel
region needs read-only access to it — take the folder key once, then fan out.
Watch `FOLDER_KEY_CACHE_CAP`/`folder_keys` (PERF #9 made it an LRU) for
contention if the fan-out takes that lock per node.

**Step 4 — re-measure the same three folders**, which have before/after numbers
already recorded above and in B12:

| folder | cold plain `ls` | cold `ls -l` (today) |
|---|---|---|
| `Music/aC_ID.dll` (793) | 4.58 s | 85.8 s |
| `InstantUpload/Camera` (484) | 2.91 s | not measured |
| `Pictures/old` (417) | 2.25 s | not measured |

Fill in the middle column's `ls -l` first — there is only one data point for the
regression right now, and B12's "cliff past ~500" framing was wrong precisely
because it generalised from a single folder. **Do not repeat that mistake here.**

**What success looks like:** cold `ls -l` on the 793-entry folder within ~2–3× of
the plain `ls`, rather than 19×. Perfect scaling is not available — the request
round trips are serial per chunk and only the crypto parallelises.

**Explicitly out of scope:** `readdirplus`. It would cut the *number* of calls
(one reply per directory instead of one `lookup` per entry) and is the right
long-term shape for the directory-read path, but it does not remove a single S2K
— the sizes still have to be computed. It is a separate change and should not be
bundled in, for the same reason B5 declined it: it would hide this bug rather
than fix it.

**Do not** attempt to derive the plaintext size from `total_size_on_storage`
instead. The delta is not a constant (+564 B on a 39 MB file, +51 B on a 16-byte
one — it scales with block count), so the arithmetic would have to model Proton's
block framing exactly, and being wrong in the *small* direction reproduces B11,
where a short size made the kernel skip reads entirely.

**Verify with:** the ciphertext delta is the probe (+564 B on a 39 MB file,
+51 B on a 16-byte one — it scales with block count, so it is not a constant).
`pdfs refresh` a folder, then `ls -l` immediately and diff against a settled
listing.

---

## B15 — an empty-but-`listed` folder, and duplicated folder uids

**Status:** Open (side finding, unattributed). Originally filed as "a sync-folder
listing came back empty"; that part was investigated and retracted — see below.
**Found:** 2026-07-20, while confirming B13.

`ls ~/Music/Buunshin/not everythin is your fault (wav)/` returns `total 0` — no
entries — even though the rename failures in B13 prove files existed under those
names minutes earlier (Proton rejected the rename *because* the name was taken).
The same folder read through the primary mount
(`~/ProtonDrive/Music/Buunshin/…`) lists all seven `.wav` files correctly.

**Checked (2026-07-20) — the "stale empty listing" reading was wrong.** The
folder `not everythin is your fault (wav)` that read empty has **`listed = 0`**
in `nodes`, so its `ls` was a real network enumeration, not a cached empty one.
The files genuinely are not there: every rename in B13 failed and rsync
discarded its temp files. B15 as originally filed does not exist.

**What the query did turn up, and is worth keeping:**

1. **A real B1-shaped row.** `not everything is your fault` (no `(wav)`) reads
   `listed = 1, trashed = 0` with **zero child rows** — the exact
   empty-but-listed state B1 was about, still present in the live DB, reached by
   some route the B1 fix does not cover. This one is worth chasing.
2. **Duplicated folders.** `Music` and `Buunshin` each exist under **two
   distinct uids**, and the `(wav)` folder name appears twice as well — one copy
   populated (7 children), one empty. So the remote tree has picked up duplicate
   directories somewhere. Related to the self-conflict duplication already
   recorded in memory, but not yet attributed here.

Both are folder-identity problems rather than the mount disagreement originally
suspected. Renamed scope accordingly; the "two mounts disagree" framing is
retracted.

---

## B16 — aria2c preallocation / rapid sequential writes trigger false self-conflict copies

**Status:** Fixed (verified 2026-07-21)
**Found:** 2026-07-21, user reported sync conflict files created during torrent downloads with aria2c onto the FUSE mount (`[DB]Oshi no Ko 3rd Season_-_05... (sync-conflict 1784644482).mkv`).
**Where:** `crates/pdfs-fuse/src/lib.rs`, `crates/pdfs-fuse/src/drain.rs`, `crates/pdfs-core/src/db/ops.rs`

**Cause:** Tools like `aria2c` preallocate target file sizes using `ftruncate`/`setattr` and then write actual content across multiple file opens or handle releases.
1. First close (e.g. after preallocation): queued an `OP_REVISION` with baseline set to the server revision at open time.
2. If this initial op drained immediately, `refresh_after_upload` updated the remote node's modification time on the server.
3. Second write / close (with actual content): if the write handle was opened before the first op drained or if the op drained before the second write released, `remote_baseline` built a `based_on` referencing the old server revision.
4. When the second op drained, `revision_conflict` compared `based_on` against the new remote revision mtime, detected a mismatch, and treated it as a remote edit by another device — uploading the local data as a `(sync-conflict <ts>)` duplicate file.

**Fix:** A two-part defense:
1. **Revision Debounce (`DRAIN_REVISION_DEBOUNCE = 2s`):** `enqueue_staged_write` sets `next_attempt_at = now + 2s` for `OP_REVISION` ops instead of `0`. Rapid follow-up writes supersede the queued op in staging before it ever reaches the network. Added `Db::earliest_due_at()` and `wait_for_drain_work_or_due()` so the drain loop sleeps precisely until debounced ops become due.
2. **Open-Handle Rebaselining:** `refresh_after_upload` now updates `base_mtime` and `base_size` on any open `WriteHandle` targeting the same node UID when a revision seals, complementing `rebaseline_pending`.

**Verified:** Clean build and 200/200 workspace tests passing.

**Residual (2026-07-24):** the two-part defense narrowed the window but did not close it — the baseline was still keyed on `(mtime, size)`, which drifts when the server re-stamps the *same* revision. Reproduced again on a plain application save (`test_export.xml`): base mtime `1784897777` vs sealed `1784897780`, identical size, no other device involved. The root cause is that mtime is not a revision identity. Fixed properly by **B69**.

---

## B17 — FUSE mount lock contention, missing next_attempt_at in DB enqueue, and ioctl ENOTTY fix

**Status:** Fixed (verified 2026-07-21)
**Found:** 2026-07-21, deep code audit of `fix/ioctl-handler` branch.
**Where:** `crates/pdfs-fuse/src/lib.rs`, `crates/pdfs-core/src/db/ops.rs`

**Cause:**
1. `Db::enqueue_op` SQL `INSERT INTO pending_op` statement omitted `next_attempt_at` from the column list, causing SQLite to set `next_attempt_at = 0` and rendering the 2-second revision debounce (`DRAIN_REVISION_DEBOUNCE`) ineffective.
2. `ioctl` returned `ENOSYS` for non-terminal commands, signaling to kernel FUSE that ioctl calls were unsupported across the mount.
3. `fallocate` executed `libc::fallocate` while holding `self.core.state.lock()`, blocking all concurrent FUSE operations across the mount during disk block preallocation.
4. `open`/`create` invoked `create_scratch()` prior to checking `st.active_writes`, leaking scratch files on disk during concurrent opens.

**Fix:**
1. Updated `Db::enqueue_op` in `ops.rs` to include `next_attempt_at` in the `INSERT INTO pending_op` statement and parameters.
2. Changed `ioctl` handler in `lib.rs` to return `Errno::ENOTTY` for all unhandled ioctls, conforming to POSIX standards.
3. Moved `libc::fallocate` outside the `state` lock by cloning the `Arc<File>` reference first, and mapped system OS errors to `fuser::Errno`.
4. Refactored `open` and `create` to check `st.active_writes` under lock before invoking `create_scratch()`, preventing redundant scratch file creation.

**Verified:** Workspace unit tests (200/200) and `clippy` passing cleanly.

---

## B18 — Subtree Erasure via POSIX `unlink` / `rmdir` (CRIT-01)

**Status:** Fixed (verified 2026-07-22)  
**Found:** 2026-07-21, multi-agent filesystem safety audit (`audit_bugs.md` CRIT-01)  
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `ProtonFs::unlink`, `ProtonFs::rmdir`

**Cause:** `unlink` on directories and `rmdir` on non-empty directories bypassed POSIX checks and called `trash_child` directly, silently deleting remote subtrees.

**Fix:** Added type and non-emptiness checks in `lib.rs`:
- `unlink`: Returns `Errno::EISDIR` if target is a folder.
- `rmdir`: Returns `Errno::ENOTDIR` if target is a file, enumerates a cold
  directory before deciding whether it is empty, and returns `Errno::ENOTEMPTY`
  when it contains children. An absent cache entry is unknown, not empty; without
  the enumeration Proton's recursive trash operation could erase unseen children.

**Verified:**
- Unit tests: `test_posix_unlink_and_rmdir_checks` in `lib.rs` passing cleanly.
- Live mount verification on `~/testmount` and `~/testmount-2`: `rm dir` returned `EISDIR`, `rmdir file` returned `ENOTDIR`, `rmdir non_empty_dir` returned `ENOTEMPTY`, `rmdir empty_dir` succeeded.

---

## B19 — Remote Folder Trashing on Mode Switch Failure (CRIT-02)

**Status:** Fixed; all four managed-live mode pairs verified 2026-07-22,
fault-injected mount failure still pending
**Found:** 2026-07-21, multi-agent sync engine audit (`audit_bugs.md` CRIT-02)  
**Where:** `crates/pdfs-fuse/src/devices.rs`, `apply_sync_folder_mode`

**Cause:** The first fix mounted FUSE before calling `evict_dir_contents`. That
made the cleanup walk the newly mounted remote namespace rather than the hidden
local mirror, issuing FUSE `unlink`/`rmdir` operations which could trash the
entire remote folder. The local files were not reclaimed at all.

**Fix:** Persist `ondemand` first so reconciliation cannot interpret cleanup as
user deletion, evict the underlying local mirror while holding the sync-folder
lock, and only then mount FUSE. A mount failure leaves an inert `ondemand` row;
switching back to `mirror` clears the baseline and restores the local copy.

**Verified:** `cargo clippy -p pdfs-fuse --all-targets -- -D warnings`, the
`pdfs-fuse` unit suite, and the complete managed-live matrix passed against the
1.0.0 release binary. A fault-injected mount-failure test is still needed.

---

## B20 — Un-fsynced Temp File Atomic Rename in Content Cache (CRIT-03)

**Status:** Fixed (verified 2026-07-21)  
**Found:** 2026-07-21, multi-agent storage audit (`audit_bugs.md` CRIT-03)  
**Where:** `crates/pdfs-core/src/cache.rs`, `ContentCache::store`

**Cause:** Cache store renamed temporary files into the cache directory without explicit `fsync`. Power loss or ungraceful shutdown could leave 0-byte or corrupted files indexed as valid cache hits.

**Fix:** Added `f.sync_all()?` calls on open file handles prior to executing `std::fs::rename`.

**Verified:** `cargo test -p pdfs-core` suite passing cleanly.

---

## B21 — Untracked Hole Punching in `fallocate` (HIGH-01)

**Status:** Fixed (verified 2026-07-22)  
**Found:** 2026-07-21, multi-agent FUSE audit (`audit_bugs.md` HIGH-01)  
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `ProtonFs::fallocate`

**Cause:** `FALLOC_FL_PUNCH_HOLE` zeroed scratch blocks but removed their
`WriteHandle::written` authored status. Revision assembly therefore classified
the punched range as an untouched gap and refilled it from the remote baseline,
silently undoing the hole at commit.

**Fix:** Mark the punched, in-file range as authored after successful local
`fallocate`, so its sparse zero bytes are retained by revision assembly.

**Verified:**
- Live mount verification: `libc.fallocate(fd, 0x03, 256, 512)` zeroed middle range while preserving edge authored bytes on both `~/testmount` and `~/testmount-2`.

---

## B22 — Synchronous `block_on` inside Mutex Guard (HIGH-02)

**Status:** Open / In Progress  
**Found:** 2026-07-21, multi-agent concurrency audit (`audit_bugs.md` HIGH-02)  
**Where:** `crates/pdfs-fuse/src/lib.rs`, `sync.rs`, `devices.rs`

**Cause:** Invoking `rt.block_on(...)` while holding `state.lock()` deadlocks worker threads and the main FUSE loop under high concurrency. Most major handlers have been refactored to drop `state.lock()` before invoking async runtime methods, but a full systematic audit across all background tasks and FUSE callbacks is required to eliminate all instances.

---

## B23 — Un-fsynced Scratch Wipe on Daemon Restart (HIGH-03)

**Status:** Fixed (verified 2026-07-22)  
**Found:** 2026-07-21, multi-agent recovery audit (`audit_bugs.md` HIGH-03)  
**Where:** `crates/pdfs-core/src/cache.rs:L741`, `ContentCache::rescue_scratch`

**Cause:** On daemon startup, `rescue_scratch` inspected the scratch directory and only preserved files with valid `.json` sidecars created by `fsync`. Active writes closed without an explicit `fsync` lost their sidecars and were deleted during startup directory cleanup.

**Fix:** Updated `rescue_scratch` in `cache.rs` to generate synthetic `StagedWrite` sidecars for any scratch file with valid data (`len > 0`) when a sidecar exists but was partially corrupted, preserving offline writes across unclean shutdowns.

**Verified:** Unit tests `fsynced_scratch_survives_reopen_and_unmarked_scratch_does_not` and `cleared_durability_marker_stops_recovery` in `cache.rs` passing 100%.

---

## B24 — Directory Hierarchy Loop in `rename` (HIGH-04)

**Status:** Fixed (verified 2026-07-21)  
**Found:** 2026-07-21, multi-agent FUSE audit (`audit_bugs.md` HIGH-04)  
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `ProtonFs::rename`, `crates/pdfs-fuse/src/state.rs:L237`

**Cause:** Moving a directory into one of its own subdirectories created an infinite recursive loop in inode state memory and DB tree structure.

**Fix:** Added `State::is_ancestor_of` helper in `state.rs` that walks up parent chains. `rename` checks `st.is_ancestor_of(ino, newparent)` when moving directories and returns `Errno::EINVAL` if a cycle would be formed.

**Verified:**
- Unit tests: `test_is_ancestor_of_hierarchy` in `state.rs` passing.
- Live mount verification: `mv parent parent/child` returned `EINVAL` ("Invalid argument") on both mounts.

---

## B25 — Weak Conflict Baseline Detection `(mtime, size)` (HIGH-05)

**Status:** Open  
**Found:** 2026-07-21, multi-agent sync engine audit (`audit_bugs.md` HIGH-05)  
**Where:** `crates/pdfs-core/src/cache.rs:L157`, `Baseline`

**Cause:** Conflict resolution baseline detection relies strictly on `(mtime, size)` tuple comparisons without content hashing or revision IDs. Concurrent edits landing within the same second that produce identical file sizes match baselines, causing offline edits to overwrite remote changes silently without creating `(sync-conflict <ts>)` copies.

---

## B26 — Broken Open-Unlinked File Semantics (HIGH-06)

**Status:** Fixed (verified 2026-07-22)  
**Found:** 2026-07-21, multi-agent FUSE audit (`audit_bugs.md` HIGH-06)  
**Where:** `crates/pdfs-fuse/src/lib.rs`, `crates/pdfs-fuse/src/state.rs`

**Cause:** Unlinking an open file called `trash_child`, which invalidated the node and dropped it from state immediately. Subsequent `read` calls on open file handles failed with `ENOENT` / `EIO`.

**Fix:** 
1. Added `open_count` and `unlinked` fields to `Entry` in `state.rs`.
2. `open` increments `open_count`, `release` decrements `open_count`.
3. `forget_or_unlink` marks `entry.unlinked = true` and removes `ino` from `st.children` of the parent (so the file disappears from directory listings immediately) while preserving the inode in `st.entries`.
4. `read` calculates optimistic `fsize` falling back to `core.pending` length for unlinked/pending files.
5. Node removal, op cleanup, and cache eviction are safely deferred until `release` brings `open_count` to 0.
6. `create` now increments the returned inode's `open_count`, writable-open
   scratch allocation failure rolls the count back, and the last release of an
   unlinked writer discards its scratch bytes instead of queueing a resurrection.

**Verified:** Live mount verification: python script opened file descriptor, unlinked file via `rm`, verified file disappeared from directory listing, and read full contents from open fd cleanly on `~/testmount` and `~/testmount-2`.

---

## B27 — Orphaned Write Revision Bricking Offline Sync Queue (HIGH-07)

**Status:** Fixed in code (unverified create→unlink live sequence, 2026-07-22)
**Found:** 2026-07-21, multi-agent drain engine audit (`audit_bugs.md` HIGH-07)  
**Where:** `crates/pdfs-fuse/src/lib.rs:L1811`, `crates/pdfs-fuse/src/drain.rs`

**Cause:** Unlinking a newly created offline file discards its `Create` pending op, but closing an open write handle on the file queued a `Write` revision for `local~...`.

**Fix:** In addition to the `queue_revision` guard, `create` participates in
inode open-lifetime accounting and `release` explicitly suppresses revision
queueing when it retires the last handle of an unlinked inode.

**Verified:** Code inspection in `lib.rs:1811` and workspace tests passing.

---

## B28 — Unhandled Disk Exhaustion (`ENOSPC`) in Cache & Scratch Store (MED-01)

**Status:** Fixed in code — previous remediation was unsafe (2026-07-22)
**Found:** 2026-07-21, multi-agent storage audit (`audit_bugs.md` MED-01)  
**Where:** `crates/pdfs-core/src/cache.rs`, `emergency_evict`

**Cause:** Running out of local disk space caused unhandled `EIO` errors without triggering emergency eviction of unpinned cache blobs.

**Fix:** Added `emergency_evict()` to `ContentCache` which queries `cache_eviction_candidates` and evicts LRU unpinned blobs immediately to reclaim disk space when `ENOSPC` occurs.

**Verified:** `cargo test -p pdfs-core` suite passing cleanly.

---

## B29 — Missing FUSE `forget` Method / Memory Leak (MED-02)

**Status:** Fixed (verified 2026-07-22)  
**Found:** 2026-07-21, multi-agent FUSE state audit (`audit_bugs.md` MED-02)  
**Where:** `crates/pdfs-fuse/src/lib.rs:L3983`, `crates/pdfs-fuse/src/state.rs`

**Cause:** Kernel `forget` messages sent by FUSE were ignored, causing `state.entries` and `state.by_uid` maps to grow monotonically in RAM over long daemon uptimes.

**Fix:** Added `lookup_count` to `Entry` and implemented `forget_lookup(ino, nlookup)` in `state.rs`. `ProtonFs::forget` now invokes `st.forget_lookup(ino.0, nlookup)` to prune unreferenced inodes from memory.

**Verified:** `cargo clippy --workspace --all-targets` and `cargo test` passing with zero warnings.

---

## B30 — SQLite Write Lock & Mutex Contention under Heavy Drain (MED-03)

**Status:** Open / In Progress  
**Found:** 2026-07-21, multi-agent database audit (`audit_bugs.md` MED-03)  
**Where:** `crates/pdfs-core/src/db/ops.rs`, `crates/pdfs-core/src/db/mod.rs`

**Cause:** Long-running upload drain transactions hold SQLite write locks while worker threads wait for `state.lock()`, causing periodic FUSE response latency spikes during heavy sync operations.

---

## B31 — Unimplemented Special Files & Attribute Modifications (MED-04)

**Status:** Open  
**Found:** 2026-07-21, multi-agent POSIX audit (`audit_bugs.md` MED-04)  
**Where:** `crates/pdfs-fuse/src/lib.rs`

**Cause:** Symlinks (`symlink`/`readlink`), hardlinks (`link`), FIFOs/sockets (`mknod`), and `chmod`/`chown` attribute modifications return `ENOSYS` or are silently ignored.

---

## B32 — Race Condition in Concurrent Handle Release and Open (MED-05)

**Status:** Open  
**Found:** 2026-07-21, multi-agent concurrency audit (`audit_bugs.md` MED-05)  
**Where:** `crates/pdfs-fuse/src/lib.rs:L4042, L4574`

**Cause:** Opening a file for writing while a previous handle release is draining creates overlapping scratch write handles and out-of-order remote revision uploads.

---

## B33 — Lack of Partial Transfer Resumption & Head-of-Line Queue Blocking (MED-06)

**Status:** Fixed (verified 2026-07-22)  
**Found:** 2026-07-21, multi-agent drain engine audit (`audit_bugs.md` MED-06)  
**Where:** `crates/pdfs-fuse/src/drain.rs:L73`

**Cause:** The attempted remediation classified errors by substring and deleted
the pending row after five failures or strings containing 402/403/413. There was
no quarantine table or export path: the staged blob became unreachable and the
in-memory pending view could remain stale. Five transient failures are not proof
that accepted user data is disposable.

**Fix:** Never delete an accepted write on a retry path. Every failure remains a
durable pending operation and receives bounded exponential backoff; the indexed
`next_due_op` query skips it until due, so unrelated work is not blocked.

**Verified:** `cargo clippy -p pdfs-fuse --all-targets -- -D warnings` and the
`pdfs-fuse` unit suite pass. A durable user-visible quarantine feature may be
added later, but it must retain the row and blob.

---

## B34 — Writable POSIX Exposure of Read-Only Shared Folders (MED-07)

**Status:** Fixed in code by P2/P3 (2026-07-29); P3 now exposes the shared tree, while credentialed
second-account and live FUSE acceptance remain pending P7
**Found:** 2026-07-21, multi-agent permissions audit (`audit_bugs.md` MED-07)  
**Where:** `crates/pdfs-fuse/src/lib.rs`, `crates/pdfs-fuse/src/sharing.rs`

**Cause:** Shared "Viewer" (read-only) folders are exposed via POSIX with write permissions (`0755`). Local writes succeed initially on the mount but fail perpetually during background online drain with `403 Forbidden` errors.

**Fix:** P2 persists share authority in schema V17 `share_access`, inherits it through resident
entries, exposes read-only POSIX modes, and gates mutations at handler, queue, and drain boundaries.
Downgrades fail closed and are not acknowledged past failed durable state updates.

**Verification:** The local workspace passes formatting, locked checks, clippy with warnings denied,
and all 347 workspace tests, including P3 virtual-tree materialization and access inheritance plus
mutation gating, queued-operation authorization, and downgrade failure paths. Credentialed
second-account and live FUSE acceptance have not been run and remain required in P7.

---

## B35 — IPC Unix Socket Creation Permission Race (MED-08)

**Status:** Fixed (verified 2026-07-20 - see B6)  
**Found:** 2026-07-21, multi-agent security audit (`audit_bugs.md` MED-08)  
**Where:** `crates/pdfs-fuse/src/control.rs`, `crates/pdfs-core/src/config.rs`

**Cause:** Binding the domain socket before setting `chmod(0600)` created a race window where local users could connect before permissions were enforced.

**Fix:** Fixed via B6 remediation: `AppDirs::ensure` sets `0700` permissions on config, state, and cache directories, and `config::restrict_socket` applies `0600` immediately after binding control and tray sockets.

---

## B36 — Unicode Normalization Discrepancy (NFC vs NFD) (LOW-01)

**Status:** Open  
**Found:** 2026-07-21, multi-agent sync audit (`audit_bugs.md` LOW-01)  
**Where:** `crates/pdfs-fuse/src/lib.rs`, `crates/pdfs-fuse/src/sync.rs`

**Cause:** macOS/HFS+ NFD UTF-8 path inputs differ from Linux NFC UTF-8 paths, causing duplicate folder creation or lookup misses for accented filenames.

---

## B37 — Failed Remote Trash Removed the Local Dentry (CRIT-04)

**Status:** Fixed in code (unverified on a live mount, 2026-07-22)
**Found:** 2026-07-22, deep FUSE/POSIX audit
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `ProtonFs::trash_child`

**Cause:** `trash_child` called `forget_or_unlink` before the online
`trash_nodes` request. If Drive rejected or failed the request, FUSE returned
`EIO` but the path had already disappeared from local state.

**Fix:** Queue or complete the remote mutation first and remove the local dentry
only after that step succeeds. Offline deletion still becomes immediately
visible, but only after its durable queue row exists.

---

## B38 — Sync Task Panic Could Authorize Destructive Mode Switch (HIGH-08)

**Status:** Fixed in code (2026-07-22)
**Found:** 2026-07-22, sync concurrency audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, `Core::flush_batch`

**Cause:** A `JoinSet` task panic was logged but not added to `Outcome.errors`.
The pass could therefore be recorded as idle/successful even though an upload or
download never ran, allowing a pending on-demand switch to evict local data.

**Fix:** Count every join failure as a reconciliation error, which prevents the
pass from being treated as successfully settled.

---

## B39 — Sync Download Can Overwrite a Concurrent Local Edit (HIGH-09)

**Status:** Open
**Found:** 2026-07-22, sync concurrency audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, download classification and apply path

**Cause:** Reconciliation classifies a local file from an earlier scan, downloads
to a temporary file, then renames over the destination without revalidating that
the local inode/content stayed unchanged. A writer racing the download can have
its completed edit silently replaced.

**Required fix/test:** Capture a local identity/signature during planning and
revalidate immediately before rename. Preserve a conflict copy on mismatch. A
deterministic test should pause the download, edit the target, resume it, and
verify that neither version is lost.

---

## B40 — Sync Upload Can Stream Torn Live Content (HIGH-10)

**Status:** Open
**Found:** 2026-07-22, sync concurrency audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, upload apply and baseline update

**Cause:** Planning stats a path, but upload later opens and streams the live
file. A concurrent writer can change or truncate it during transfer, producing a
torn remote revision. Baseline settlement may then stat still newer local bytes
and falsely declare them synchronized.

**Required fix/test:** Upload an immutable staged snapshot, or validate an open
descriptor before and after streaming and refuse baseline settlement on change.

---

## B41 — Sync Folder Removal Races Active Reconciliation (HIGH-11)

**Status:** Open
**Found:** 2026-07-22, sync lifecycle audit
**Where:** `crates/pdfs-fuse/src/devices.rs`, `remove_sync_folder`

**Cause:** Folder removal does not acquire the per-folder `sync_lock` used by a
reconcile pass. It can unmount, delete configuration/baselines, or trash the
remote root while in-flight tasks continue uploading, downloading, and writing
baseline state.

**Required fix/test:** Serialize removal with reconciliation, cancel/disable the
folder, await active work, then unmount and remove durable state.

---

## B42 — Local Delete Failure Is Recorded as Sync Success (HIGH-12)

**Status:** Open
**Found:** 2026-07-22, sync error-path audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, local file/directory deletion branches

**Cause:** Some local removal errors are ignored while the baseline is removed
and success is logged. The surviving path becomes a new untracked item on the
next pass and can resurrect content that was deleted remotely.

**Required fix/test:** Treat `ENOENT` as success; for every other removal error,
retain the baseline and increment `Outcome.errors`.

---

## B43 — FUSE Directory Cookies Are Not Stable (MED-09)

**Status:** Open
**Found:** 2026-07-22, FUSE/POSIX audit
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `ProtonFs::serve_readdir`

**Cause:** `readdir` rebuilds a live vector for every call and uses its array
index as the continuation cookie. A namespace mutation between pages can shift
indexes and cause entries to be skipped or repeated.

**Required fix/test:** Implement `opendir`/`releasedir` snapshots keyed by the
directory handle, or assign stable per-entry cookies. Test with deliberately
small reply pages and mutation between calls.

---

## B44 — FUSE Background Workers Have No Coordinated Shutdown (MED-10)

**Status:** Open
**Found:** 2026-07-22, daemon lifecycle audit
**Where:** `crates/pdfs-fuse/src/mount.rs`, mount lifecycle; drain/index/sync loops

**Cause:** Long-lived threads and tasks retain `Core` clones but share no
cancellation token or owned join set. Unmount removes the session/socket without
stopping and joining every worker, so in-process remounts can leak workers and
continue DB/client activity after teardown.

**Required fix/test:** Add shared cancellation, interruptible waits, owned join
handles, and ordered shutdown. Repeated mount/unmount tests should return thread
counts to baseline and observe no post-unmount DB work.

---

## B45 — Truncate After a Queued Rewrite Corrupts or Returns EIO

**Status:** Fixed and live-verified (2026-07-22)
**Found by:** `scripts/fuse-acceptance.sh`
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, write-open scratch setup;
`crates/pdfs-fuse/src/lib.rs`, `Core::queue_truncate`

**Repro:** Write a file, immediately replace its contents, then run
`truncate -s 4 file` before the queued replacement drains. Reading it returned
four NUL bytes instead of the first four replacement bytes.

The expanded managed suite found the path-based variant: create and close a
10-byte file, then immediately call `truncate(path, 4)`. The close's complete
revision was still inside the two-second drain debounce. `queue_truncate`
described the shrink as an incomplete edit over the remote base, noticed the
pending edit, preserved an orphan staging file, and returned `EIO`.

**Cause:** A new write handle always treated the server revision as its base.
When a newer complete revision was still queued locally, the handle's scratch
file began sparse and zero-filled. Shrinking it preserved those scratch zeros,
not the prefix of the locally authoritative queued revision.

**Fix:** A write opened over a complete queued revision seeds its scratch file
from that staged blob and marks the copied range authored. Stacking a write over
an incomplete queued revision is refused rather than guessed. Path-based
truncate now composes the same way: it copies a complete pending blob, applies
the new length, marks the result complete, and inherits the last real remote
baseline. Live acceptance verified shrink, zero-filled growth, and sparse I/O.

---

## B46 — Combined Cross-Directory Move and Rename Can Fail Out of Date

**Status:** Fixed and live-verified (2026-07-22)
**Found by:** `scripts/fuse-acceptance.sh`
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `ProtonFs::rename`

**Repro:** `mv source victim`, replacing an existing file, followed immediately
by `mv victim dir/moved`. Proton accepted the move half of the second operation,
then rejected its rename half with `InvalidRequirements` (HTTP 422, "out of
date"), surfaced as `EIO`.

**Cause:** Moving changes the node's encrypted-name requirements. The following
link-details read can briefly observe the pre-move state, so the new name is
signed against stale requirements.

**Fix history:** A bounded retry of `InvalidRequirements` reduced the window but
did not close it; the managed matrix reproduced the same failure after all four
retries. Combined cross-directory move+rename now enters the durable rename
queue as a desired end state. Its drain re-fetches the node on every attempt,
skips either half that already landed, renames before moving, and resolves name
collisions without losing the source. Simple rename-only and move-only calls
remain synchronous. The managed live suite subsequently passed the replacement
and combined move/rename cases.

---

## B47 — FUSE Accepts Path Components Longer Than NAME_MAX

**Status:** Fixed and managed-live verified (2026-07-22)
**Found:** 2026-07-22, managed FUSE acceptance suite
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, name-taking callbacks

**Repro:** `open(<mount>/<256 ASCII bytes>, O_CREAT)` succeeded. A conventional
Linux filesystem must reject a pathname component longer than 255 bytes with
`ENAMETOOLONG`.

**Cause:** Every callback converted `OsStr` with `to_string_lossy` and passed it
straight to Drive. This imposed neither Linux's component limit nor Drive's
UTF-8 requirement, and could silently change non-UTF-8 names.

**Fix:** Route lookup, create, mkdir, unlink, rmdir, and both rename components
through one validator. It enforces the 255-byte limit, rejects reserved/empty
components, rejects invalid UTF-8 with `EILSEQ`, and preserves valid Unicode.
The managed live suite is the verification gate.

---

## T1 — Managed Acceptance Raced Daemon Startup

**Status:** Fixed (2026-07-22)
**Found:** repeated install/restart/acceptance loop
**Where:** `scripts/fuse-acceptance.py`, managed setup

**Repro:** Run `systemctl --user restart proton-drive.service` and immediately
start `--managed-live`. The offline reference completed before the daemon had
recreated its control socket, so the first `pdfs --json sync list` failed with
`ENOENT` and setup aborted.

**Fix:** Managed validation polls the supported `sync list` API until the daemon
is ready or `PDFS_ACCEPTANCE_SYNC_TIMEOUT` expires. This is tracked as a test
harness defect rather than an application bug.

---

## T2 — Acceptance Cleanup Raced Queued Namespace Replay

**Status:** Fixed (2026-07-22)
**Found:** managed on-demand/on-demand matrix after every functional test passed
**Where:** `scripts/fuse-acceptance.py`, per-contract cleanup

**Repro:** A combined move+rename was correctly accepted into the durable queue.
Cleanup started recursively deleting its test tree while the rename drain was
still landing. The moved entry appeared in `dir-b` after `rmtree` enumerated it
but before `rmdir(dir-b)`, producing `ENOTEMPTY`.

**Fix:** Before cleanup, poll `pdfs --json status` until both pending counters
are zero. Retry `rmtree` only for `ENOTEMPTY` inside the uniquely owned test
root, then wait for the queued deletions before changing modes. Other errors are
still immediate failures.

---

## T3 — Mode-Matrix Preservation Check Raced Mirror Restoration

**Status:** Fixed (2026-07-22)
**Found:** managed on-demand/on-demand → on-demand/mirror transition
**Where:** `scripts/fuse-acceptance.py`, managed mode switching

**Repro:** Switch a populated managed folder from `ondemand` to `mirror`, wait
until `sync list` reports mirror/idle, and immediately read a preservation file.
The file can still be absent even though the subsequent restore pass downloads
it correctly.

**Cause:** The application commits the new mode before scheduling reconciliation
and leaves the previous idle state and sync timestamp in the row. Those values
describe the completed on-demand state, not a completed mirror restoration.

**Fix:** The harness records every transition to mirror and explicitly requests
and waits for a newer completed sync pass before validating local bytes. This is
tracked as a harness synchronization defect; transient absence before that pass
is expected asynchronous behavior, not data loss.

---

## T4 — Successful Managed Cleanup Left Its Own Sentinel Behind

**Status:** Fixed (2026-07-22)
**Found:** second consecutive managed acceptance run
**Where:** `scripts/fuse-acceptance.py`, managed cleanup

**Repro:** Complete the mode matrix in mirror/mirror, then immediately start a
new managed run. Registration cleanup removed the sync-folder records and remote
test folders, but the next run's empty-directory precheck found the harness's
128 KiB preservation sentinel in each local directory.

**Cause:** Removing a mirror registration intentionally preserves the local
copy. The harness treated unregistering as if it also emptied local storage.

**Fix:** After successful unregister, cleanup deletes only the sentinel whose
contents match the per-run bytes generated by this harness. Unknown files remain
untouched and continue to fail the strict precheck.

---

## B48 — Successful Unlink Can Reappear From a Stale Remote Listing

**Status:** Fixed (unverified)
**Found:** 2026-07-22, managed acceptance cleanup after all on-demand I/O passed
**Where:** `crates/pdfs-fuse/src/lib.rs`, child enumeration and trash paths

**Repro:** Unlink every child of a directory and immediately remove the
directory. `unlink` returned success, but a parent invalidation followed by an
eventually consistent remote enumeration returned the just-trashed child again.
`rmdir` then returned `ENOTEMPTY`; repeated recursive removal could reproduce it
for the entire acceptance timeout.

**Cause:** Three stale-data paths combined. A queued cross-directory rename
updated `State` but did not invalidate the kernel's cached empty destination
listing, so recursive walkers never saw or unlinked the moved child. Removing a node recorded no
authoritative local tombstone, so an eventually consistent Drive listing could
intern it again. Separately, `State::has_children` treated a resident empty
listing as inconclusive and fell back to obsolete SQLite child rows. Thus
`readdir` could report empty while `rmdir` returned `ENOTEMPTY` forever.

**Fix:** Every successful online or queued trash records its uid in a
session-shared hidden set. Both persisted and remote child enumeration filter
that set. Secondary on-demand mounts share it, and an explicit restore removes
the uid so restored content can appear normally. `has_children` now treats any
resident listing—including an empty one—as authoritative and consults SQLite
only when the listing is absent. After replying successfully to a queued rename,
the FUSE handler invalidates both exact dentries and both directory inodes; the
ordering matters because an invalidation sent before the rename reply is
overwritten by the kernel's response processing. Uids are immutable and never
reused, making a session-long tombstone safe.

---

## B49 — Failed Conflict Preservation Still Overwrites the Local File (CRIT-05)

**Status:** Fixed in code with deterministic regressions; live/fault verification pending
**Found:** 2026-07-22, 1.0 data-safety audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, download/conflict handling

**Cause:** If renaming a concurrently changed local file to a conflict copy
fails, sync only logs the error and continues publishing the remote download
over the local version. Second-resolution conflict names can also collide.

**Required fix/test:** Abort download publication unless preservation succeeds;
use collision-resistant names. Test existing names, permissions, `ENOSPC`,
`EIO`, and cross-filesystem behavior. This is B39's failure path; B25 remains
the separate weak-baseline problem.

---

## B50 — Failed Orphan Staging Deletes the Sole Scratch Copy (CRIT-06)

**Status:** Fixed in code; injected filesystem-failure verification pending
**Found:** 2026-07-22, 1.0 data-safety audit
**Where:** `crates/pdfs-fuse/src/lib.rs`, `Core::stage_orphaned_write`

**Cause:** If preserving an orphaned scratch write into staging fails, the
failure path removes the original even though no other durable copy exists.

**Required fix/test:** Never unlink the source until a durable replacement and
metadata are committed. Inject `ENOSPC`, `EIO`, `EROFS`, permission, and
cross-device failures and assert a byte-identical copy remains discoverable.
Related: B23, B27, and B28.

---

## B51 — Scratch Sidecar Does Not Establish a Durable Recovery Record (CRIT-07)

**Status:** Fixed in code; power-loss verification pending
**Found:** 2026-07-22, 1.0 crash-consistency audit
**Where:** `crates/pdfs-core/src/cache.rs`, scratch durability markers

**Cause:** A temporary sidecar is renamed without syncing the sidecar first or
the directory afterward. Power loss after successful user `fsync` can preserve
bytes but lose authored-range metadata; recovery may discard the write or treat
a sparse partial write as complete and replace untouched ranges with zeros.

**Required fix/test:** Sync data, write and sync temporary metadata, rename it,
then sync the directory before acknowledging durability. Crash-test every
boundary and corrupt/partial sidecars. B20 and B23 do not cover this invariant.

---

## B52 — Staged-Write Publication Is Not Crash-Safe (CRIT-08)

**Status:** Fixed in code; cross-filesystem and power-loss verification pending
**Found:** 2026-07-22, 1.0 crash-consistency audit
**Where:** `crates/pdfs-core/src/cache.rs`, staged-write publication

**Cause:** Rename/copy and direct sidecar writes lack a complete sync protocol.
The cross-filesystem fallback can remove its source after an unsynced copy,
leaving no complete data/metadata pair after a crash.

**Required fix/test:** Publish synced temporary data and metadata via atomic
renames and directory syncs; delete the source only after verification. Inject
failure after every write, sync, rename, DB enqueue, and deletion. Related: B50
and B51.

---

## B53 — Superseding a Pending Operation Is Not Transactional (CRIT-09)

**Status:** Fixed with rollback regression; crash/ENOSPC verification pending
**Found:** 2026-07-22, 1.0 SQLite durability audit
**Where:** `crates/pdfs-core/src/db/ops.rs`, `Db::enqueue_op`

**Cause:** The old durable row is deleted before its replacement is inserted,
outside a transaction. A crash, constraint error, full disk, or I/O failure can
erase acknowledged upload work and orphan its blob.

**Required fix/test:** Select and replace in one transaction; retain the old
blob until commit and make cleanup recoverable. Inject insert/commit failures
and prove that old or new operation—never neither—remains queued. B27 concerns
a retained failing row; this issue erases the row.

---

## B54 — Total-Wipe Guard Does Not Protect a One-Entry Baseline (CRIT-10)

**Status:** Fixed with one-entry regression; live mode-matrix verification pending
**Found:** 2026-07-22, 1.0 mirror-sync audit
**Where:** `crates/pdfs-fuse/src/sync/planner.rs`

**Cause:** The safeguard activates only when the baseline has at least two
paths. A one-file local folder that is missing, unreadable, or wrongly mounted
can therefore trash its sole remote file.

**Required fix/test:** Protect every non-empty baseline and require an
independently complete scan plus an explicit deliberate-delete signal for a
total wipe. Test absent roots, replaced mountpoints, permissions, and one-file
folders in every mode combination.

---

## B55 — Incomplete Local Scans Are Interpreted as Deletions (CRIT-11)

**Status:** Fixed in code; injected iterator/stat failure tests pending
**Found:** 2026-07-22, 1.0 mirror-sync audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, local scan/reconciliation

**Cause:** Directory-entry and metadata errors omit paths instead of marking the
snapshot incomplete. Reconciliation can treat omissions as intentional local
deletions and propagate them remotely.

**Required fix/test:** Carry completeness/errors into planning; an incomplete
subtree must make the pass non-destructive and retain its prior baseline. Test
`readdir`, `stat`, permission, transient-I/O, and disappearing-entry failures.
B42 covers the inverse local-delete failure.

---

## B56 — Database Schema Version Parsing Fails Open (HIGH-13)

**Status:** Partly fixed — future schemas are refused; malformed-version handling remains open
**Found:** 2026-07-22, 1.0 database audit
**Where:** `crates/pdfs-core/src/db/migrations.rs`, `Db::migrate`

**Cause:** Missing, malformed, or non-numeric version values become zero, while
all future versions are accepted. Corruption can replay migrations, and an
older binary can open a newer incompatible database.

**Required fix/test:** Distinguish a new DB from corrupt metadata; reject
malformed/future versions without mutation and validate required schema. Test
empty, malformed, negative, future, partial/interrupted migration, and upgrade
fixtures from every released schema.

---

## B57 — Event Cursor Can Advance Past Unapplied State (HIGH-14)

**Status:** Partly fixed — cursor now advances per successfully invalidated event; DB/apply fault tests remain
**Found:** 2026-07-22, 1.0 event-recovery audit
**Where:** `crates/pdfs-fuse/src/background.rs`, `run_event_sync`

**Cause:** Cache invalidation failures are ignored while the batch cursor still
advances. Cursor-persistence failure also leaves the in-memory cursor moving;
first-seed persistence failure can later reseed at a newer head. Applying state
and committing its cursor have no atomic relationship.

**Required fix/test:** Apply idempotently and persist derived DB state plus the
cursor atomically, or advance only through the last durable event. Retry failed
prerequisites. Crash/fault-test every event and cursor boundary, first seed,
duplicate replay, malformed events, and database-full conditions.

---

## B58 — Control Socket Has Unbounded Frames and Connections (HIGH-15)

**Status:** Fixed in code with frame-limit regressions; connection-flood verification pending
**Found:** 2026-07-22, 1.0 IPC audit
**Where:** `crates/pdfs-fuse/src/control.rs`

**Cause:** The server uses unbounded `read_line`, a new thread per connection,
and no concurrency or idle limit. A local process can exhaust memory, file
descriptors, and threads, blocking filesystem control and safe shutdown.

**Required fix/test:** Bound frames, clients, and idle lifetime. Test slow/no-
newline clients, oversized/truncated JSON, floods, and recovery. B6/B35 cover
socket authority and permissions, not resource bounds.

---

## B59 — Private State Permission Enforcement Fails Open (HIGH-16)

**Status:** Fixed in code with symlink regression; wrong-owner integration verification pending
**Found:** 2026-07-22, 1.0 local-privacy audit
**Where:** `crates/pdfs-core/src/config.rs`, `AppDirs::ensure`

**Cause:** chmod/creation failures are warned about or ignored while decrypted
content, plaintext metadata, and the authenticated socket may remain accessible
to other local users.

**Required fix/test:** Before loading credentials or serving data, verify every
sensitive directory is owned by the effective user, is not a symlink, and is
owner-only. Test wrong owner, symlink substitution, read-only filesystems,
permissive restored directories, and chmod failure. This tightens B6/B35.

---

## B60 — Config Writes Are Non-Atomic and Parse Errors Are Overwritten (HIGH-17)

**Status:** Fixed in code; fault/concurrent-save verification pending
**Found:** 2026-07-22, 1.0 configuration audit
**Where:** `crates/pdfs-core/src/config.rs`, `load_config` / `save_config`

**Cause:** Save truncates in place without sync. Load treats read/parse failure
as absence and best-effort overwrites with defaults, silently losing mountpoint,
ignore policy, budget, and explicit device adoption.

**Required fix/test:** Atomically publish a restricted, synced temporary file
and sync its directory. Preserve malformed input and report an actionable error.
Test invalid/truncated JSON, permissions, `ENOSPC`, crash points, and concurrent
saves.

---

## B61 — Rotated Single-Use Credentials Can Be Lost (HIGH-18)

**Status:** Open — 1.0 authentication/recovery blocker
**Found:** 2026-07-22, 1.0 authentication audit
**Where:** `crates/pdfs-core/src/auth.rs`, refresh callback

**Cause:** Failure to serialize or persist rotated credentials is only logged
(and some setup failures are silent). The daemon continues in memory while the
keyring retains an invalid single-use refresh token, so restart loses login.

**Required fix/test:** Expose unhealthy persistence, retry with bounded backoff,
notify the user, and define a safe re-login/shutdown path. Test locked/full/
unavailable keyrings, callback races, repeated rotations, death, and restart.

---

## B62 — Debian Artifact Omits Required Service and Autostart Units (HIGH-19)

**Status:** Fixed in workflow; clean-install VM verification pending
**Found:** 2026-07-22, 1.0 packaging audit
**Where:** `.github/workflows/release.yml`; `packaging/`

**Cause:** The generated package omits `proton-drive.service` and the tray
autostart entry even though normal login/GUI flows assume that lifecycle.

**Required fix/test:** Package all units/resources with correct paths, modes,
and hooks. In clean supported-distribution VMs test install, login, reboot,
daemon/tray/FUSE, upgrade, rollback, and uninstall without deleting user state.

---

## B63 — Release Version Identity Is Inconsistent and Unenforced (HIGH-20)

**Status:** Partly fixed — tag/workspace/PKGBUILD equality is enforced; protocol identity remains
**Found:** 2026-07-22, 1.0 release audit
**Where:** manifests, client identity constants, tags, release workflow

**Cause:** Workspace/binary versions, protocol identity strings, and tags can
disagree; the workflow trusts any `v*` tag without checking package metadata or
installed `--version` output.

**Required fix/test:** Establish one version source and fail CI on every tag,
manifest, package, and binary mismatch. Document any intentionally independent
protocol identity.

---

## B64 — Stable Publishing Lacks Data-Safety and Recovery Gates (CRIT-12)

**Status:** Partly fixed — automated gates and managed-live matrix pass; recovery approval remains open
**Found:** 2026-07-22, 1.0 release audit
**Where:** CI and `.github/workflows/release.yml`

**Cause:** A tag can publish without required format/lint/tests, offline and
dedicated-account live acceptance, migrations, or crash/recovery drills. Fixes
still marked unverified can ship as stable.

**Required fix/test:** Publish immutable RC artifacts only after all workspace
checks, full managed mode matrix, migration/power-loss recovery, clean install/
upgrade tests, and explicit sign-off for every critical/high integrity issue.

---

## B65 — Release Artifacts Lack Supply-Chain Verification (MED-11)

**Status:** Open — 1.0 release/security blocker
**Found:** 2026-07-22, 1.0 supply-chain audit
**Where:** CI and release workflows

**Cause:** Releases do not enforce advisory/license policy or publish signed
provenance, SBOMs, and verifiable checksums for exact distributed artifacts.

**Required fix/test:** Add locked audit/license gates, SBOM and provenance,
checksums and signatures, documented verification, and an independent clean-job
verification/install test.

---

## B66 — Local-Only Pending Data Is Not Protected at Shutdown (HIGH-21)

**Status:** Open — 1.0 data-safety/UX blocker
**Found:** 2026-07-22, 1.0 recovery audit
**Where:** shutdown, CLI/GUI status, logout/unregister workflows

**Cause:** Undrained writes may exist only on one machine, but there is no flush
command with a completion contract, prominent pending byte/count health, or
shutdown/logout warning/inhibition.

**Required fix/test:** Add `pdfs sync flush`, expose pending operations/bytes
and last errors, and protect destructive lifecycle actions. Test offline stop,
logout, unregister, reboot, retry exhaustion, and drain. Credential removal must
never delete staged bytes.

---

## B67 — Fresh-State Restore Can Silently Select a New Device (HIGH-22)

**Status:** Open — 1.0 recovery blocker
**Found:** 2026-07-22, 1.0 disaster-recovery audit
**Where:** device discovery/adoption; `docs/RECOVERY.md`

**Cause:** Restore depends on hostname and folder basename. After local-state
loss, a mismatch can silently create/select another device rather than attach
the intended remote roots, making recovery appear complete while data is absent.

**Required fix/test:** Require explicit adoption/confirmation on ambiguity and
show old/proposed device and root IDs. Test state loss, hostname change,
duplicates, renamed folders, multiple devices, and byte-identical restoration.

---

## B68 — Profile Backup Cannot Be Written to the Device Root

**Status:** Open — live-reproduced 2026-07-22
**Found:** complete managed-live mode matrix against the 1.0.0 release binary
**Where:** `crates/pdfs-fuse/src/profile.rs`, profile backup destination

**Repro:** Every sync-folder registration or mode transition attempted the
profile backup and received `NotEnoughPermissions` (HTTP 422): `Cannot create
file at the root of a device`. The filesystem matrix itself passed, but profile
backup never became durable remotely.

**Impact:** The recovery feature appears active but does not preserve the device
profile, so a replacement machine still lacks modes, pins, mount settings, and
the explicit remote-folder mapping. Repeated retries also add noisy warnings.

**Required fix/test:** Store the profile below a writable application folder
rather than directly at the device root, reuse that folder by stable identity,
and make backup health visible. Verify upload, replacement, restart restore, and
byte-identical fresh-state recovery on a dedicated account.

## B69 — Spurious `(sync-conflict)` copies from mtime-keyed revision identity (B16/B25 root fix)

**Status:** Fixed — offline-tested 2026-07-25, **live-validated 2026-07-26** (see below)
**Found:** 2026-07-24, user reported `test_export (sync-conflict 1784898786).xml` created after a normal save from an application into the on-demand mount. A full-mount scan turned up ~156 conflict copies; 151 were byte-identical to their live sibling.
**Where:** `crates/pdfs-fuse/src/{drain,filesystem,lib,state,sweep,sync,sync/planner}.rs`, `crates/pdfs-core/src/cache.rs`, `crates/pdfs-core/src/control.rs`; SDK `proton-drive-rs` `node.rs`/`client.rs`/`public_link.rs`.

**Cause:** Two independent false-positive sources, both from `(mtime, size)` being used as a revision identity (the weakness B25 flagged; the recurrence B16 could not fully close):
1. **Re-stamped mtime.** `revision_conflict` compared the queued write's baseline mtime against the remote's. The server stamps a sealed revision with its own commit time (`…780`), a few seconds off the optimistic time the client baselined at (`…777`). Same bytes, same size, drifted mtime → the drain diverted the write into a conflict copy of its own base.
2. **First-sync of pre-existing files.** `plan_file` classified `(local, remote, no-baseline)` as `Conflict`, so the *first* reconcile of a folder cut a conflict copy of every file that already existed identically on both sides — the ~151-file storm, all in one ~40s window.

**Fix:** Make conflict detection key on the server **revision id**, the true identity (it advances iff a new revision was sealed):
1. **SDK** exposes `active_revision_id` and `content_sha1` (plaintext SHA-1 from the revision's decrypted `XAttr`) on `NodeKind::File` — both already on the wire / already decrypted, no extra round trips.
2. **`Baseline`** gains `revision_id`, captured at write-open (`WriteHandle.base_revision_id`) and carried across supersede/rebaseline like the mtime. `revision_changed` (extracted, pure, unit-tested) trusts the id when both sides have it and only falls back to `(mtime, size)` for an old sidecar. A re-stamped mtime on the same id is no longer a conflict; a same-size edit by another device (new id) still is.
3. **Planner** adds `FilePlan::AdoptBaseline`: a no-baseline pair of equal size is recorded into the baseline instead of conflicted. Divergent sizes stay a real conflict.
4. **Auto-sweep** (`sweep.rs`, `pdfs-conflict-sweep` thread, 5-min cadence): removes conflict copies proven identical to their live sibling (equal size **and** equal `content_sha1`, trashing the copy remotely) and surfaces divergent/orphaned ones once as a new `ActivityKind::Conflict` entry. Never removes a copy it cannot prove is a duplicate.

**Verified:** `proton-sdk-rs` 50/61 lib tests + clippy clean; client `cargo fmt`/`clippy -D warnings`/`cargo test --workspace --locked` all green (258 tests, incl. new `revision_changed`, planner `AdoptBaseline`, and `conflict_base_name` cases). The 151 identical copies were removed manually during triage; the 5 divergent ones are left for the user.

**Live validation (2026-07-26):** 1.1.0 installed over the production account (6
FUSE mounts of real data), daemon restarted, sweep observed across its warmup pass
and a full 5-minute interval. Pre-run snapshot: exactly 2 `(sync-conflict)` copies
left on the account, both divergent in size *and* `content_sha1`, so the correct
behaviour was to remove nothing. Result: **nothing removed** — the conflict
manifest was byte-identical before and after, the activity feed gained zero
`auto-removed duplicate` entries, and the copies were flagged `differs from …` as
intended. (The 40 `trash` entries in the feed for that window are
`trashed from the mount` — a previous FUSE-acceptance run's queued cleanup ops
replaying through the drain, unrelated to the sweep.) The sweep's *removal* path
therefore remains unexercised against a real account; it is now report-only by
default (B71) precisely so that stays true until there is field evidence.

**Shipping note (resolved 2026-07-25):** the client's revision-id/sha1 use depends on new SDK surface (`proton-sdk`/`proton-drive-rs` **0.2.2**). That version is now **published to crates.io**; the client's workspace deps were bumped `"0.2"` → `"0.2.2"`, the `Cargo.lock` re-resolved to the registry crates, and the local `[patch.crates-io]` shim removed. No local patch remains. (Live validation of the fix against a real account is still pending.)

## B70 — Browser in-flight temp files uploaded and conflict-forked on the on-demand mount

**Status:** Layers A + B fixed in-tree 2026-07-25 — **live-validated 2026-07-26**.
Validation was blocked until B74 was fixed (its scenario ends in a rename, which
B74 made read back as empty); with that out of the way the acceptance case
`regression B70: transient download name is not sealed` passes live.
**Found:** triaging the divergent conflict copies B69 left behind. In `~/Downloads`
(an on-demand mount) the complete files had been forked into `(sync-conflict)`
copies while their truncated partials kept the canonical name — e.g.
`teamspeak.tar.gz` (30 MB partial live vs a 3.77 GB conflict copy) and `nils.zip`
(17 MB vs 3.18 GB). The conflict copies were byte-exact to the user's independent
gdrive backups, proving the *conflict copy* was the finished download and the
*live* file the stub. The smoking gun: four `Unconfirmed NNNNN (sync-conflict …).crdownload`
copies — Brave's in-flight temp files, forked *mid-download*. User confirmed the
trigger: downloading with Brave directly into the on-demand folder, where a stall
+ "resume" is routine.
**Where:** on-demand write path — `crates/pdfs-fuse/src/filesystem.rs` `release`
(`queue_revision`, ~line 780) and `rename` (~line 933); drain seal in
`crates/pdfs-fuse/src/drain.rs`. The existing ignore system
(`crates/pdfs-core/src/syncignore.rs`, `DEFAULT_IGNORE_PATTERNS`) is wired **only**
into mirror reconcile (`sync.rs`), so it never sees on-demand writes.

**Cause:** A browser downloading into the mount writes a growing temp file
(`*.crdownload`, or `*.part` for Firefox) and, on a stall, closes the fd. `release`
hands every closed write to `queue_revision`, so a *partial* revision gets sealed
as the canonical file. On resume the browser reopens and rewrites the full file;
its baseline revision has moved (the client sealed the partial itself), so the
drain diverts the finished bytes into a `(sync-conflict)` copy. Net: the stub wins
the name, the real file is exiled to a conflict copy, and every abandoned
`.crdownload` is uploaded as its own multi-hundred-MB node. Unlike B69 this is a
*genuine* two-revision divergence, so B69's revision-id detection correctly refuses
to auto-sweep it — the fix has to stop the fork from happening, not reconcile it
after.

**Fix — two layers, both without an SDK release (A stops transient names ever
sealing; B stops a single-writer resume forking against its own seal):**

- **A — transient names never seal a revision until finalized (done 2026-07-25).**
  A new `syncignore::is_transient_name` recognises browser download temps
  (`*.crdownload`/`*.part`/`*.partial`/`*.download`), generic scratch
  (`*.tmp`/`*.temp`), editor swap/backup (`*.swp`/`*.swx`/`*~`), and office/lock
  files (`.~lock.*`, `~$*`). On `create` (filesystem.rs) a transient name is kept
  **purely local** — no empty remote node is minted — and its queued create is
  **parked**: `next_attempt_at` is set to the new `PARK_UNTIL` sentinel so
  `Db::next_due_op` never selects it. Written bytes still ride on that create via
  the existing `attach_blob_to_create`, whose `next_attempt_at` reset now
  *preserves* a park (SQL `CASE … >= PARK_UNTIL`), so a growing download attaches
  revision after revision without ever waking the drain. The finalize `rename`
  (`foo.crdownload → foo`) goes through the existing `is_local_uid` branch
  (`rewrite_op_target` renames the queued create); when the old name was transient
  and the new one is not, it calls `Db::set_create_hold(uid, false)` and wakes the
  drain, so the *completed* file uploads exactly once. Net: no partial and no
  abandoned temp ever reaches Drive, and nothing forks. Reuses the whole
  create/attach/rename/drain pipeline — the only new surface is the predicate, the
  `PARK_UNTIL` sentinel, `set_create_hold`, a `hold` arg on `queue_local_node`,
  and the attach-preserves-park `CASE`. Tests: `syncignore` predicate cases +
  `db::a_parked_transient_create_stays_off_the_drain_until_finalized`. No SDK
  dependency.
  - *Not yet covered by A:* apps that write the final name in place with **no**
    temp suffix (rare), and Firefox multi-connection `.part` files that it may
    rename per-segment — those still rely on B. An abandoned transient file (a
    cancelled download left on disk) now stays local-only forever rather than
    polluting Drive; a later janitor could reclaim its staged blob.
- **B — don't self-conflict a single-writer sequential rewrite (done 2026-07-25,
  no SDK dep).** The safety net for names A does not recognise (an app writing the
  final name in place, Firefox per-segment `.part` renames, or any resume the
  in-memory rebase machinery missed). The daemon now records the server revision
  id it *itself* seals per node (`Core.own_sealed_revs`, written in
  `refresh_after_upload` where the sealed node is already fetched, dropped on
  trash). When a queued write drains and `revision_changed` fires,
  `Core::revision_conflict` checks whether the remote sits at one of *our own*
  sealed revisions (pure `is_own_self_supersede`): if so, no other device touched
  the file — it is a single-writer stall→resume — so it chains (supersedes)
  instead of forking a `(sync-conflict)` copy. Gated on `meta.complete`: an
  incomplete blob's gaps still refer to the stale base, so it keeps the
  non-destructive conflict-copy path. This is a superset guard over the existing
  `refresh_after_upload`/`rebaseline_pending` rebasers, which only fire while a
  handle/op is live; B catches the windows they miss (fresh open during the
  earlier upload's flight, a stale inherited baseline). Tests: `is_own_self_supersede`
  cases in `drain.rs`. *Residual:* the map is in-memory only, so a daemon restart
  between the partial seal and the resume drops the record and that one write can
  still fork — narrow, and B69's revision-id keying already covers the mtime-drift
  half. A persisted own-seal ledger would close it.

**Triage of the copies already made is done (2026-07-25):** all completed files
were promoted back to their canonical names (metadata-only rename, no re-upload)
and the abandoned `.crdownload` stubs trashed.

## B71 — Conflict sweep is not production-ready (auto-trash loop, CRIT-13)

**Status:** Blockers 1–4 fixed in-tree 2026-07-26; items 5–10 and the remaining
cleanup still open. The sweep is safe to leave running (it is report-only by
default and cannot trash anything until explicitly switched to `enforce`).
**Where:** `crates/pdfs-fuse/src/sweep.rs`, spawn site `crates/pdfs-fuse/src/mount.rs`,
config surface `crates/pdfs-core/src/config.rs`.

The B69 fix (see above) shipped a `pdfs-conflict-sweep` thread that **trashes user
files on the real Drive** from a background loop. The identity check itself is
conservative (equal size *and* equal `content_sha1`, missing digest treated as
divergent), but the machinery around it is not yet safe to run unattended. Review
found the following, ordered by severity. Each is a separate defect; they are
grouped because they gate the same feature.

**Blockers (all fixed 2026-07-26 — the sweep may not trash anything without them):**

1. **Data-loss race — trash-then-discard (CRIT).** `remove_conflict_copy`
   (`sweep.rs:137-155`) trashes the node remotely and *then* calls
   `discard_queued_ops(uid)`, which deletes the node's queued ops **and discards
   their staged blobs** (`lib.rs:1544-1553`). The decision was made from the
   `load_all()` snapshot taken at `sweep.rs:77`, with network round trips in
   between. A user edit landing in that window is destroyed — the queued write is
   dropped and its staged bytes, which may be the only copy, are discarded. This
   violates the invariant that `staging/` is never purged. *Fix:* re-read the node
   from the DB and re-verify `active_revision_id` immediately before trashing;
   skip the node outright if `Core.pending` or the op table has any entry for it;
   never let the sweep discard staged bytes.
2. **No kill-switch (CRIT).** `mount.rs:234-239` spawns the loop unconditionally.
   There is no `AppConfig` field and no environment override, so the only way to
   stop a destructive background loop is to uninstall. *Fix:* add
   `AppConfig.conflict_sweep: Option<bool>` (same `Option` idiom as `cache_budget`
   / `ignore_patterns`), a `PDFS_CONFLICT_SWEEP=off` override for support, and a
   Settings toggle.
3. **Open handles ignored (HIGH).** Nothing checks for a live file handle before
   `forget_or_unlink` + `evict_reader`, so the sweep can trash a node a reader
   currently has open. *Fix:* skip nodes with open handles.
4. **Enforcing on day one (HIGH, process).** The sweep exists because B69 proved
   the previous revision-identity check was wrong; betting deletion on the
   *replacement* check in the same release, with no field evidence, is the wrong
   order. *Fix:* ship report-only (log `would-trash` plus the existing
   `ActivityKind::Conflict`), collect a release cycle of real logs, then flip to
   enforcing.

**Fix for 1–4 (landed 2026-07-26):**

- **`SweepMode`** (`pdfs-core/src/config.rs`) — `Off` / `Report` / `Enforce`,
  serialised into `AppConfig.conflict_sweep` (`#[serde(default)]`, so configs
  predating the field still load). `Default` is **`Report`**, which is blocker 4:
  an existing install that has never heard of the setting cannot get the
  enforcing behaviour on upgrade. `AppConfig::resolved_conflict_sweep` layers
  `PDFS_CONFLICT_SWEEP` over the stored value; the precedence is a pure
  `resolve_sweep_mode(env, stored)` so it is testable without mutating process
  state. An unparseable value falls through to the config rather than guessing a
  mode in either direction.
- **Spawn gate** (`mount.rs`) — `SweepMode::Off` skips the thread entirely rather
  than starting an idle one, so the setting is verifiable from outside the
  process (`ls /proc/<pid>/task/*/comm`). Both branches log the resolved mode at
  startup. `mount` grew past clippy's argument limit, so `username` +
  `sweep_mode` moved into a `MountOptions` struct resolved by the caller —
  `mount` still reads neither config nor environment.
- **Interlocks** (`sweep.rs::duplicate_still_removable`) — re-checks, immediately
  before the destructive call, everything the pass decided from its stale
  snapshot: no queued op (new `Db::has_any_op`, a cheap `COUNT` — a queued op
  means bytes are owed an upload and `discard_queued_ops` would throw away the
  staged blob holding them), no open write handle or shared scratch file
  (`Core::is_busy`, checking `active_writes` + `handles` under one `State` lock),
  and the node still at the same revision id, size, digest, name, parent and
  untrashed state. Any doubt — including a failed DB read — means leave it alone:
  a copy that survives to the next pass costs nothing, a wrongly removed one
  costs data. This is blockers 1 and 3.
- **Testability** — the decision moved into a pure
  `decide(node, sibling, base, mode) -> SweepAction`, the same extraction used
  for `revision_changed` and `is_own_self_supersede`; everything that can fail or
  race stays in the caller. Tests now cover removal-when-enforcing,
  report-mode-never-removes, size-differs, digest-differs, missing-digest in both
  directions, and orphaned copies. Gate: `cargo fmt` / `clippy -D warnings` clean,
  `cargo test --workspace` 282 passed (was 270).

**Should fix:**

5. **`load_all()` every pass (MED).** Each 300 s pass pulls the entire node table
   into a `Vec<StoredNode>` and builds a full `(parent, name)` `HashMap`. *Fix:*
   query only conflict-named rows (`name LIKE '% (sync-conflict %'`) and look up
   siblings per parent — O(conflicts), not O(drive).
6. **No batching or per-pass cap (MED).** The B69 incident produced 151 copies;
   that is 151 sequential `trash_nodes` calls in one pass with no cap and no
   backoff. `trash_nodes` takes a slice — batch it and cap per pass.
7. **`online` sampled once per pass (MED).** Read at `sweep.rs:100`, ahead of a
   loop that does network I/O; going offline mid-pass yields a long run of failing
   calls. *Fix:* bail on the first transport error.
8. **Ambiguous sibling index (MED).** `by_parent_name.insert` (`sweep.rs:97`) is
   last-write-wins, so with duplicate `(parent, name)` rows the surviving entry
   depends on `load_all` row order. *Fix:* make the choice deterministic or skip
   ambiguous parents.
9. **Trashed ancestors (LOW).** A copy under a trashed folder finds no live
   sibling and is flagged "orphaned", spamming the activity feed. *Fix:* skip
   nodes with a trashed ancestor.
10. **SHA-1 provenance unverified (MED).** `is_identical` (`sweep.rs:181`)
    compares digests without asserting they belong to the current
    `active_revision_id`. A stale `XAttr` digest from an earlier revision plus a
    size collision would delete real data. (The *missing* digest case is already
    handled correctly.)

**Also required to close:**

- **Tests.** The *policy* is now covered via the pure `decide` (see above). Still
  untested are the *interlocks*, which need a `Core`: pending-op→skip,
  open-handle→skip, revision-moved→skip, offline→skip. Those want a test harness
  that can build a `Core` against a temp DB and a stub client.
- **`conflict_notified` is unbounded.** `HashSet<NodeUid>` that only grows except
  on trash (`sweep.rs:157,170`).
- **Docs.** `docs/ARCHITECTURE.md` thread map does not list `pdfs-conflict-sweep`;
  `docs/RECOVERY.md` needs a "the sweep trashed a file, recover it from Proton
  trash" path.

**Mitigating context from the 2026-07-26 pre-run snapshot:** the account had only
2 conflict copies left, both divergent in size *and* `content_sha1`, so the
correct behaviour for the first live pass is to trash nothing.

## B72 — `pdfs trash` never returns

**Status:** Hardened in-tree 2026-07-26 — the hang is gone by construction; the
underlying slowness of the first refresh is still unmeasured (live-pending).
**Found:** capturing a trash baseline before the B69/B70 live validation. `pdfs trash`
produced no output and was still running when killed at 120 s. Daemon was healthy
and every other CLI call (`pdfs sync list`, `pdfs --version`) returned promptly.
**Where:** `Core::list_trash` (`crates/pdfs-fuse/src/lib.rs`). Not the CLI: the
client's 120 s `READ_TIMEOUT` (`pdfs-core/src/control.rs`) is what ends the call,
and the daemon never writes a response line.

**Root cause.** The trash of this account had never been fetched successfully —
`trash` table empty, no `trash_synced_ms` in `sync_state`. `list_trash` therefore
always took its "never fetched → `rt.block_on(refresh_trash())`" branch, which:

- materialized the *entire* trash in one `enumerate_nodes` call (one S2K unlock
  per node) before persisting anything, so nothing was ever saved and no later
  request could be served from the DB;
- had no single-flight guard on that branch (only `spawn_trash_refresh` did), so
  every attempt started another full refresh;
- was never cancelled when the requester timed out, because the control handler
  sits in `block_on` on its own thread and nothing signals it.

So each look at the trash added a permanent, full-cost refresh to the daemon.
Measured on the live daemon after a handful of GUI/CLI attempts: **11 stale
`pdfs-control` threads**, RSS 2.9 GB. Concurrent with a drain backlog that was
uploading conflict copies continuously (1991 `Conflict` activity rows, multi-GB
uploads), none of the refreshes ever finished.

**Fix (in-tree).** `list_trash` never blocks on the whole refresh: it kicks the
single-flighted background refresh, waits at most `TRASH_FIRST_WAIT` (20 s, well
under the front-end read timeout) on a `Notify`, and answers with whatever has
materialized. `refresh_trash` now materializes in `TRASH_MATERIALIZE_CHUNK` (150)
batches, persisting the cumulative listing after each one, and logs the uid count
plus per-batch and total elapsed at INFO/DEBUG — previously the whole path was
silent, which is why the stall could not be located from the journal.

**Still open:** *why* one refresh is slow enough to matter. The new INFO lines
(`trash refresh: enumerated` / `batch` / `done`) answer that on the next run:
whether `enumerate_trash_node_uids` returns at all, how many nodes the trash
holds, and the per-batch cost. If the count is large, materializing with
`enumerate_nodes_light` (skips the per-file node-key S2K; `size` would fall back
to on-storage size) is the next lever.

**Impact:** the trash listing is the user's recovery path after any destructive
operation, including the B71 sweep. It needs to work before the sweep is allowed
to trash anything.

## B73 — Unimplemented FUSE handlers log a WARN per probe

**Status:** Fixed in-tree 2026-07-26 — live-validated. Found 2026-07-26 by the
expanded acceptance suite.
**Found:** `scripts/fuse-acceptance.sh --live` step "unsupported operations refuse
cleanly". Each probe produced a multi-line `fuser` warning in the daemon journal:

```
WARN fuser: [Not Implemented] symlink(parent: INodeNo(0x2d), link_name: "symlink", target: "link-target")
WARN fuser: [Not Implemented] link(ino: INodeNo(0x94), newparent: INodeNo(0x2d), newname: "hardlink")
WARN fuser: [Not Implemented] mknod(parent: INodeNo(0x2d), name: "fifo", mode: 4516, umask: 0x12, rdev: 0)
WARN fuser: [Not Implemented] setxattr(ino: INodeNo(0x91), name: "user.pdfs.probe", flags: 0x0, position: 0)
WARN fuser: [Not Implemented] lseek(ino: INodeNo(0x93), fh: 0, offset: 0, whence: 3)
```

**Cause:** `ProtonFs` does not implement these callbacks, so `fuser`'s default
trait method answers `ENOSYS` *and logs at WARN*. The refusal is correct — the
acceptance suite accepts every one of them as a clean refusal — but the logging
is not conditional on anything the user did wrong.
**Impact:** cosmetic, but these are not exotic calls: `git` probes symlinks,
`cp -a` probes hard links, `tar` and `cp --sparse` probe `SEEK_HOLE`/`SEEK_DATA`
(`whence: 3` above is `SEEK_DATA`), and any xattr-aware tool probes `setxattr`.
Warnings that look like faults bury real ones and make both `--journal-check`
and manual triage noisier.
**Measured** across two full acceptance runs against one daemon lifetime (PID
887441, mount never remounted):

| op | warnings | behaviour |
|---|---|---|
| `link` | 11 | repeats on every call |
| `symlink` | 4 | repeats on every call |
| `mknod` | 2 | repeats on every call |
| `setxattr` | 1 | kernel cached the `ENOSYS`; never asked again |
| `lseek` | 1 | cached |
| `copy_file_range` | 1 | cached |
| `fsyncdir` | 1 | cached |

So the FUSE kernel module remembers `ENOSYS` for the file-level operations and
stops issuing them for the lifetime of the mount, but not for the namespace
operations, which are re-issued forever.

`link` leads for a sharper reason than "tools probe it": git uses `link()` to
place **every loose object**, falling back to `rename()` when it fails. In one
workloads run, 9 of 10 `link` warnings carried a 38-hex-character name — the
`.git/objects/ab/<38 hex>` layout — from just 3 commits over 3 files. The volume
is therefore proportional to work done, not a fixed startup cost: a `git clone`
of a real repository emits one multi-line warning per object. The lone `symlink`
that is not the acceptance probe is git's `core.symlinks` detection at
`git init` (a random name pointing at `testing`).

That moves this from cosmetic to a genuine operational problem: ordinary use of
git inside a mount can bury every other log line.

**Fix (applied):** all seven callbacks are now implemented explicitly in
`crates/pdfs-fuse/src/filesystem.rs`, each answering with **the same errno
`fuser`'s default would** and nothing else — no WARN. Following `ioctl`
(which already used this pattern and says why in its comment), the errno
contract is unchanged, so the acceptance suite's clean-refusal assertions hold
byte for byte:

| op | errno | why that one |
|---|---|---|
| `symlink`, `link` | `EPERM` | meaningful operation, unrepresentable on Drive; git falls back to `rename()` |
| `mknod` | `ENOSYS` | no devices/FIFOs/sockets; regular files arrive via `create` |
| `setxattr` | `ENOSYS` | surfaces as `EOPNOTSUPP`; reads stay implemented for the thumbnail interface |
| `fsyncdir` | `ENOSYS` | kernel absorbs it and reports success; dir changes are already durable in SQLite |
| `lseek`, `copy_file_range` | `ENOSYS` | **must stay** `ENOSYS` — it is what makes the kernel emulate `SEEK_HOLE`/`SEEK_DATA` and coreutils fall back to read/write instead of failing |

Only the three namespace ops were strictly required (the other four silence
themselves once the kernel caches `ENOSYS`), but implementing all seven keeps the
refusal explicit and documented at the call site rather than inherited.

**Verified:** full live acceptance run after the fix — 19 pass / 1 skip, the
capability diff identical to the pre-fix run, and **0** `Not Implemented` lines
in the journal across the whole run (was 17, 10 of them `link`).
**Regression cover:** the acceptance suite's clean-refusal probes assert the
errno contract; they pass unchanged.

## B74 — A drained create never reaches a sync-folder mount's inode space, so the file reads as empty

**Status:** Fixed in-tree 2026-07-26 — live-validated. Found 2026-07-26 by the
expanded acceptance suite (`regression B70` case), then isolated to a broader
trigger.
**Impact:** on a *sync folder* mount (not `~/ProtonDrive`), a file whose create
was still queued reads back as **0 bytes for as long as the daemon runs**, even
though the bytes are on Drive intact. This is the write-to-temp-then-rename
pattern used by every browser download, most editor saves, `rsync`, and `git`.

**Not data loss.** An earlier revision of this entry called it permanent data
loss; that was wrong and is corrected here. The uploaded revision is complete and
correct: for every "lost" file the node row carried the right `size` and
`active_revision_state: "Active"`, and a daemon restart made all of them read
back with the expected sha256. Nothing had to be recovered. The defect was
entirely local and entirely in the read path.

**Reproduction** (on an on-demand mount, per file: write, close, rename, wait for
`pdfs status` to report the queue drained, then read back):

| scenario | result |
|---|---|
| plain name, never renamed | content intact |
| plain name, renamed after close | **0 bytes until restart** |
| `.crdownload`, renamed after close | **0 bytes until restart** |
| `.crdownload`, never renamed | content intact |
| already-settled file, renamed | content intact |
| any of the above, after a daemon restart | content intact |

The gap between `close()` and `rename()` does not matter — 0 s, 2 s and 20 s all
behave the same. The boundary is not timing but state: the affected node is one
whose **create op was still queued**, on a mount that is not the primary one.

**Root cause — one drain thread, many inode spaces.** The daemon mounts
`~/ProtonDrive` plus one FUSE session per `ondemand` sync folder. Each of those
is a `Core::fork_state` clone with its **own** `State` — its own `entries`,
`by_uid`, `children`, `next_ino` — while sharing `db`, `cache`, `pending` and
`client`. There is exactly **one** `pdfs-drain` thread in the process, owned by
the primary mount, and it serves the whole shared `pending_op` table.

So when a create queued by a sync-folder mount lands, the drain calls
`adopt_real_uid` — which looked only at `self.state`, the *primary* mount's inode
space. The fork's entry keeps its `local~…` placeholder uid forever. And
`Core::read_range` (`crates/pdfs-fuse/src/reads.rs:314`) short-circuits a local
uid to an empty vec:

```rust
if is_local_uid(uid) { return Ok(Vec::new()); }
```

which is why the read is silent, instant, and zero-length. A restart rebuilds
every state from the DB, where `remap_local_uid` had already written the real
uid, so the file reads correctly from then on.

Confirmed live: `read handler uid=local~1785067300752-0 fsize=140000` three
seconds after `pending create landed`, and at the same moment
`adopt_real_uid … hit=None strays=[]` — the primary state had no entry for the
uid at all. `/proc/<pid>/task/*/comm` showed 1 `pdfs-drain` against 8 fuser
sessions.

**Fix:** `Core` gained a `states: Arc<StateRegistry>` — a `Vec` of weakly-held
mounts that every mount publishes itself into (`Core::register_state`, called
from `mount.rs` for the primary and from `fork_state` for each fork), plus
`Core::for_each_state`, which walks every live inode space one lock at a time.
A uid is unique across mounts, so at most one state matches and the rest are
no-ops. Five drain sites were converted: `adopt_real_uid`, `adopt_drained_name`,
the conflict-copy `invalidate_listing`, the `active_writes` baseline rebase in
`refresh_after_upload`, and that function's closing `intern`.

**It was completely silent.** No `ERROR`, no `WARN`, no failed op, no conflict
copy, no retry — `pdfs status` reported the queue drained, because it had. Any
diagnosis that relies on the journal or on queue state will not see this.

**Regression cover:** `scripts/fuse-acceptance.py`, case
`regression B74: rename after close keeps its content` — the narrowest form
(plain names, no overwrite, no transient suffix). It failed in two consecutive
live runs before the fix and passes after it. The pre-existing B70 case failed on
the same cause and now passes too.

**Audit of the rest of the daemon.** Every background-thread `state.lock()` was
reviewed for the same shape. The rule that came out of it: **a uid is unique
across mounts, so uid-keyed work must be broadcast; an inode number is only
meaningful to the mount that minted it, so inode-keyed work must not be.** What
changed:

| file | site | why |
|---|---|---|
| `background.rs` | `apply_event` | the read-side twin of this bug — remote deletes, trashes and renames only ever reached the primary mount's tree |
| `sweep.rs` | conflict-copy `forget_or_unlink` | a conflict copy inside a sync folder lives in that fork's inode space |
| `sweep.rs` | `is_busy` | answered "idle" for every file open on a fork, letting the sweep delete one with a writer attached |
| `sharing.rs` | `rel_path_for_uid` | a share of a node in a sync folder could not be named |
| `drain.rs` | `node_place` | fell through to a DB lookup for fork-resident nodes |
| `lib.rs` | `queue_trash`, `rename`, `remove_replaced` (×2), `drop_local`, `restore` | uid-keyed `forget`/invalidate |

Deliberately left on `self.state`: `upload.rs` (`parent_ino`/`pino` are inode
numbers), the size-upgrade path in `lib.rs`, and `drain.rs`'s `root_uid`, which
is correctly primary-only. The 38 `filesystem.rs` sites are FUSE callbacks —
they already run on the right mount's session thread. `parent_is_gone` never
touches state at all; an earlier note here saying it did was wrong.

The registry is now a `StateRegistry` type with a unit test
(`state_registry_walks_every_live_mount_and_reaps_the_rest`) covering the case
this bug was: a fork that fails to appear in the walk, or one that lingers after
unmount.

**One regression came out of the audit, and is fixed.** Broadcasting
`apply_event`'s parent-listing invalidation meant forks started receiving
invalidations they had never received — and it invalidated a folder once per
event, while a burst of our own uploads produces one event per file all naming
the *same* folder. Every following lookup in that folder then re-enumerated it
from the API: quadratic in the folder's size.

Two mitigations, both in `background.rs`. A batch is collapsed to one
invalidation per folder — `apply_event` records the parent in `DirtyParents` and
`flush_dirty_parents` publishes it once after the batch loop (every `break` path
falls through to it). And a change the daemon made *itself* is recognised as its
own echo rather than treated as foreign: the drain records it
(`Core::note_self_change`, consumed once within `SELF_CHANGE_TTL_MS`), which
stops the feed's report of our own upload from evicting the content blob we just
sent. Neither weakens the result — the state after a batch is what it always was.

**The hang that showed up alongside this was a different bug, and this entry
originally misattributed it.** `readdir stability` at 133.94 s and a concurrency
case that blew past its timeout were blamed here on the invalidation storm. They
were not: the mount was freezing because network work runs on fuser's single
dispatch thread, which is B75. Fixing that took the concurrency case from two
timeouts in three runs to 16.9 / 24.1 / 29.1 s and `readdir stability` to 62.9 s.
The coalescing above is still worth having — it is a real amplification — but it
was never the stall.

**Note on B70:** B70's own fix was never implicated. The `.crdownload` park/un-park
behaves correctly in isolation; the empty read arrived with the rename, on the
same path a plain temp file takes. B70's live validation was blocked on this and
is now unblocked — its acceptance case passes.

## B75 — Network work on fuser's single dispatch thread freezes the whole mount

**Status:** Fixed in-tree 2026-07-26 — live-validated. Found 2026-07-26 while
chasing what was thought to be a B74 regression.
**Impact:** any burst of concurrent writes froze the entire mount — not the
files being written, the *mount*. `ls` on an unrelated directory of it timed out
for minutes. The daemon itself stayed healthy throughout: the primary mount and
the control socket answered normally while a sync-folder mount was unreachable.

**Root cause.** `mount.rs` and `devices.rs` both build their session from
`Config::default()`, which leaves `n_threads` unset, and fuser 0.17 defaults it
to 1 (`session.rs:254`). One dispatch thread per mount reads a request, runs the
handler to completion, and only then reads the next. Six handlers were handed to
the [`Workers`] pool — `lookup`, `getattr`, `readdir`, `read`, `getxattr` and the
slow `serve_lookup` — and everything else ran inline, including handlers that
block on the API:

| handler | blocking work |
|---|---|
| `create` | `upload_file` to mint the node, then `fetch_node` — two round trips |
| `mkdir` | `create_folder`, then `fetch_node` |
| `rename` | `rename_node`, then `move_node`, plus a trash on the replaced-destination path |
| `unlink` / `rmdir` | `trash_nodes`; `rmdir` enumerates the folder first |
| `setattr` (path truncate) | `queue_truncate` → `fill_gaps`, which reads the kept prefix from the remote |

So eight applications each creating a file served one create at a time, and
every `read`, `open` and `lookup` on that mount queued behind them.

**Measured**, acceptance suite's `independent and shared-file concurrency` (8
threads × 16 files), and a 3-second `ls` of the mount root sampled every 4 s
throughout:

| | before | after |
|---|---|---|
| concurrency case | 200 s watchdog ×2, 37 s ×1 | 16.9 / 24.1 / 29.1 s, all pass |
| `ls` of the mount during the run | 33 of 55 probes timed out | 0 of 85 |
| `readdir stability` | 148.8 s | 62.9 s |
| `namespace operations` | 31.3 s | 15.7 s |

**Fix:** each of those handlers now parses its arguments inline — cheap, and it
keeps the error paths prompt — and hands the body to the worker pool as a
`serve_*` method, the pattern `lookup`/`readdir` already used. Metadata work
takes `Lane::Meta`; the truncate path takes `Lane::Transfer` because it can pull
a whole file.

**`release` was deliberately left inline, and this is load-bearing.** Moving it
too made all three concurrency runs fail with `concurrent file 1 mismatch`. The
kernel does not wait for a `release` reply before letting `close(2)` return, so
the only thing sequencing the staging of the written bytes against the `open` +
`read` an application issues immediately afterwards is that both are served by
the same dispatch loop. From a worker, the read overtakes the staging and is
answered from the remote's older revision. Taking `release` off the loop — worth
doing, since `queue_revision` gap-fills a partial write from the network — needs
a per-node "staging in flight" barrier that reads wait on, not the dispatch
loop's accidental ordering.

**Not fixed here.** `n_threads` is still 1; with the blocking handlers moved off,
the loop is short enough that raising it is a separate question. Related findings
from the same audit, still open: cache/reader validity is keyed on
`(mtime, size)` while the revision identity is `active_revision_id`
(`cache.rs:84` — two same-size revisions within one mtime second collide, which
is what a SQLite page rewrite looks like); `local_finish_scan` holds the daemon's
one SQLite connection across a full FTS5 trigram rebuild (`db/local.rs:79`);
`earliest_due_at` has no `<= now` filter where `next_due_op` does, so the drain
busy-spins when offline with a due op (`db/ops.rs:385` vs `:421`); and
`drain_local_node` deletes its op before the fallible `adopt_real_uid`
(`drain.rs:346`).
