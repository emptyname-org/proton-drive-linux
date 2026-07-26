#!/usr/bin/env bash
# Filesystem syscall contract, with optional tests against mounted Drive views.
set -Eeuo pipefail

usage() {
    cat >&2 <<'EOF'
usage: scripts/fuse-acceptance.sh [--offline-only] [OPTION ...]
       scripts/fuse-acceptance.sh --live MOUNTPOINT [MOUNTPOINT ...] [OPTION ...]
       scripts/fuse-acceptance.sh --managed-live EMPTY_DIR EMPTY_DIR [OPTION ...]
       scripts/fuse-acceptance.sh MOUNTPOINT [MOUNTPOINT ...]  # compatibility

The local reference suite always runs first and requires no account; the mount
is then diffed against its recorded behaviour. --live runs the same contract on
each mount. Set PDFS_ACCEPTANCE_CONVERGENCE=1 only when all live mountpoints
show the same remote folder.

Options are forwarded to the Python runner. The useful ones:
  --list                  print every case and the targets it runs against
  --timeout SECONDS       per-case limit (default 180); a hung case dumps stacks
  --fail-fast             stop at the first failure instead of continuing
  --report-json PATH      machine-readable results and recorded observations
  --report-junit PATH     JUnit XML for CI
  --budget SECONDS        report any case slower than this
  --journal-check         fail if the daemon logs errors during the run
  --durability            restart the daemon mid-suite and re-verify bytes
EOF
    exit 2
}

command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
runner=(python3 -u "$script_dir/fuse-acceptance.py")

mountpoints=()
passthrough=()
# Paths come first and options after them, so the first dash-prefixed argument
# ends the path list and everything from there is forwarded untouched.
collect_paths() {
    while (( $# > 0 )) && [[ "$1" != -* ]]; do
        mountpoints+=("$1")
        shift
    done
    passthrough=("$@")
}

case "${1:-}" in
    -h|--help) usage ;;
    --offline-only|"")
        shift || true
        exec "${runner[@]}" "$@"
        ;;
    --live)
        shift
        collect_paths "$@"
        (( ${#mountpoints[@]} > 0 )) || usage
        ;;
    --managed-live)
        shift
        (( $# >= 2 )) || usage
        first="$1"; second="$2"; shift 2
        exec "${runner[@]}" --managed-live "$first" "$second" "$@"
        ;;
    --*)
        # Bare options with no mode: an offline run with those options.
        exec "${runner[@]}" "$@"
        ;;
    *)
        collect_paths "$@"
        ;;
esac

command -v findmnt >/dev/null || { echo "findmnt is required for --live" >&2; exit 2; }
for mountpoint in "${mountpoints[@]}"; do
    [[ -d "$mountpoint" ]] || { echo "not a directory: $mountpoint" >&2; exit 2; }
done
fstype="$(findmnt -T "${mountpoints[0]}" -n -o FSTYPE | head -n 1)"
[[ "$fstype" == fuse* ]] || {
    echo "refusing primary non-FUSE path ${mountpoints[0]} (type: ${fstype:-unknown})" >&2
    exit 2
}

exec "${runner[@]}" --live "${mountpoints[@]}" "${passthrough[@]}"
