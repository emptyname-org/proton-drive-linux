//! Reading a Google Photos **Takeout** export: what is in the archives, which
//! album each photo belongs to, and when it was taken.
//!
//! This module is pure structure — it opens the archives' central directories
//! and reads the small JSON sidecars, never the media bytes. The import itself
//! (hashing, duplicate detection, upload, album creation) lives in the daemon;
//! everything here is offline-testable and has no Proton dependency.
//!
//! # What a Takeout export looks like
//!
//! Google splits an export into numbered zips (`takeout-20260817T...-001.zip`,
//! `-002.zip`, …) that are *independent* archives — no zip64 spanning — but a
//! photo and its JSON sidecar can land in different parts, and one album's
//! folder can be spread over several. So the scan treats the whole set as one
//! namespace keyed by the path inside the export:
//!
//! ```text
//! Takeout/Google Photos/Iceland 2019/IMG_0042.jpg
//! Takeout/Google Photos/Iceland 2019/IMG_0042.jpg.supplemental-metadata.json
//! Takeout/Google Photos/Iceland 2019/metadata.json        <- album title
//! Takeout/Google Photos/Photos from 2019/IMG_0043.jpg     <- no album
//! Takeout/Google Photos/Trash/IMG_0044.jpg                <- skipped
//! ```
//!
//! The service folder (`Google Photos`) is localized, as are the year folders
//! (`Photos from 2019`) and the trash folder, so nothing here matches on the
//! service name: the first component is dropped when it is the export root, the
//! second is the service, and the rest is `<folder>/<file>`. A folder is an
//! **album** when it carries an album metadata sidecar naming it; anything else
//! is a plain bucket that imports into the timeline only. Trash is matched by
//! name against [`TRASH_FOLDERS`] — the one place a localized name is
//! unavoidable, because a trashed photo is not marked as such anywhere else.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};

/// Folder names Google uses for the trash bucket, lowercased. A Takeout export
/// only contains one when the user asked for it, and its photos are deleted —
/// importing them would resurrect them, so they are skipped.
///
/// Localized, and necessarily incomplete: a locale not listed here imports its
/// trash as ordinary timeline photos. Extend as reports come in rather than
/// guessing at transliterations.
pub const TRASH_FOLDERS: &[&str] = &[
    "trash",
    "bin",
    "papierkorb",
    "corbeille",
    "papelera",
    "papelera de reciclaje",
    "cestino",
    "lixeira",
    "prullenbak",
    "kosz",
    "koš",
    "skräpkorgen",
    "papperskorg",
    "papirkurv",
    "roskakori",
    "kuka",
    "回收站",
    "回收筒",
    "ゴミ箱",
    "휴지통",
    "корзина",
    "кошик",
    "çöp kutusu",
    "σκουπίδια",
];

/// Sidecar file names that describe the *folder* (its album title) rather than
/// one photo, lowercased. Google has renamed this file more than once.
const ALBUM_METADATA_NAMES: &[&str] = &[
    "metadata.json",
    "album-metadata.json",
    "print-subscriptions.json",
    "shared_album_comments.json",
    "user-generated-memory-titles.json",
];

/// The suffix Google appends to a photo's sidecar, lowercased. Truncation means
/// only a *prefix* of it may survive (see [`strip_sidecar_suffix`]).
const SUPPLEMENTAL: &str = ".supplemental-metadata";

/// File extensions imported as photos or videos. Anything else in the export —
/// `archive_browser.html`, `.csv` activity logs, `print-subscriptions.json` —
/// is not media and is ignored.
const MEDIA_EXTENSIONS: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("png", "image/png"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("heic", "image/heic"),
    ("heif", "image/heif"),
    ("avif", "image/avif"),
    ("dng", "image/x-adobe-dng"),
    ("cr2", "image/x-canon-cr2"),
    ("cr3", "image/x-canon-cr3"),
    ("nef", "image/x-nikon-nef"),
    ("arw", "image/x-sony-arw"),
    ("orf", "image/x-olympus-orf"),
    ("rw2", "image/x-panasonic-rw2"),
    ("raf", "image/x-fuji-raf"),
    ("mp4", "video/mp4"),
    ("m4v", "video/x-m4v"),
    ("mov", "video/quicktime"),
    ("avi", "video/x-msvideo"),
    ("mkv", "video/x-matroska"),
    ("webm", "video/webm"),
    ("3gp", "video/3gpp"),
    ("mts", "video/mp2t"),
    ("m2ts", "video/mp2t"),
    ("mpg", "video/mpeg"),
    ("mpeg", "video/mpeg"),
];

/// The media type for `name`'s extension, or `None` when it is not media this
/// import handles.
pub fn media_type_of(name: &str) -> Option<&'static str> {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    MEDIA_EXTENSIONS
        .iter()
        .find(|(candidate, _)| *candidate == ext)
        .map(|(_, media_type)| *media_type)
}

/// Where a photo sits in the export, which decides whether it joins an album.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Bucket {
    /// A named album folder — the photo joins that album *and* the timeline.
    Album(String),
    /// A year folder (`Photos from 2019`) or the export root: timeline only.
    Timeline,
    /// The archive bucket: timeline only, like Google shows it.
    Archived,
}

/// One media file found in the export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TakeoutPhoto {
    /// Index into the archive list the scan was given.
    pub archive: usize,
    /// The entry's full path inside that archive — how the importer reopens it.
    pub entry: String,
    /// The name to upload under: the sidecar's `title` when it has one (Google
    /// mangles long names on disk but keeps the original there), else the file
    /// name on disk.
    pub name: String,
    /// Uncompressed size in bytes, from the zip's central directory.
    pub size: u64,
    /// Media type from the extension.
    pub media_type: String,
    /// Capture time in epoch seconds, from `photoTakenTime` (falling back to
    /// `creationTime`). `None` when the photo has no sidecar — the upload then
    /// defaults it to now, as the SDK does.
    pub capture_time: Option<i64>,
    /// The sidecar's `favorited` flag.
    pub favorite: bool,
    /// Which bucket the file was found in.
    pub bucket: Bucket,
}

/// Everything one scan found, ready to be turned into an import plan.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TakeoutScan {
    /// Every importable media file, in archive then entry order.
    pub photos: Vec<TakeoutPhoto>,
    /// Album names in the order first seen, so the importer creates them in a
    /// stable order.
    pub albums: Vec<String>,
    /// Media files skipped because they sit in the trash bucket.
    pub skipped_trashed: usize,
    /// Entries that were neither media nor a sidecar (`archive_browser.html`,
    /// activity `.csv`s, `.mp` motion-photo parts).
    pub skipped_other: usize,
}

impl TakeoutScan {
    /// Total bytes of media to be read, before duplicate detection.
    pub fn total_bytes(&self) -> u64 {
        self.photos.iter().map(|p| p.size).sum()
    }
}

/// Google's per-photo sidecar. Only the fields the import uses are read; the
/// file also carries geo data, people tags and view counts.
#[derive(Debug, Default, Deserialize)]
struct PhotoSidecar {
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "photoTakenTime", default)]
    photo_taken_time: Option<SidecarTime>,
    #[serde(rename = "creationTime", default)]
    creation_time: Option<SidecarTime>,
    #[serde(default)]
    favorited: bool,
}

/// A sidecar timestamp: epoch seconds, as a *string*.
#[derive(Debug, Deserialize)]
struct SidecarTime {
    #[serde(default)]
    timestamp: Option<String>,
}

impl SidecarTime {
    fn epoch_seconds(&self) -> Option<i64> {
        self.timestamp.as_ref()?.parse().ok()
    }
}

/// A folder-level sidecar. `title` is the album name as the user typed it,
/// which is what the folder name is derived (and sometimes truncated) from.
#[derive(Debug, Deserialize)]
struct AlbumSidecar {
    #[serde(default)]
    title: Option<String>,
}

/// One entry as the scan sees it, independent of where it came from.
struct RawEntry {
    archive: usize,
    entry: String,
    /// `<folder>/<file>` with the export root and service folder removed.
    relative: String,
    size: u64,
}

impl RawEntry {
    /// The folder component of `relative`, or `""` at the service root.
    fn folder(&self) -> &str {
        match self.relative.rfind('/') {
            Some(index) => &self.relative[..index],
            None => "",
        }
    }

    /// The file name component of `relative`.
    fn file_name(&self) -> &str {
        match self.relative.rfind('/') {
            Some(index) => &self.relative[index + 1..],
            None => &self.relative,
        }
    }
}

/// Scan a set of Takeout zip archives into one plan.
///
/// The archives are treated as one export: an album folder split across parts
/// is one album, and a sidecar in part 3 is matched to its photo in part 1.
/// Reads only the central directory and the JSON sidecars — no media bytes, so
/// this stays fast on a 200 GB export.
pub fn scan_archives(archives: &[PathBuf]) -> CoreResult<TakeoutScan> {
    let mut entries: Vec<RawEntry> = Vec::new();
    let mut sidecars: HashMap<String, Vec<u8>> = HashMap::new();

    for (index, path) in archives.iter().enumerate() {
        let file = std::fs::File::open(path)
            .map_err(|e| CoreError::invalid(format!("cannot open {}: {e}", path.display())))?;
        let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| {
            CoreError::invalid(format!("{} is not a readable zip: {e}", path.display()))
        })?;

        for position in 0..zip.len() {
            let mut entry = zip.by_index(position).map_err(|e| {
                CoreError::invalid(format!("{}: unreadable entry: {e}", path.display()))
            })?;
            if entry.is_dir() {
                continue;
            }
            // `enclosed_name` refuses paths that escape the archive root, which
            // is exactly the check we want before using one as a key.
            let Some(name) = entry.enclosed_name() else {
                continue;
            };
            let name = name.to_string_lossy().replace('\\', "/");
            let Some(relative) = strip_export_prefix(&name) else {
                continue;
            };
            let size = entry.size();

            // Sidecars are read now, while the entry is open: they are a few
            // hundred bytes each and reopening the archive per photo later would
            // be a second full pass.
            if relative.to_ascii_lowercase().ends_with(".json") {
                let mut body = Vec::with_capacity(size.min(64 * 1024) as usize);
                if entry.read_to_end(&mut body).is_ok() {
                    sidecars.insert(relative.to_string(), body);
                }
                continue;
            }

            entries.push(RawEntry {
                archive: index,
                entry: name.clone(),
                relative: relative.to_string(),
                size,
            });
        }
    }

    Ok(assemble(entries, &sidecars))
}

/// Build the scan from raw entries plus the sidecar bodies keyed by their path
/// inside the export. Split out from [`scan_archives`] so the matching rules can
/// be tested without building zip fixtures.
fn assemble(entries: Vec<RawEntry>, sidecars: &HashMap<String, Vec<u8>>) -> TakeoutScan {
    // Album titles per folder, from the folder-level sidecars.
    let mut album_titles: HashMap<String, String> = HashMap::new();
    for (path, body) in sidecars {
        let (folder, file) = split_folder(path);
        if !ALBUM_METADATA_NAMES.contains(&file.to_ascii_lowercase().as_str()) {
            continue;
        }
        if let Ok(album) = serde_json::from_slice::<AlbumSidecar>(body)
            && let Some(title) = album.title.filter(|t| !t.trim().is_empty())
        {
            album_titles.insert(folder.to_string(), title);
        }
    }

    // Per-photo sidecars, indexed by folder so a truncated name only ever
    // matches within its own folder.
    let mut per_folder_sidecars: HashMap<String, BTreeMap<String, &Vec<u8>>> = HashMap::new();
    for (path, body) in sidecars {
        let (folder, file) = split_folder(path);
        if ALBUM_METADATA_NAMES.contains(&file.to_ascii_lowercase().as_str()) {
            continue;
        }
        per_folder_sidecars
            .entry(folder.to_string())
            .or_default()
            .insert(file.to_string(), body);
    }

    let mut scan = TakeoutScan::default();
    let mut seen_albums: HashMap<String, ()> = HashMap::new();

    for raw in &entries {
        let folder = raw.folder().to_string();
        let file_name = raw.file_name().to_string();

        if is_trash_folder(&folder) {
            if media_type_of(&file_name).is_some() {
                scan.skipped_trashed += 1;
            }
            continue;
        }
        let Some(media_type) = media_type_of(&file_name) else {
            scan.skipped_other += 1;
            continue;
        };

        let sidecar = per_folder_sidecars
            .get(&folder)
            .and_then(|in_folder| find_sidecar(&file_name, in_folder))
            .and_then(|body| serde_json::from_slice::<PhotoSidecar>(body).ok())
            .unwrap_or_default();

        let bucket = match album_titles.get(&folder) {
            Some(title) => Bucket::Album(title.clone()),
            None if is_archive_folder(&folder) => Bucket::Archived,
            None => Bucket::Timeline,
        };
        if let Bucket::Album(title) = &bucket
            && seen_albums.insert(title.clone(), ()).is_none()
        {
            scan.albums.push(title.clone());
        }

        // The sidecar's `title` is the name Google had before it mangled the
        // on-disk one (truncation, `(1)` suffixes, character replacement), so it
        // is the better name to upload under — but only when it still looks like
        // the same file, i.e. the extension agrees.
        let name = sidecar
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty() && same_extension(title, &file_name))
            .unwrap_or(&file_name)
            .to_string();

        scan.photos.push(TakeoutPhoto {
            archive: raw.archive,
            entry: raw.entry.clone(),
            name,
            size: raw.size,
            media_type: media_type.to_string(),
            capture_time: sidecar
                .photo_taken_time
                .as_ref()
                .and_then(SidecarTime::epoch_seconds)
                .or_else(|| {
                    sidecar
                        .creation_time
                        .as_ref()
                        .and_then(SidecarTime::epoch_seconds)
                }),
            favorite: sidecar.favorited,
            bucket,
        });
    }

    scan
}

/// Drop the export root (`Takeout/`) and the localized service folder from a
/// path inside the archive, leaving `<folder>/<file>` or `<file>`.
///
/// `None` for a path with no service folder at all (the export's own
/// `archive_browser.html`), which carries nothing to import.
fn strip_export_prefix(path: &str) -> Option<&str> {
    let mut rest = path;
    // The root is usually `Takeout/`, but an export unzipped and re-zipped by
    // the user may not have it — so the root is dropped only when a service
    // folder follows it, which the component count decides.
    let components = rest.split('/').count();
    if components >= 3 {
        rest = rest.split_once('/')?.1;
    }
    // Drop the service folder (`Google Photos`, `Google Fotos`, …).
    let (_service, tail) = rest.split_once('/')?;
    Some(tail)
}

/// Split `<folder>/<file>` into its parts; the folder is `""` at the root.
fn split_folder(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(index) => (&path[..index], &path[index + 1..]),
        None => ("", path),
    }
}

fn is_trash_folder(folder: &str) -> bool {
    let name = folder.rsplit('/').next().unwrap_or(folder).to_lowercase();
    TRASH_FOLDERS.contains(&name.as_str())
}

/// Google's archive bucket, which is not an album but is not the plain timeline
/// either. Only the English name is matched; a missed locale simply imports as
/// [`Bucket::Timeline`], which is the same upload either way.
fn is_archive_folder(folder: &str) -> bool {
    let name = folder.rsplit('/').next().unwrap_or(folder).to_lowercase();
    name == "archive"
}

/// Whether two file names end in the same extension, case-insensitively.
fn same_extension(a: &str, b: &str) -> bool {
    let ext = |name: &str| {
        Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
    };
    ext(a) == ext(b)
}

/// Strip a sidecar's `.json` and any `.supplemental-metadata` suffix, including
/// the *truncated* forms Google produces.
///
/// Google caps the sidecar's file name at 51 characters, so a long photo name
/// leaves only a prefix of the suffix behind — `IMG_1234.jpg.supplemental-me`,
/// `…-metad`, or nothing at all. Any non-empty prefix of the suffix is stripped;
/// a bare `.json` sidecar loses only that.
fn strip_sidecar_suffix(sidecar: &str) -> String {
    let stem = sidecar
        .strip_suffix(".json")
        .or_else(|| sidecar.strip_suffix(".JSON"))
        .unwrap_or(sidecar);
    // Longest match first so `.supplemental-metadata` wins over `.s`.
    for length in (2..=SUPPLEMENTAL.len()).rev() {
        let candidate = &SUPPLEMENTAL[..length];
        if let Some(head) = stem.strip_suffix(candidate) {
            return head.to_string();
        }
    }
    stem.to_string()
}

/// Move a trailing `(N)` from after the extension to before it: Google writes
/// the sidecar of `IMG_1234(1).jpg` as `IMG_1234.jpg(1).json`.
fn unswap_duplicate_marker(stem: &str) -> Option<String> {
    let marker_start = stem.rfind('(')?;
    let marker = &stem[marker_start..];
    if !marker.ends_with(')')
        || !marker[1..marker.len() - 1]
            .chars()
            .all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let head = &stem[..marker_start];
    let dot = head.rfind('.')?;
    Some(format!("{}{}{}", &head[..dot], marker, &head[dot..]))
}

/// Find the sidecar belonging to `file_name` among the JSON files of its folder.
///
/// Tried in order of confidence: the exact name, the `(N)`-swapped name, then a
/// prefix match for the case where truncation cut into the photo name itself.
fn find_sidecar<'a>(
    file_name: &str,
    in_folder: &BTreeMap<String, &'a Vec<u8>>,
) -> Option<&'a Vec<u8>> {
    let mut prefix_match: Option<&'a Vec<u8>> = None;
    let mut prefix_len = 0usize;

    for (sidecar_name, body) in in_folder {
        let stem = strip_sidecar_suffix(sidecar_name);
        if stem == file_name {
            return Some(body);
        }
        if unswap_duplicate_marker(&stem).as_deref() == Some(file_name) {
            return Some(body);
        }
        // Truncated: the stem is a prefix of the photo's name. Keep the longest
        // such prefix — a folder holding `IMG_1.jpg` and `IMG_12.jpg` must not
        // let the shorter stem claim the longer photo.
        if !stem.is_empty() && file_name.starts_with(&stem) && stem.len() > prefix_len {
            prefix_len = stem.len();
            prefix_match = Some(body);
        }
    }
    prefix_match
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(archive: usize, path: &str, size: u64) -> RawEntry {
        RawEntry {
            archive,
            entry: path.to_string(),
            relative: strip_export_prefix(path).unwrap().to_string(),
            size,
        }
    }

    fn sidecar(map: &mut HashMap<String, Vec<u8>>, path: &str, body: &str) {
        let relative = strip_export_prefix(path).unwrap().to_string();
        map.insert(relative, body.as_bytes().to_vec());
    }

    #[test]
    fn strips_export_root_and_localized_service_folder() {
        assert_eq!(
            strip_export_prefix("Takeout/Google Fotos/Island 2019/IMG_1.jpg"),
            Some("Island 2019/IMG_1.jpg")
        );
        // Already unwrapped by the user: no `Takeout/` root left.
        assert_eq!(
            strip_export_prefix("Google Photos/IMG_1.jpg"),
            Some("IMG_1.jpg")
        );
        // Nothing below a service folder is nothing to import.
        assert_eq!(strip_export_prefix("archive_browser.html"), None);
    }

    #[test]
    fn album_folder_becomes_an_album_year_folder_does_not() {
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Iceland 2019/metadata.json",
            r#"{"title":"Iceland 2019"}"#,
        );
        let scan = assemble(
            vec![
                entry(0, "Takeout/Google Photos/Iceland 2019/IMG_1.jpg", 10),
                entry(0, "Takeout/Google Photos/Photos from 2019/IMG_2.jpg", 20),
            ],
            &sidecars,
        );

        assert_eq!(scan.albums, vec!["Iceland 2019"]);
        assert_eq!(scan.photos[0].bucket, Bucket::Album("Iceland 2019".into()));
        assert_eq!(scan.photos[1].bucket, Bucket::Timeline);
        assert_eq!(scan.total_bytes(), 30);
    }

    #[test]
    fn trashed_photos_are_skipped_and_counted() {
        let scan = assemble(
            vec![
                entry(0, "Takeout/Google Photos/Papierkorb/IMG_1.jpg", 10),
                entry(0, "Takeout/Google Photos/Bin/IMG_2.jpg", 10),
                entry(0, "Takeout/Google Photos/Photos from 2019/IMG_3.jpg", 10),
            ],
            &HashMap::new(),
        );

        assert_eq!(scan.skipped_trashed, 2);
        assert_eq!(scan.photos.len(), 1);
        assert_eq!(scan.photos[0].name, "IMG_3.jpg");
    }

    #[test]
    fn non_media_entries_are_ignored() {
        let scan = assemble(
            vec![
                entry(0, "Takeout/Google Photos/Photos from 2019/IMG_1.jpg", 10),
                entry(0, "Takeout/Google Photos/Photos from 2019/notes.txt", 10),
            ],
            &HashMap::new(),
        );

        assert_eq!(scan.photos.len(), 1);
        assert_eq!(scan.skipped_other, 1);
    }

    #[test]
    fn sidecar_supplies_capture_time_and_favourite() {
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/IMG_1.jpg.supplemental-metadata.json",
            r#"{"title":"IMG_1.jpg","photoTakenTime":{"timestamp":"1560000000"},"favorited":true}"#,
        );
        let scan = assemble(
            vec![entry(
                0,
                "Takeout/Google Photos/Photos from 2019/IMG_1.jpg",
                10,
            )],
            &sidecars,
        );

        assert_eq!(scan.photos[0].capture_time, Some(1_560_000_000));
        assert!(scan.photos[0].favorite);
    }

    #[test]
    fn creation_time_is_the_fallback_capture_time() {
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/IMG_1.jpg.json",
            r#"{"creationTime":{"timestamp":"1500000000"}}"#,
        );
        let scan = assemble(
            vec![entry(
                0,
                "Takeout/Google Photos/Photos from 2019/IMG_1.jpg",
                10,
            )],
            &sidecars,
        );

        assert_eq!(scan.photos[0].capture_time, Some(1_500_000_000));
    }

    #[test]
    fn truncated_supplemental_suffixes_still_match() {
        // Google caps the sidecar name at 51 chars, cutting into the suffix.
        assert_eq!(
            strip_sidecar_suffix("IMG_1.jpg.supplemental-me.json"),
            "IMG_1.jpg"
        );
        assert_eq!(
            strip_sidecar_suffix("IMG_1.jpg.supplemental-metadata.json"),
            "IMG_1.jpg"
        );
        assert_eq!(strip_sidecar_suffix("IMG_1.jpg.json"), "IMG_1.jpg");
    }

    #[test]
    fn duplicate_marker_after_the_extension_is_moved_back() {
        assert_eq!(
            unswap_duplicate_marker("IMG_1.jpg(1)").as_deref(),
            Some("IMG_1(1).jpg")
        );
        assert_eq!(unswap_duplicate_marker("IMG_1.jpg"), None);

        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/IMG_1.jpg(1).json",
            r#"{"photoTakenTime":{"timestamp":"1600000000"}}"#,
        );
        let scan = assemble(
            vec![entry(
                0,
                "Takeout/Google Photos/Photos from 2019/IMG_1(1).jpg",
                10,
            )],
            &sidecars,
        );

        assert_eq!(scan.photos[0].capture_time, Some(1_600_000_000));
    }

    #[test]
    fn longest_prefix_wins_when_the_photo_name_itself_was_truncated() {
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/IMG_1.json",
            r#"{"photoTakenTime":{"timestamp":"1"}}"#,
        );
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/IMG_12.json",
            r#"{"photoTakenTime":{"timestamp":"2"}}"#,
        );
        let scan = assemble(
            vec![entry(
                0,
                "Takeout/Google Photos/Photos from 2019/IMG_12345.jpg",
                10,
            )],
            &sidecars,
        );

        assert_eq!(scan.photos[0].capture_time, Some(2));
    }

    #[test]
    fn sidecar_title_replaces_a_mangled_on_disk_name() {
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/a_very_long_name_that_google_.jpg.json",
            r#"{"title":"a_very_long_name_that_google_truncated.jpg"}"#,
        );
        let scan = assemble(
            vec![entry(
                0,
                "Takeout/Google Photos/Photos from 2019/a_very_long_name_that_google_.jpg",
                10,
            )],
            &sidecars,
        );

        assert_eq!(
            scan.photos[0].name,
            "a_very_long_name_that_google_truncated.jpg"
        );
    }

    #[test]
    fn a_title_with_a_different_extension_is_not_trusted() {
        // Motion photos name their `.MP` part after the `.jpg`; taking the title
        // verbatim would upload the video under an image name.
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/IMG_1.mp4.json",
            r#"{"title":"IMG_1.jpg"}"#,
        );
        let scan = assemble(
            vec![entry(
                0,
                "Takeout/Google Photos/Photos from 2019/IMG_1.mp4",
                10,
            )],
            &sidecars,
        );

        assert_eq!(scan.photos[0].name, "IMG_1.mp4");
        assert_eq!(scan.photos[0].media_type, "video/mp4");
    }

    #[test]
    fn an_album_split_across_archives_stays_one_album() {
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Iceland 2019/metadata.json",
            r#"{"title":"Iceland 2019"}"#,
        );
        let scan = assemble(
            vec![
                entry(0, "Takeout/Google Photos/Iceland 2019/IMG_1.jpg", 10),
                entry(1, "Takeout/Google Photos/Iceland 2019/IMG_2.jpg", 10),
            ],
            &sidecars,
        );

        assert_eq!(scan.albums, vec!["Iceland 2019"]);
        assert_eq!(scan.photos.len(), 2);
        assert_eq!(scan.photos[1].archive, 1);
    }

    #[test]
    fn archive_bucket_imports_to_the_timeline() {
        let scan = assemble(
            vec![entry(0, "Takeout/Google Photos/Archive/IMG_1.jpg", 10)],
            &HashMap::new(),
        );

        assert_eq!(scan.photos[0].bucket, Bucket::Archived);
        assert!(scan.albums.is_empty());
    }

    /// Write a zip holding `files` and return its path. No `tempfile`
    /// dev-dependency: the daemon crates deliberately carry none, and one
    /// uniquely named file under the temp dir is all this needs.
    fn write_zip(tag: &str, files: &[(&str, &[u8])]) -> PathBuf {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "pdfs-takeout-test-{tag}-{}.zip",
            std::process::id()
        ));
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, body) in files {
            zip.start_file(*name, options).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    #[test]
    fn scans_a_two_part_export_where_the_sidecar_is_in_the_other_part() {
        // The case that makes the whole set one scan: part 1 has the photo,
        // part 2 has its metadata and the album title.
        let first = write_zip(
            "part1",
            &[(
                "Takeout/Google Photos/Iceland 2019/IMG_1.jpg",
                b"the photo bytes",
            )],
        );
        let second = write_zip(
            "part2",
            &[
                (
                    "Takeout/Google Photos/Iceland 2019/metadata.json",
                    br#"{"title":"Iceland 2019"}"#.as_slice(),
                ),
                (
                    "Takeout/Google Photos/Iceland 2019/IMG_1.jpg.supplemental-metadata.json",
                    br#"{"title":"IMG_1.jpg","photoTakenTime":{"timestamp":"1560000000"}}"#
                        .as_slice(),
                ),
                (
                    "Takeout/Google Photos/Trash/IMG_9.jpg",
                    b"deleted".as_slice(),
                ),
            ],
        );

        let scan = scan_archives(&[first.clone(), second.clone()]).unwrap();
        let _ = std::fs::remove_file(&first);
        let _ = std::fs::remove_file(&second);

        assert_eq!(scan.photos.len(), 1);
        assert_eq!(scan.photos[0].name, "IMG_1.jpg");
        assert_eq!(scan.photos[0].archive, 0, "the photo is in part 1");
        assert_eq!(scan.photos[0].size, "the photo bytes".len() as u64);
        assert_eq!(scan.photos[0].capture_time, Some(1_560_000_000));
        assert_eq!(scan.photos[0].bucket, Bucket::Album("Iceland 2019".into()));
        assert_eq!(scan.albums, vec!["Iceland 2019"]);
        assert_eq!(scan.skipped_trashed, 1);
    }

    #[test]
    fn a_file_that_is_not_a_zip_is_a_readable_error() {
        let path =
            std::env::temp_dir().join(format!("pdfs-takeout-not-a-zip-{}", std::process::id()));
        std::fs::write(&path, b"not a zip at all").unwrap();
        let error = scan_archives(std::slice::from_ref(&path)).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(
            error.message.contains("not a readable zip"),
            "unexpected message: {}",
            error.message
        );
    }
}
