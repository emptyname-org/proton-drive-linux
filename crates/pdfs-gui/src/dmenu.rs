//! `pdfs-prompt --dmenu`: the same search, presented in the user's own launcher.
//!
//! Everything here is blocking and GTK-free — the process exists for one
//! round-trip and then exits, so there is no main loop to keep off. It reuses
//! the GTK prompt's [`Hit`](crate::Hit) model and ranking, and its Drive
//! activation policy, so the two front ends cannot drift on what "open" means.
//!
//! Launchers filter a static list; they cannot call back for a new one per
//! keystroke. So the interaction is a loop: the first menu shows pinned files
//! and accepts free text, and any text that matches no entry becomes the next
//! search. Escape (an empty selection) ends it.

use std::path::{Path, PathBuf};

use pdfs_core::config::AppDirs;
use pdfs_core::control::{
    Request, Response, SearchFilters, SearchHit, SearchKind, SearchSource, send,
};
use pdfs_core::menu::{self, MenuChoice, MenuItem, PromptConfig};
use pdfs_core::opener::{self, OpenWith};

use crate::activation::{DriveActivation, drive_activation, mounted_or_relative, mounted_target};
use crate::{Hit, file_name, is_document, is_image, is_media, rank_hits};

/// Guard against a launcher that keeps handing back text we cannot satisfy —
/// without it a scripted (non-interactive) menu could spin forever.
const MAX_ROUNDS: usize = 64;

pub(crate) struct Options {
    /// Launcher argv from `--menu`, overriding the configured/detected one.
    pub menu: Option<Vec<String>>,
    /// Skip the pinned-files round and search for this immediately.
    pub query: Option<String>,
}

/// Run the launcher flow. Returns an error only for conditions the user must
/// act on (no daemon, no launcher); a plain cancel is `Ok`.
pub(crate) fn run(options: Options) -> Result<(), String> {
    let dirs = AppDirs::new().map_err(|e| format!("cannot resolve app dirs: {e}"))?;
    let config = dirs.load_config();
    let prompt: PromptConfig = config.resolved_prompt();
    let policy: OpenWith = config.resolved_open_with();
    let socket = dirs.control_socket();

    let menu_argv =
        menu::resolve_menu(options.menu.as_ref().or(prompt.menu.as_ref())).ok_or_else(|| {
            "no launcher found — install fuzzel/rofi/wofi, or set prompt.menu in config.json"
                .to_string()
        })?;

    let mountpoint = match request(&socket, Request::Status)? {
        Response::Status { mountpoint, .. } => PathBuf::from(mountpoint),
        _ => dirs.default_mountpoint(),
    };

    let mut query = options
        .query
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty());
    for _ in 0..MAX_ROUNDS {
        let hits = match &query {
            Some(text) => search(&socket, text, prompt.resolved_menu_limit())?,
            None => pins(&socket)?,
        };
        let items: Vec<MenuItem> = if hits.is_empty() {
            vec![MenuItem::new(placeholder(query.as_deref()), None)]
        } else {
            label_all(&hits)
        };

        match menu::run(&menu_argv, &title(query.as_deref()), &items).map_err(|e| e.to_string())? {
            MenuChoice::Item(index) => {
                let Some(hit) = hits.get(index) else {
                    // The placeholder row. Selecting it is not a cancel — the
                    // user pressed Enter on "type to search", so start over
                    // with the pinned list rather than quitting on them.
                    query = None;
                    continue;
                };
                return open(&socket, &mountpoint, &policy, hit, query.is_none());
            }
            // No entry matched what was typed: treat it as the next query.
            MenuChoice::Custom(text) => query = Some(text),
            MenuChoice::Cancelled => return Ok(()),
        }
    }
    Ok(())
}

/// The launcher's prompt string.
///
/// A launcher renders this immediately left of the input, so it is the only
/// place to say what typing here does. "Drive" alone read as a label and left
/// the two-step nature of the flow — type, Enter, *then* results — invisible;
/// the verb and the chevron make it a search box, and echoing the current query
/// shows which round you are in.
fn title(query: Option<&str>) -> String {
    match query {
        Some(text) => format!("Drive: {text} › "),
        None => "Search Drive › ".to_string(),
    }
}

/// Text for the one row shown when there is nothing to list.
///
/// A launcher handed an empty list shows a bare prompt, and some refuse to
/// return anything at all from an empty menu — which would strand the user on
/// the very first round, since a fresh account has no pins. A single row keeps
/// the menu non-empty and says why it is empty.
///
/// A fuzzy launcher can match this row against what the user types, and the
/// selection comes back as a label with no way to tell the two apart — so the
/// wording is parenthesised and avoids words that read like a filename query.
fn placeholder(query: Option<&str>) -> String {
    match query {
        Some(text) => format!("(no matches for “{text}”)"),
        None => "(no pinned files — type to search)".to_string(),
    }
}

/// Pinned files — the same "you asked to keep these" set the GTK prompt opens
/// with, and the only listing the daemon offers without a query.
fn pins(socket: &Path) -> Result<Vec<Hit>, String> {
    let Response::Pins { pins } = request(socket, Request::ListPins)? else {
        return Ok(Vec::new());
    };
    Ok(pins
        .into_iter()
        .map(|pin| {
            Hit::Drive(SearchHit {
                name: file_name(&pin.path),
                path: pin.path,
                is_dir: pin.is_dir.unwrap_or(pin.recursive),
                size: 0,
                modified: 0,
                pinned: true,
                uid: pin.uid,
                mounted_path: None,
                score: 0,
            })
        })
        .collect())
}

fn search(socket: &Path, query: &str, limit: usize) -> Result<Vec<Hit>, String> {
    let reply = request(
        socket,
        Request::SearchV2 {
            query: query.to_string(),
            limit,
            filters: SearchFilters {
                sources: vec![SearchSource::Drive, SearchSource::Local],
                kind: SearchKind::All,
            },
        },
    )?;
    let Response::SearchResultsV2 {
        drive_hits,
        local_hits,
        ..
    } = reply
    else {
        return Ok(Vec::new());
    };
    let mut hits: Vec<Hit> = drive_hits.into_iter().map(Hit::Drive).collect();
    hits.extend(local_hits.into_iter().map(Hit::Local));
    rank_hits(&mut hits);
    hits.truncate(limit);
    Ok(hits)
}

/// One launcher line per hit. Labels must be unique: they are how the selection
/// is matched back, and two files of the same name in the same folder listing
/// would otherwise be indistinguishable.
fn label_all(hits: &[Hit]) -> Vec<MenuItem> {
    let mut items: Vec<MenuItem> = Vec::with_capacity(hits.len());
    for hit in hits {
        let mut label = format!("{}   ·   {}", hit.name(), hit.location());
        let mut suffix = 2;
        while items.iter().any(|item| item.label == label) {
            label = format!("{}   ·   {} ({suffix})", hit.name(), hit.location());
            suffix += 1;
        }
        items.push(MenuItem::new(label, Some(icon_name(hit))));
    }
    items
}

/// Icon theme name for a hit.
///
/// Coarse on purpose: `content_type_guess` would give an exact MIME name, but
/// only with file *contents* to sniff — a Drive hit is metadata, and passing it
/// an empty buffer answers `application-x-zerosize` for everything. The generic
/// names below exist in every icon theme.
fn icon_name(hit: &Hit) -> String {
    if hit.is_dir() {
        return "folder".to_string();
    }
    let name = hit.name();
    if is_image(name) {
        "image-x-generic"
    } else if is_media(name) {
        "video-x-generic"
    } else if is_document(name) {
        "text-x-generic"
    } else {
        "application-x-generic"
    }
    .to_string()
}

/// Open a chosen hit, mirroring the GTK prompt: local files are already on
/// disk, a Drive folder or streamable file goes through the mount, and anything
/// else is materialised by the daemon first.
fn open(
    socket: &Path,
    mountpoint: &Path,
    policy: &OpenWith,
    hit: &Hit,
    from_pins: bool,
) -> Result<(), String> {
    match hit {
        Hit::Local(local) => {
            opener::open(policy, Path::new(&local.path), local.is_dir);
            Ok(())
        }
        Hit::Drive(drive) => {
            // A folder has no materialised form, and a pin row's `is_dir` is pin
            // policy rather than node kind, so both go to the mount unchecked.
            let unconditional = from_pins
                || matches!(
                    drive_activation(&drive.name, drive.is_dir),
                    DriveActivation::Folder | DriveActivation::MountedMedia
                );
            if unconditional {
                opener::open(
                    policy,
                    &mounted_or_relative(mountpoint, drive),
                    drive.is_dir,
                );
                return Ok(());
            }
            // Anything else the mount exposes opens there too, so an editor
            // saves back into Drive instead of into a cache blob.
            if let Some(path) = mounted_target(mountpoint, drive) {
                opener::open(policy, &path, false);
                return Ok(());
            }
            match request(
                socket,
                Request::OpenFile {
                    path: drive.path.clone(),
                    uid: Some(drive.uid.clone()),
                },
            )? {
                Response::FilePath { path } => {
                    // The reply is a content-hash blob; the rules key off the
                    // Drive name, which is what the user actually opened.
                    opener::open_named(policy, Path::new(&path), &drive.name, false);
                    Ok(())
                }
                Response::Error { message, .. } => Err(format!("could not open: {message}")),
                _ => Err("unexpected reply from the daemon".to_string()),
            }
        }
    }
}

/// One control-socket round-trip, with the daemon-is-down case phrased for a
/// terminal rather than a status page.
fn request(socket: &Path, request: Request) -> Result<Response, String> {
    send(socket, &request).map_err(|e| format!("cannot reach the Proton Drive daemon: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfs_core::control::LocalHit;

    fn drive(name: &str, path: &str, is_dir: bool) -> Hit {
        Hit::Drive(SearchHit {
            name: name.into(),
            path: path.into(),
            is_dir,
            size: 0,
            modified: 0,
            pinned: false,
            uid: "uid".into(),
            mounted_path: None,
            score: 0,
        })
    }

    #[test]
    fn labels_are_unique_so_a_selection_maps_back_to_one_hit() {
        let hits = vec![
            drive("notes.md", "Docs/notes.md", false),
            Hit::Local(LocalHit {
                name: "notes.md".into(),
                path: "/home/me/Docs/notes.md".into(),
                is_dir: false,
                size: 0,
                modified: 0,
                score: 0,
            }),
            drive("notes.md", "Docs/notes.md", false),
        ];
        let items = label_all(&hits);
        assert_eq!(items.len(), 3);
        assert_ne!(items[0].label, items[2].label);
        let unique: std::collections::HashSet<_> =
            items.iter().map(|item| item.label.clone()).collect();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn the_prompt_says_a_search_is_what_enter_does() {
        let first = title(None);
        assert!(first.to_lowercase().contains("search"), "{first}");
        // The launcher renders the input immediately after the prompt, so the
        // prompt has to carry its own separator or it runs into what is typed.
        assert!(first.ends_with(' '), "{first:?}");

        let refined = title(Some("md"));
        assert!(refined.contains("md"), "{refined}");
        assert!(refined.ends_with(' '), "{refined:?}");
    }

    #[test]
    fn an_empty_round_still_has_a_row_explaining_itself() {
        assert!(placeholder(None).contains("type to search"));
        assert!(placeholder(Some("md")).contains("md"));
    }

    #[test]
    fn icons_are_theme_names_the_launcher_can_look_up() {
        assert_eq!(icon_name(&drive("Photos", "Photos", true)), "folder");
        assert_eq!(
            icon_name(&drive("a.png", "a.png", false)),
            "image-x-generic"
        );
        assert_eq!(icon_name(&drive("a.md", "a.md", false)), "text-x-generic");
        assert_eq!(
            icon_name(&drive("a.bin", "a.bin", false)),
            "application-x-generic"
        );
    }
}
