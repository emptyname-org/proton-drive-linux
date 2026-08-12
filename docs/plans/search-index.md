# Search index + prompt display fixes

Working plan. Findings come from an audit of a real `cache.db`
(9,252 Drive nodes, 110,187 local files) on 2026-08-12.

## Evidence

```
nodes                     9252
nodes_fts                 6547     ← 2705 rows never indexed
nodes in orphan subtrees  2705     ← exactly the missing set
local_files              110187
  /home/narl/go/pkg/mod   63399    (58% of the local index)
query "test"             46405 local matches, candidate pool capped at 1000
  top 500 by fts rank      367 (73%) from go/pkg/mod
```

`pdfs search tickets` returns only `Documents/Tickets` (My Files); the
`tickets.pdf` sitting in the device folder `Downloads` is invisible.

## P1 — 29% of Drive is unsearchable

`node_is_indexable_tx` (`crates/pdfs-core/src/db/nodes.rs:606`) indexes a node
only when the ancestor walk terminates at a row with `parent_uid IS NULL`. The
DB holds exactly one such row (the My Files root). Device roots live in
`device`/`sync_folder`, never in `nodes`, so every device-folder subtree
(`Downloads`, `Pictures`, `Documents`, `Music`, `Videos` under device
*nthinkpad*) walks up to a `parent_uid` with no row and is judged unindexable.
Stale `pdfs-acceptance-*` roots hit the same path.

- [x] Relax the rule to "the walk terminates and no ancestor is trashed",
      keeping the existing cycle guard. A subtree whose top row's parent is
      simply not cached is still real, reachable data.
- [x] `MIGRATION_V21`: rebuild `nodes_fts` under the new rule, so existing
      installs recover the missing rows instead of waiting for each node to be
      upserted again.
- [x] Tests: a node under an uncached parent is indexed; a trashed ancestor
      still excludes; the migration backfills.

### Found while testing: a parent cycle hangs the daemon

Writing the cycle test surfaced a live defect, not a test artefact. A query
below the trigram minimum takes the `LIKE` lane, which reads `nodes` directly
and never consults the index, and every row it returns goes through `path_of` —
whose parent walk is an unguarded `UNION ALL`. On a `parent_uid` cycle it never
terminates, while holding the daemon's only SQLite connection.

- [x] Depth-cap `path_of` (`db/utils.rs`), matching the 1024 cap
      `path_relative_to` already carried.
- [x] Depth-cap the `descendants` CTE in `upsert_node_tx`, which has the same
      exposure on the write side.
- [x] Keep cycles out of the index (part of the P1 rule above).

Still unguarded, not touched here: `pin_is_pinned`'s ancestor walk.

## P2 — local index noise crowds real files out of the candidate pool

`SKIP_DIRS` (`crates/pdfs-core/src/localindex.rs:23`) misses the Go module
cache and build output dirs. `search_v2` clamps candidates to ≤1000 and scores
*after* SQLite, so for any common term the pool is exhausted by vendored files
before a real one is considered.

- [x] Skip `go/pkg/mod` (path-relative, not a bare name), `dist`, `build`,
      `.cache`-style artefact dirs.
- [x] Give `search_local_candidates` a name-prefix lane, mirroring what
      `search_candidates` already does for Drive, so an exact-prefix match
      cannot be starved by trigram rank.
- [x] Tests for both.

## P3 — display lies about where a hit lives

- [x] `Hit::location()` (`crates/pdfs-gui/src/prompt.rs:150`) hardcodes
      `My files / …` for every Drive hit. Derive the label from
      `mounted_path` when the daemon resolved one (device folder, sync folder)
      and fall back to `My files` only for the primary mount.
- [x] Pin rows set `is_dir: pin.recursive` (`dmenu.rs:133`, `prompt.rs:872`) —
      a non-recursively pinned folder renders and activates as a file.
- [ ] `pdfs locations` prints `Device folder 1..5` rather than the device and
      folder name. (Cosmetic; deferred.)

## Verification

Done, 2026-08-12:

```
cargo fmt --all -- --check                                  clean
cargo clippy --workspace --all-targets -- -D warnings       clean
cargo test --workspace --locked                             439 passed, 0 failed
```

The V21 backfill was also replayed against a read-only `.backup` of the live
`cache.db`:

```
live_nodes         9276
indexed            9276     (was 6547)
downloads_subtree   300
tickets.pdf ->     Downloads/tickets.pdf
```

That run is what caught the last defect: the backfill originally seeded an
uncached-parent root with an empty path, so a node indexed by the migration
(`tickets.pdf`) and the same node after a later upsert (`Downloads/tickets.pdf`)
disagreed. The seed now keeps the name for any row that has a parent, matching
`path_of`.

Still to do, on the live system: rebuild, restart `proton-drive.service`, and
confirm `pdfs search tickets` returns the device folder's `tickets.pdf`. The
schema move to 21 is one-way — an older `pdfs` refuses a v21 database. The
`local_files` shrink (~110k → ~47k) lands with the next background scan.
