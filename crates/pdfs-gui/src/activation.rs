//! Shared Drive-result activation policy for the GTK browser and quick prompt.
//!
//! Media should be handed to its mounted path so players can issue range reads
//! through FUSE. Everything else opens through the mount too whenever the mount
//! exposes it, and only falls back to materialize-then-open when it does not —
//! see [`mounted_target`].
//!
//! Both binaries `#[path]`-include this file, so each compiles its own copy and
//! anything only one of them calls looks dead in the other. That is what the
//! `dead_code` allowances below are for — not code nobody uses.

use std::path::{Path, PathBuf};

use pdfs_core::control::{PhotoKind, SearchHit};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriveActivation {
    Folder,
    MountedMedia,
    Materialize,
}

pub(crate) fn drive_activation(name: &str, is_dir: bool) -> DriveActivation {
    if is_dir {
        DriveActivation::Folder
    } else if is_streamable_media(name) {
        DriveActivation::MountedMedia
    } else {
        DriveActivation::Materialize
    }
}

pub(crate) fn mounted_path(mountpoint: &Path, relative_path: &str) -> PathBuf {
    mountpoint.join(relative_path)
}

/// Where a Drive hit should be opened from, when a mount exposes it.
///
/// Materialised content is a *copy*: it lives in the cache under its content
/// hash, so an editor saving into it writes somewhere Drive will never look,
/// and the cache is free to evict it afterwards. The mounted path is the real
/// file — reads pull 4 MiB blocks on demand and a write becomes a new revision
/// — so it wins for every kind of file, not just streamable media.
///
/// [`Request::OpenFile`](pdfs_core::control::Request::OpenFile) stays the
/// fallback for hits no mount exposes: the search index also covers nodes
/// outside the mounted tree (a mirror folder's contents, a stale row), and
/// those can still be read, just not edited in place.
///
/// The `stat` is why this returns an `Option` rather than a path: it is the
/// only way to tell those two cases apart, and it costs one syscall on a path
/// the caller is about to hand to an application anyway.
#[allow(dead_code)] // prompt only
pub(crate) fn mounted_target(mountpoint: &Path, hit: &SearchHit) -> Option<PathBuf> {
    present(mounted_or_relative(mountpoint, hit))
}

/// [`mounted_target`] for callers holding a mountpoint-relative path rather
/// than a search hit — the browser's listing entries, which are by definition
/// paths in the primary mount's tree.
#[allow(dead_code)] // browser only
pub(crate) fn mounted_target_rel(mountpoint: &Path, relative_path: &str) -> Option<PathBuf> {
    present(mounted_path(mountpoint, relative_path))
}

fn present(path: PathBuf) -> Option<PathBuf> {
    path.symlink_metadata().is_ok().then_some(path)
}

/// The mount path for a hit, whether or not anything is there — for callers
/// that have no fallback (a folder cannot be materialised) or already know the
/// entry came from a mount listing.
#[allow(dead_code)] // prompt only
pub(crate) fn mounted_or_relative(mountpoint: &Path, hit: &SearchHit) -> PathBuf {
    hit.mounted_path
        .as_deref()
        .map_or_else(|| mounted_path(mountpoint, &hit.path), PathBuf::from)
}

fn is_streamable_media(name: &str) -> bool {
    if PhotoKind::classify(Some(name), None) == PhotoKind::Video {
        return true;
    }

    let extension = Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();

    matches!(
        extension.to_ascii_lowercase().as_str(),
        "mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a" | "aac"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_use_the_mount_regardless_of_extension() {
        assert_eq!(
            drive_activation("recordings.mp4", true),
            DriveActivation::Folder
        );
    }

    #[test]
    fn media_extensions_use_fuse_path_case_insensitively() {
        for name in ["movie.MKV", "voice.opus", "song.FLAC", "clip.m4v"] {
            assert_eq!(
                drive_activation(name, false),
                DriveActivation::MountedMedia,
                "{name}"
            );
        }
    }

    #[test]
    fn ordinary_files_are_materialized() {
        for name in ["report.pdf", "notes.txt", "archive.zip", "no-extension"] {
            assert_eq!(
                drive_activation(name, false),
                DriveActivation::Materialize,
                "{name}"
            );
        }
    }

    #[test]
    fn mounted_paths_preserve_relative_components() {
        assert_eq!(
            mounted_path(Path::new("/mnt/drive"), "Videos/a movie.mkv"),
            Path::new("/mnt/drive/Videos/a movie.mkv")
        );
    }
}
