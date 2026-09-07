//! Builds the searchable SQLite index from a PastPerfect Data folder, plus
//! any standalone photo archive folders (not linked to PastPerfect records).
//! Direct port of the prototype's `ingest.py`.

use crate::dbf::{find_file, DbfTable};
use crate::thumbnails::get_preview_jpeg;
use rusqlite::Connection;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "tif", "tiff", "bmp", "png", "gif"];
pub const THUMB_MAX_DIM: u32 = 1600;

pub struct PhotoArchiveRoot {
    pub label: String,
    pub path: PathBuf,
}

/// (catalog label, dbf filename, title fields, body fields, detail fields)
struct CatalogSpec {
    catalog: &'static str,
    filename: &'static str,
    title_fields: &'static [&'static str],
    body_fields: &'static [&'static str],
    detail_fields: &'static [&'static str],
}

const CATALOG_TABLES: &[CatalogSpec] = &[
    CatalogSpec {
        catalog: "Object",
        filename: "OBJECTS.DBF",
        title_fields: &["OBJNAME", "OBJNAME2", "OBJNAME3"],
        body_fields: &[
            "DESCRIP", "CREATOR", "CREATOR2", "CREATOR3", "MATERIAL", "MEDIUM",
            "SUBJECTS", "PEOPLE", "SITE", "NOTES", "COLLECTION", "PROVENANCE",
            "OTHERNAME", "HOMELOC", "STERMS", "CLASSES",
        ],
        detail_fields: &[
            "OBJNAME", "OBJNAME2", "OBJNAME3", "ACCESSNO", "DATE", "DESCRIP",
            "CREATOR", "CREATOR2", "CREATOR3", "MATERIAL", "MEDIUM", "SUBJECTS",
            "PEOPLE", "SITE", "COLLECTION", "PROVENANCE", "CONDITION", "HOMELOC",
            "NOTES",
        ],
    },
    CatalogSpec {
        catalog: "Photo",
        filename: "PHOTOS.DBF",
        title_fields: &["OBJNAME", "OBJNAME2", "OBJNAME3", "TITLE"],
        body_fields: &[
            "CAPTION", "DESCRIP", "CREATOR", "SUBJECTS", "PEOPLE", "SITE", "NOTES",
            "COLLECTION", "PLACE", "EVENT", "STERMS", "CLASSES",
        ],
        detail_fields: &[
            "OBJNAME", "TITLE", "ACCESSNO", "DATE", "CAPTION", "DESCRIP", "CREATOR",
            "SUBJECTS", "PEOPLE", "SITE", "PLACE", "EVENT", "COLLECTION",
            "CONDITION", "HOMELOC", "NOTES",
        ],
    },
    CatalogSpec {
        catalog: "Archive",
        filename: "ARCHIVES.DBF",
        title_fields: &["OBJNAME", "OBJNAME2", "OBJNAME3", "TITLE"],
        body_fields: &[
            "DESCRIP", "CREATOR", "SUBJECTS", "PEOPLE", "SITE", "NOTES",
            "COLLECTION", "SERIES", "NARRATOR", "STERMS", "CLASSES",
        ],
        detail_fields: &[
            "OBJNAME", "TITLE", "ACCESSNO", "DATE", "DESCRIP", "CREATOR",
            "SUBJECTS", "PEOPLE", "SITE", "SERIES", "NARRATOR", "COLLECTION",
            "CONDITION", "HOMELOC", "NOTES",
        ],
    },
    CatalogSpec {
        catalog: "Library Item",
        filename: "LIBRARY.DBF",
        title_fields: &["OBJNAME", "OBJNAME2", "OBJNAME3", "TITLE", "TITLEX"],
        body_fields: &[
            "DESCRIP", "AUTHORX", "SUBJECTS", "PEOPLE", "SITE", "NOTES",
            "COLLECTION", "SERIES", "PUBLISHER", "STERMS", "CLASSES",
        ],
        detail_fields: &[
            "OBJNAME", "TITLE", "TITLEX", "ACCESSNO", "DATE", "AUTHORX", "DESCRIP",
            "SUBJECTS", "PEOPLE", "SITE", "SERIES", "PUBLISHER", "COLLECTION",
            "CONDITION", "HOMELOC", "NOTES",
        ],
    },
];

const PEOPLE_TABLE: CatalogSpec = CatalogSpec {
    catalog: "Person",
    filename: "PEOPLE.DBF",
    title_fields: &["NAME", "OTHERNAME"],
    body_fields: &["OCCUPATION", "NOTES", "BIRTHPLACE", "LASTPLACE", "NATIONAL", "ROLE", "CAPTION"],
    detail_fields: &[
        "NAME", "OTHERNAME", "BORN", "DIED", "BIRTHPLACE", "LASTPLACE", "OCCUPATION",
        "NATIONAL", "ROLE", "FATHER", "MOTHER", "NOTES",
    ],
};

pub type ProgressFn<'a> = dyn FnMut(&str, Option<u64>, Option<u64>) + 'a;

fn noop_progress(_phase: &str, _current: Option<u64>, _total: Option<u64>) {}

// ------------------------------------------------------------ MEDIA.DBF

/// The last path segment of a reference stored in a DBF, treating both '/'
/// and '\' as separators.
///
/// PastPerfect writes Windows paths into these fields regardless of platform,
/// and `std::path` on macOS/Linux does not treat '\' as a separator -- so
/// `Path::new(r"c:\pp5\multimedia\x.tif").file_name()` returns the *entire*
/// string, not "x.tif".
fn stored_path_basename(raw: &str) -> Option<&str> {
    let name = raw.trim().rsplit(['/', '\\']).next()?.trim();
    (!name.is_empty()).then_some(name)
}

/// The same path with trailing dots removed from its final component, or None
/// if there were none. See `resolve_pastperfect_image` for why this matters.
fn trailing_dot_variant(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let trimmed = name.trim_end_matches('.');
    if trimmed == name || trimmed.is_empty() {
        return None;
    }
    Some(path.with_file_name(trimmed))
}

/// One row of MEDIA.DBF: a single image attached to a catalog record.
struct MediaEntry {
    sort_key: f64,
    caption: String,
    imagefile: String,
    /// Recorded location of the high-resolution master, as an absolute path
    /// from whichever machine imported it. Only its filename is usable here.
    hires_ref: String,
}

/// Every image attached to each catalog record, keyed by OBJECTID and ordered
/// by the POSITION column.
///
/// A record can have several images -- the front and back of a monument, the
/// pages of a letter -- each with its own caption. The catalog tables only
/// carry a single IMAGEFILE (which is always POSITION 1), so without this
/// every image after the first is invisible.
type MediaIndex = HashMap<String, Vec<MediaEntry>>;

fn media_key(object_id: &str) -> String {
    object_id.trim().to_uppercase()
}

fn load_media_index(data_dir: &Path, warnings: &mut Vec<String>) -> MediaIndex {
    let mut index: MediaIndex = HashMap::new();
    // No MEDIA.DBF is not an error: without it each record simply keeps the
    // single image named by its own IMAGEFILE field.
    let Some(path) = find_file(data_dir, "MEDIA.DBF") else { return index };
    let mut table = match DbfTable::open(&path) {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!(
                "MEDIA.DBF could not be read ({e}) -- records with more than one image \
                 will only show the first."
            ));
            return index;
        }
    };

    for rec in table.records() {
        let object_id = rec.get("OBJECTID").map(|s| s.trim()).unwrap_or("");
        if object_id.is_empty() {
            continue;
        }
        let imagefile = rec.get("IMAGEFILE").map(|s| s.trim().to_string()).unwrap_or_default();
        if imagefile.is_empty() {
            continue;
        }
        index.entry(media_key(object_id)).or_default().push(MediaEntry {
            sort_key: rec
                .get("POSITION")
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(0.0),
            caption: rec.get("CAPTION").map(|s| s.trim().to_string()).unwrap_or_default(),
            imagefile,
            // HIRESFILE is blank throughout this collection and LOCHIGHRES is
            // the literal text "n/a"; OLDHIRES is the field that actually
            // points at a master. Fall back through all three anyway rather
            // than assuming another install fills in the same one.
            hires_ref: ["OLDHIRES", "HIRESFILE", "LOCHIGHRES"]
                .iter()
                .filter_map(|f| rec.get(*f).map(|s| s.trim()))
                .find(|s| !s.is_empty() && !s.eq_ignore_ascii_case("n/a"))
                .unwrap_or("")
                .to_string(),
        });
    }

    // Stable sort: entries sharing a POSITION keep their order in the file,
    // so a rebuild over unchanged data always numbers images identically.
    for entries in index.values_mut() {
        entries.sort_by(|a, b| a.sort_key.total_cmp(&b.sort_key));
    }
    index
}

// ------------------------------------------------- high-resolution masters

/// Index of every file under the PastPerfect "Multimedia" folder, keyed by
/// lowercase filename.
///
/// MEDIA.DBF stores a master's location as an absolute path captured on the
/// machine that imported it (`c:\pp5\multimedia\Largeimages1\x-Large.tif`),
/// which says nothing about where the folder lives now. The file is therefore
/// found by name. Names appearing more than once are treated as ambiguous and
/// resolve to nothing, so a download is never silently the wrong file.
#[derive(Default)]
pub struct MultimediaIndex {
    by_name: HashMap<String, PathBuf>,
    ambiguous: HashSet<String>,
}

impl MultimediaIndex {
    pub fn build(dir: &Path) -> Self {
        let mut index = Self::default();
        if !dir.is_dir() {
            return index;
        }
        let mut files = Vec::new();
        collect_all_files_depth(dir, &mut files, 0);
        for path in files {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            let key = name.to_lowercase();
            if index.by_name.insert(key.clone(), path).is_some() {
                index.ambiguous.insert(key);
            }
        }
        index
    }

    /// Resolves a stored master reference to a real file, by filename only.
    fn lookup(&self, stored_ref: &str) -> Option<PathBuf> {
        let key = stored_path_basename(stored_ref)?.to_lowercase();
        if self.ambiguous.contains(&key) {
            return None;
        }
        self.by_name.get(&key).filter(|p| p.is_file()).cloned()
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

fn collect_all_files_depth(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            crate::diagnostics::error_repeating(
                "multimedia-folder-unreadable",
                "index",
                format!("could not read the Multimedia folder {}: {e}", dir.display()),
            );
            return;
        }
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_all_files_depth(&path, out, depth + 1);
        } else {
            out.push(path);
        }
    }
}

/// Everything the per-record indexing needs that is shared across all
/// catalog tables, gathered once per rebuild.
struct IngestContext {
    images_dir: PathBuf,
    thumb_cache_dir: PathBuf,
    media: MediaIndex,
    multimedia: MultimediaIndex,
}

fn resolve_pastperfect_image(imagefile: &str, images_dir: &Path) -> Option<PathBuf> {
    let raw = imagefile.trim();
    if raw.is_empty() {
        return None;
    }
    let rel: String = raw
        .chars()
        .map(|c| if c == '\\' || c == '/' { std::path::MAIN_SEPARATOR } else { c })
        .collect();
    let rel = Path::new(&rel);

    // Keep the reference inside the images folder. `join` with an absolute
    // path silently discards the base entirely, and ".." walks out of it --
    // either way a malformed IMAGEFILE value would pull in a file that isn't
    // part of the collection, then thumbnail it and offer it for download.
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::Prefix(_)))
    {
        // Worth recording rather than dropping quietly: on Windows a drive
        // letter in this field parses as a Prefix component, so if a whole
        // catalogue lost its pictures there, this is where it happened.
        crate::diagnostics::error_repeating(
            "image-reference-rejected",
            "index",
            format!("refusing the image reference {raw:?} -- it points outside the Images folder"),
        );
        return None;
    }

    let full = images_dir.join(rel);
    if full.is_file() {
        return Some(full);
    }

    // PastPerfect records some filenames with a trailing dot and no
    // extension: "008\2012231." for a file that is actually named
    // "2012231". Windows, where these records were written, silently strips
    // trailing dots, so the reference resolves there and the picture appears.
    // Every other platform takes the dot literally and finds nothing, so the
    // image looks missing when it is sitting right beside its neighbours.
    // Tried only after the exact name fails, so a file genuinely named with a
    // trailing dot still wins.
    if let Some(trimmed) = trailing_dot_variant(&full) {
        if trimmed.is_file() {
            return Some(trimmed);
        }
    }

    {
        crate::diagnostics::error_repeating(
            "image-file-missing",
            "index",
            format!("the record names an image {raw:?} but there is no file at {}", full.display()),
        );
        None
    }
}

/// One image belonging to an item, in display order.
struct ImageRow {
    caption: String,
    image_path: String,
    thumb_path: Option<String>,
    /// The high-resolution master, when one exists. The copy in the Images
    /// folder is PastPerfect's display copy and is often much smaller (850px
    /// against 2400-4800px), so this is what "download full resolution"
    /// should hand over.
    hires_path: Option<String>,
}

struct ItemRow {
    catalog: String,
    accessno: String,
    title: String,
    body: String,
    image_path: Option<String>,
    thumb_path: Option<String>,
    detail_json: String,
    images: Vec<ImageRow>,
}

/// Builds the ordered image list for one catalog record.
///
/// `primary` is the image named by the record's own IMAGEFILE field, which is
/// all the app used to know about. When MEDIA.DBF has entries for this record
/// they supersede it -- MEDIA is where the second and subsequent images live,
/// along with their captions and their links to the high-resolution masters.
fn images_for_record(object_id: &str, primary: Option<&Path>, ctx: &IngestContext) -> Vec<ImageRow> {
    let make = |path: PathBuf, caption: String, hires: Option<PathBuf>| ImageRow {
        caption,
        thumb_path: get_preview_jpeg(&path, &ctx.thumb_cache_dir, THUMB_MAX_DIM)
            .map(|p| p.to_string_lossy().to_string()),
        image_path: path.to_string_lossy().to_string(),
        hires_path: hires.map(|p| p.to_string_lossy().to_string()),
    };

    if let Some(entries) = ctx.media.get(&media_key(object_id)) {
        let rows: Vec<ImageRow> = entries
            .iter()
            .filter_map(|e| {
                let path = resolve_pastperfect_image(&e.imagefile, &ctx.images_dir)?;
                let hires = ctx.multimedia.lookup(&e.hires_ref);
                Some(make(path, e.caption.clone(), hires))
            })
            .collect();
        if !rows.is_empty() {
            return rows;
        }
        // Every file MEDIA names is missing from disk; fall through so the
        // record still shows its own IMAGEFILE if that one happens to exist.
    }

    primary
        .map(|p| vec![make(p.to_path_buf(), String::new(), None)])
        .unwrap_or_default()
}

fn index_catalog_table(
    data_dir: &Path,
    ctx: &IngestContext,
    spec: &CatalogSpec,
    progress: &mut ProgressFn,
    warnings: &mut Vec<String>,
) -> Vec<ItemRow> {
    progress(&format!("Reading {} records", spec.catalog), None, None);
    let Some(path) = find_file(data_dir, spec.filename) else {
        // Not every install has every catalogue, so a missing table is
        // normal and not worth warning about.
        return Vec::new();
    };
    let mut table = match DbfTable::open(&path) {
        Ok(t) => t,
        Err(e) => {
            // Present but unreadable is a real problem: without this the
            // whole catalogue would just silently vanish from the index.
            warnings.push(format!("{} could not be read ({e}) -- those records are missing from the index.", spec.filename));
            return Vec::new();
        }
    };
    if table.memo_unavailable() {
        warnings.push(format!(
            "{}: the memo file (.FPT) is missing or unreadable, so descriptions and notes are blank for {} records.",
            spec.filename, spec.catalog
        ));
    }

    let mut rows = Vec::new();
    for rec in table.records() {
        let title = spec
            .title_fields
            .iter()
            .map(|f| rec.get(*f).map(|s| s.trim()).unwrap_or(""))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if title.is_empty() {
            continue;
        }

        // OBJECTID is the full, unique per-item catalog number (e.g.
        // "2010.2.7"). ACCESSNO is only the accession *lot* it arrived in
        // and can be shared by many separate objects, so it's not a safe
        // stand-in for the real id.
        let mut object_id = rec.get("OBJECTID").map(|s| s.trim().to_string()).unwrap_or_default();
        if object_id.is_empty() {
            object_id = rec.get("ACCESSNO").map(|s| s.trim().to_string()).unwrap_or_default();
        }

        let body = spec
            .body_fields
            .iter()
            .filter_map(|f| rec.get(*f).map(|s| s.trim()).filter(|s| !s.is_empty()))
            .collect::<Vec<_>>()
            .join("\n");

        let image_path = rec
            .get("IMAGEFILE")
            .and_then(|v| resolve_pastperfect_image(v, &ctx.images_dir));
        let thumb_path = image_path
            .as_ref()
            .and_then(|p| get_preview_jpeg(p, &ctx.thumb_cache_dir, THUMB_MAX_DIM));

        let images = images_for_record(&object_id, image_path.as_deref(), ctx);

        let mut detail = serde_json::Map::new();
        for f in spec.detail_fields {
            if let Some(v) = rec.get(*f) {
                if !v.trim().is_empty() {
                    detail.insert((*f).to_string(), json!(v));
                }
            }
        }

        rows.push(ItemRow {
            catalog: spec.catalog.to_string(),
            accessno: object_id,
            title,
            body,
            image_path: image_path.map(|p| p.to_string_lossy().to_string()),
            thumb_path: thumb_path.map(|p| p.to_string_lossy().to_string()),
            detail_json: serde_json::Value::Object(detail).to_string(),
            images,
        });
    }
    rows
}

/// Indexes a standalone folder of photos that is NOT part of PastPerfect.
/// These are indexed purely by filename/folder text -- deliberately no
/// accession number is assigned, since any resemblance to a PastPerfect id
/// in a filename is not a verified link.
fn index_photo_archive_root(
    root: &PhotoArchiveRoot,
    thumb_cache_dir: &Path,
    progress: &mut ProgressFn,
    warnings: &mut Vec<String>,
) -> Vec<ItemRow> {
    if !root.path.is_dir() {
        // Not fatal -- the catalogue is what matters -- but the photographs
        // from this folder will simply be absent from the index, and without
        // saying so the only symptom is that they stop turning up in searches.
        warnings.push(format!(
            "The photo folder {:?} ({}) could not be read, so none of its pictures are in \
             the index.",
            root.label,
            root.path.display()
        ));
        return Vec::new();
    }

    progress(&format!("Scanning {}…", root.label), None, None);
    let mut all_files: Vec<(PathBuf, String, Option<String>)> = Vec::new(); // (dirpath, filename, category)
    collect_image_files(&root.path, &root.path, &mut all_files);
    let total = all_files.len() as u64;

    let mut rows = Vec::with_capacity(all_files.len());
    for (i, (dirpath, fname, category)) in all_files.into_iter().enumerate() {
        let n = (i + 1) as u64;
        if n.is_multiple_of(10) || n == total {
            progress(&root.label, Some(n), Some(total));
        }
        let full_path = dirpath.join(&fname);
        let name_no_ext = Path::new(&fname).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| fname.clone());
        let readable_name: String =
            name_no_ext.chars().map(|c| if c == '_' || c == '-' { ' ' } else { c }).collect();
        let readable_name = readable_name.trim().to_string();
        let title = if readable_name.is_empty() { fname.clone() } else { readable_name };

        let mut body_parts = vec![root.label.clone()];
        if let Some(c) = &category {
            body_parts.push(c.clone());
        }
        let body = body_parts.join("\n");

        let thumb_path = get_preview_jpeg(&full_path, thumb_cache_dir, THUMB_MAX_DIM);

        let mut detail = serde_json::Map::new();
        detail.insert("FOLDER".to_string(), json!(root.label));
        if let Some(c) = &category {
            if !c.is_empty() {
                detail.insert("CATEGORY".to_string(), json!(c));
            }
        }
        detail.insert("FILENAME".to_string(), json!(fname));

        let thumb_path = thumb_path.map(|p| p.to_string_lossy().to_string());
        rows.push(ItemRow {
            catalog: root.label.clone(),
            accessno: String::new(),
            title,
            body,
            image_path: Some(full_path.to_string_lossy().to_string()),
            thumb_path: thumb_path.clone(),
            detail_json: serde_json::Value::Object(detail).to_string(),
            // A standalone photo is one file with no catalog record behind
            // it, so there is no caption and no high-resolution master --
            // but it still goes through the same list, so the detail card
            // has exactly one shape to render.
            images: vec![ImageRow {
                caption: String::new(),
                image_path: full_path.to_string_lossy().to_string(),
                thumb_path,
                hires_path: None,
            }],
        });
    }
    rows
}

/// Folder Settings lets the user point at any folder, and a symlink (or
/// Windows junction) pointing back at an ancestor would otherwise recurse
/// until the stack overflows -- which aborts the whole app. Real photo
/// folders here nest ~3 deep; 32 is far beyond any legitimate layout.
const MAX_WALK_DEPTH: usize = 32;

fn collect_image_files(dir: &Path, root: &Path, out: &mut Vec<(PathBuf, String, Option<String>)>) {
    collect_image_files_depth(dir, root, out, 0)
}

fn collect_image_files_depth(
    dir: &Path,
    root: &Path,
    out: &mut Vec<(PathBuf, String, Option<String>)>,
    depth: usize,
) {
    if depth > MAX_WALK_DEPTH {
        crate::diagnostics::error_repeating(
            "folder-too-deep",
            "index",
            format!("stopped descending at {} -- more than {MAX_WALK_DEPTH} levels deep", dir.display()),
        );
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            // Previously skipped in silence, which presented as a folder
            // simply containing no pictures. On Windows this is where a
            // permissions problem or a disconnected share shows up.
            crate::diagnostics::error_repeating(
                "folder-unreadable",
                "index",
                format!("could not read the folder {}: {e}", dir.display()),
            );
            return;
        }
    };
    // Sorted so a rebuild over unchanged data always produces an identical
    // index; raw read_dir order is arbitrary and varies between machines.
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_image_files_depth(&path, root, out, depth + 1);
        } else if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
            let ext = Path::new(fname).extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase());
            if ext.as_deref().is_some_and(|e| IMAGE_EXTENSIONS.contains(&e)) {
                let rel_dir = dir.strip_prefix(root).unwrap_or(Path::new(""));
                let category = rel_dir
                    .components()
                    .next()
                    .map(|c| c.as_os_str().to_string_lossy().to_string());
                out.push((dir.to_path_buf(), fname.to_string(), category));
            }
        }
    }
}

pub struct BuildResult {
    pub total_records: u64,
    /// Non-fatal problems worth telling the user about -- an index that
    /// looks complete but is quietly missing descriptions or a whole
    /// catalogue is worse than one that says so.
    pub warnings: Vec<String>,
}

/// Moves the finished index over the live one, retrying briefly.
///
/// On Unix, renaming over a file that something else has open just works: the
/// reader keeps the old file and the new name takes effect at once. Windows
/// does not behave that way. SQLite opens database files without
/// FILE_SHARE_DELETE, so replacing one while any handle is open fails
/// outright with a sharing violation.
///
/// That matters here because the window is not theoretical. While a rebuild
/// runs, the interface polls for progress every 700ms and each poll opens the
/// index to count records, and typing in the search box opens it again. So on
/// Windows a rebuild could fail at the very last step -- after all the work --
/// purely because a poll happened to overlap. Readers hold the file for only
/// milliseconds, so waiting briefly and trying again clears it.
fn replace_index_file(from: &Path, to: &Path) -> std::io::Result<()> {
    const ATTEMPTS: u32 = 25;
    const PAUSE: std::time::Duration = std::time::Duration::from_millis(80);

    let mut last_error = None;
    for attempt in 1..=ATTEMPTS {
        match fs::rename(from, to) {
            Ok(()) => {
                if attempt > 1 {
                    crate::diagnostics::info(
                        "index",
                        format!(
                            "the index was in use; the swap succeeded on attempt {attempt} \
                             (this is expected on Windows and harmless)"
                        ),
                    );
                }
                return Ok(());
            }
            Err(e) => {
                last_error = Some(e);
                if attempt < ATTEMPTS {
                    std::thread::sleep(PAUSE);
                }
            }
        }
    }
    // ATTEMPTS is non-zero so this is always Some, but this runs on a
    // background thread in a build that aborts on panic -- an unwrap here
    // would turn a failed refresh into a dead application.
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::other("the index could not be replaced, and no reason was recorded")
    }))
}

/// Builds the full index into a temp file, then atomically swaps it into
/// place at `db_path` -- so anything reading db_path concurrently only ever
/// sees a fully-formed database (the old one, or the finished new one),
/// never a half-built one with no tables in it yet.
pub fn build(
    data_dir: &Path,
    db_path: &Path,
    images_dir: &Path,
    multimedia_dir: &Path,
    photo_archive_roots: &[PhotoArchiveRoot],
    thumb_cache_dir: &Path,
    progress: Option<&mut ProgressFn>,
) -> rusqlite::Result<BuildResult> {
    let mut fallback = noop_progress;
    let progress: &mut ProgressFn = match progress {
        Some(p) => p,
        None => &mut fallback,
    };

    let started = std::time::Instant::now();
    crate::diagnostics::reset_repeat_counts();
    crate::diagnostics::info(
        "index",
        format!(
            "rebuild starting\n    data:       {}\n    images:     {}\n    multimedia: {}\n    \
             photo folders: {}",
            data_dir.display(),
            images_dir.display(),
            multimedia_dir.display(),
            if photo_archive_roots.is_empty() {
                "(none)".to_string()
            } else {
                photo_archive_roots
                    .iter()
                    .map(|r| format!("{:?} at {}", r.label, r.path.display()))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ),
    );

    // Unique per process and per call: two copies of the application
    // rebuilding at once would otherwise write to the same file and then
    // rename the result over the live index.
    let build_path =
        db_path.with_extension(format!("db.building.{}", crate::app_paths::unique_temp_suffix()));
    if build_path.exists() {
        let _ = fs::remove_file(&build_path);
    }

    let mut conn = Connection::open(&build_path)?;
    conn.execute_batch(
        "CREATE TABLE items (
            id INTEGER PRIMARY KEY,
            catalog TEXT,
            accessno TEXT,
            title TEXT,
            body TEXT,
            image_path TEXT,
            thumb_path TEXT,
            detail_json TEXT,
            -- The catalog number with its separators removed. Indexed so that
            -- a run-together 20101918 finds 2010.19.18: FTS splits the stored
            -- number into the tokens 2010/19/18, which such a typing never
            -- matches. Kept as a real column because items_fts reads its
            -- content from here.
            accessno_norm TEXT
        );
        CREATE VIRTUAL TABLE items_fts USING fts5(
            title, body, accessno, accessno_norm,
            content='items', content_rowid='id'
        );
        CREATE TABLE item_images (
            id INTEGER PRIMARY KEY,
            item_id INTEGER NOT NULL,
            ordinal INTEGER NOT NULL,
            caption TEXT,
            image_path TEXT NOT NULL,
            thumb_path TEXT,
            hires_path TEXT
        );
        CREATE INDEX item_images_by_item ON item_images(item_id, ordinal);",
    )?;

    let mut total: u64 = 0;
    let mut warnings: Vec<String> = Vec::new();
    {
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO items (catalog, accessno, title, body, image_path, thumb_path, detail_json, accessno_norm)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            let mut image_stmt = tx.prepare(
                "INSERT INTO item_images (item_id, ordinal, caption, image_path, thumb_path, hires_path)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            let mut insert_all = |rows: Vec<ItemRow>| -> rusqlite::Result<()> {
                for r in rows {
                    stmt.execute(rusqlite::params![
                        r.catalog, r.accessno, r.title, r.body, r.image_path, r.thumb_path,
                        r.detail_json,
                        // Same normalisation the search side compares against,
                        // so the two cannot drift apart.
                        crate::store::normalize_catalog_number(&r.accessno)
                    ])?;
                    let item_id = tx.last_insert_rowid();
                    // Ordinal is the position shown to the user ("Image 2 of
                    // 4"), so it counts 1..n over the images actually found
                    // rather than echoing MEDIA's POSITION column, which can
                    // have gaps where a file has gone missing.
                    for (i, img) in r.images.iter().enumerate() {
                        image_stmt.execute(rusqlite::params![
                            item_id,
                            (i + 1) as i64,
                            img.caption,
                            img.image_path,
                            img.thumb_path,
                            img.hires_path
                        ])?;
                    }
                    total += 1;
                }
                Ok(())
            };

            progress("Reading image links", None, None);
            let ctx = IngestContext {
                images_dir: images_dir.to_path_buf(),
                thumb_cache_dir: thumb_cache_dir.to_path_buf(),
                media: load_media_index(data_dir, &mut warnings),
                multimedia: MultimediaIndex::build(multimedia_dir),
            };
            crate::diagnostics::info(
                "index",
                format!(
                    "MEDIA.DBF gave image links for {} records; the Multimedia folder holds {} files",
                    ctx.media.len(),
                    ctx.multimedia.len()
                ),
            );

            for spec in CATALOG_TABLES {
                let rows = index_catalog_table(data_dir, &ctx, spec, progress, &mut warnings);
                insert_all(rows)?;
            }
            let people_rows = index_catalog_table(data_dir, &ctx, &PEOPLE_TABLE, progress, &mut warnings);
            insert_all(people_rows)?;

            for root in photo_archive_roots {
                let rows = index_photo_archive_root(root, thumb_cache_dir, progress, &mut warnings);
                insert_all(rows)?;
            }
        }

        progress("Finishing up…", None, None);
        tx.execute(
            "INSERT INTO items_fts (rowid, title, body, accessno, accessno_norm)
             SELECT id, title, body, accessno, accessno_norm FROM items",
            [],
        )?;
        tx.commit()?;
    }

    conn.close().map_err(|(_, e)| e)?;
    // Report a failed swap as an error rather than panicking: this runs on a
    // background thread and the app is built with `panic = "abort"`, so a
    // panic here (disk full, permissions, file locked) would kill the app
    // instead of just failing the refresh.
    replace_index_file(&build_path, db_path).map_err(|e| {
        // The most consequential failure of the whole rebuild: everything
        // worked and then the finished index could not be put in place, so
        // the app silently carries on with the old one.
        crate::diagnostics::error(
            "index",
            format!(
                "the finished index could not replace the old one at {}: {e} \
                 (antivirus, a locked file, or a full disk) -- the previous index is \
                 still in use",
                db_path.display()
            ),
        );
        let _ = fs::remove_file(&build_path);
        rusqlite::Error::ToSqlConversionFailure(Box::new(e))
    })?;

    crate::diagnostics::info(
        "index",
        format!("rebuild finished: {total} records in {:?}", started.elapsed()),
    );
    for w in &warnings {
        crate::diagnostics::warn("index", w);
    }
    for (key, count) in crate::diagnostics::repeat_totals() {
        crate::diagnostics::info("index", format!("during this rebuild, '{key}' happened {count} times"));
    }

    Ok(BuildResult { total_records: total, warnings })
}

/// Whether the configured folders justify replacing the index that already
/// exists.
///
/// Rebuilding is destructive: the finished index replaces the previous one,
/// and there is no way back except fixing the settings and rebuilding again.
/// It is worth being sure first.
///
/// The check that used to be here only asked whether the Data folder existed,
/// which is not the same as whether it holds a catalogue. Choosing one level
/// too high in the folder browser -- `pp5` rather than `pp5\Data`, an easy
/// slip -- passes that test and produces an index containing no catalogue
/// records at all. Measured on a real collection: thousands of objects, photographs,
/// archives, library items and people replaced by standalone image files,
/// with the rebuild reporting success. The only visible symptom would be that
/// searches stopped finding anything with a catalog number.
///
/// Folder Settings already says "no PastPerfect tables found in it" while
/// typing, but nothing stopped Save & Rebuild going ahead anyway.
pub fn rebuild_readiness(
    data_dir_configured: &str,
    data_dir: &Path,
    photo_roots: &[PhotoArchiveRoot],
) -> Result<(), String> {
    let has_data_setting = !data_dir_configured.trim().is_empty();
    let readable_photo_roots = photo_roots.iter().filter(|r| r.path.is_dir()).count();

    if has_data_setting {
        if !data_dir.is_dir() {
            return Err(format!(
                "The PastPerfect Data folder is set to {}, which is not there any more. \
                 Rebuilding now would drop every catalogued record, so the existing index \
                 has been left alone. Open Folder Settings and correct it -- or clear it, \
                 if you no longer want the catalogue searched.",
                data_dir.display()
            ));
        }
        if catalog_labels_present(data_dir).is_empty() {
            return Err(format!(
                "No PastPerfect tables (OBJECTS.DBF and the rest) are in {}. That is usually \
                 one folder too high -- the tables live in the \"Data\" folder inside it. \
                 Rebuilding from here would replace every catalogued record with nothing, \
                 so the existing index has been left alone.",
                data_dir.display()
            ));
        }
        return Ok(());
    }

    if readable_photo_roots == 0 {
        return Err("No readable folders are configured. Open Folder Settings and \
                    choose your PastPerfect Data folder (and any photo folders) \
                    first -- the existing index has been left alone."
            .to_string());
    }
    Ok(())
}

/// Which PastPerfect catalog tables are actually present in `dir` (used by
/// Folder Settings to tell the user whether they picked the right folder).
pub fn catalog_labels_present(dir: &Path) -> Vec<&'static str> {
    CATALOG_TABLES
        .iter()
        .chain(std::iter::once(&PEOPLE_TABLE))
        .filter(|spec| find_file(dir, spec.filename).is_some())
        .map(|spec| spec.catalog)
        .collect()
}

/// The longest file path recorded in the index, and an example of it.
///
/// Windows refuses paths over 260 characters unless long-path support has
/// been turned on, and it does so with errors that read like the file is
/// simply missing. Deep Multimedia folders under a long user name get closer
/// to that limit than expected, so the number is worth having in hand.
pub fn longest_indexed_path() -> (usize, String) {
    // Opened read-only on purpose. `Connection::open` *creates* the database
    // when it is absent, and this runs while building a diagnostic report --
    // so on a machine where nothing has been indexed yet, asking for that
    // report left behind an empty index with no tables in it. The application
    // then believed it had one: the status line reported "0 records" instead
    // of "not built yet", and the warning about an index predating per-image
    // rows appeared on a machine that had never built an index at all.
    let Some(conn) = rusqlite::Connection::open_with_flags(
        crate::app_paths::db_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok() else {
        return (0, "(no index yet)".into());
    };
    let mut longest = (0usize, String::from("(none recorded)"));
    for sql in [
        "SELECT image_path FROM items WHERE image_path IS NOT NULL",
        "SELECT image_path FROM item_images",
        "SELECT hires_path FROM item_images WHERE hires_path IS NOT NULL",
    ] {
        let Ok(mut stmt) = conn.prepare(sql) else { continue };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else { continue };
        for path in rows.flatten() {
            if path.chars().count() > longest.0 {
                longest = (path.chars().count(), path);
            }
        }
    }
    longest
}

/// Counts image files under `dir` (recursively), for the same purpose.
pub fn count_image_files(dir: &Path) -> usize {
    let mut found = Vec::new();
    collect_image_files(dir, dir, &mut found);
    found.len()
}

/// How many high-resolution masters a candidate Multimedia folder holds, so
/// Folder Settings can confirm the user picked the right one.
pub fn count_multimedia_masters(dir: &Path) -> usize {
    MultimediaIndex::build(dir).len()
}

/// The Multimedia folder to use given the configured value, defaulting to the
/// folder named "Multimedia" alongside the Data folder -- the standard
/// PastPerfect layout, so the masters are found without configuring anything.
pub fn default_multimedia_dir(configured: &str, data_dir: &Path) -> PathBuf {
    if !configured.trim().is_empty() {
        return PathBuf::from(configured.trim());
    }
    // Nothing to sit it beside when no Data folder is set: returning
    // "Multimedia" on its own would name a folder relative to wherever the
    // application was started, which is not a place anyone chose.
    if data_dir.as_os_str().is_empty() {
        return PathBuf::new();
    }
    data_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("Multimedia"))
        .unwrap_or_else(|| data_dir.join("Multimedia"))
}

/// The order detail fields should be presented in, for a given catalogue.
/// Presentation order is a deliberate editorial choice (object name first,
/// then id, date, description...), so it must not be left to however the
/// stored JSON happens to be keyed -- serde_json sorts object keys, which
/// otherwise surfaces "Storage Location" above "Object".
pub fn detail_field_order(catalog: &str) -> &'static [&'static str] {
    CATALOG_TABLES
        .iter()
        .chain(std::iter::once(&PEOPLE_TABLE))
        .find(|spec| spec.catalog == catalog)
        .map(|spec| spec.detail_fields)
        .unwrap_or(&[])
}

pub fn field_labels() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("OBJNAME", "Object"), ("OBJNAME2", "Object (alt)"), ("OBJNAME3", "Object (alt)"),
        ("TITLE", "Title"), ("TITLEX", "Title"), ("ACCESSNO", "Accession Lot"),
        ("DATE", "Date"), ("DESCRIP", "Description"), ("CAPTION", "Caption"),
        ("CREATOR", "Creator/Maker"), ("CREATOR2", "Creator/Maker"),
        ("CREATOR3", "Creator/Maker"), ("AUTHORX", "Author"),
        ("MATERIAL", "Materials"), ("MEDIUM", "Medium"), ("SUBJECTS", "Subjects"),
        ("PEOPLE", "People"), ("SITE", "Site"), ("PLACE", "Place"), ("EVENT", "Event"),
        ("COLLECTION", "Collection"), ("PROVENANCE", "Provenance"),
        ("CONDITION", "Condition"), ("HOMELOC", "Storage Location"),
        ("NOTES", "Notes"), ("SERIES", "Series"), ("NARRATOR", "Narrator"),
        ("PUBLISHER", "Publisher"), ("NAME", "Name"), ("OTHERNAME", "Other Name"),
        ("BORN", "Born"), ("DIED", "Died"), ("BIRTHPLACE", "Birthplace"),
        ("LASTPLACE", "Last Residence"), ("OCCUPATION", "Occupation"),
        ("NATIONAL", "Nationality"), ("ROLE", "Role"), ("FATHER", "Father"),
        ("MOTHER", "Mother"), ("FOLDER", "Folder"), ("CATEGORY", "Category"),
        ("FILENAME", "Original Filename"),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "live-data")]
    use std::process::Command;

    // Same real data + same real thumbnail cache used throughout this
    // session's prototype (so this runs against warm cache hits, not a
    // ~15-20 minute cold regeneration). Deliberately not asserting specific
    // record counts anywhere below, for the same reason as dbf.rs's tests:
    // this is the historical society's live, actively-growing database.

    #[cfg(feature = "live-data")]
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hs_ingesttest_{name}_{}.db", std::process::id()))
    }

    /// Collapses whitespace runs so comparisons are about words, not layout.
    #[cfg(feature = "live-data")]
    fn normalize_ws(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[cfg(feature = "live-data")]
    #[derive(serde::Deserialize, Debug)]
    struct DumpedRow {
        catalog: String,
        accessno: String,
        title: String,
        body: String,
        has_image: i64,
        has_thumb: i64,
        detail_json: String,
    }

    #[cfg(feature = "live-data")]
    fn run_python_reference() -> Vec<DumpedRow> {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/dump_ingest.py");
        let prototype_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../prototype");
        let out_db = temp_path("python_ref");
        let _ = fs::remove_file(&out_db);

        let photo_roots_json = json!(crate::test_support::photo_roots()
            .into_iter()
            .map(|(label, path)| json!({"label": label, "path": path.to_string_lossy()}))
            .collect::<Vec<_>>())
        .to_string();

        let output = Command::new("python3")
            .arg(&script)
            .arg(&prototype_dir)
            .arg(crate::test_support::data_dir())
            .arg(crate::test_support::images_dir())
            .arg(crate::test_support::thumb_cache_dir())
            .arg(&photo_roots_json)
            .arg(&out_db)
            .output()
            .expect("failed to run python3 reference ingest");
        assert!(
            output.status.success(),
            "python reference ingest failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_file(&out_db);
        serde_json::from_slice(&output.stdout).expect("invalid JSON from python reference ingest")
    }

    /// Builds a real index into a temp file and hands back an open connection,
    /// so tests can inspect whatever they need rather than only the row dump.
    #[cfg(feature = "live-data")]
    fn build_test_index(name: &str) -> (PathBuf, Connection) {
        let out_db = temp_path(name);
        let _ = fs::remove_file(&out_db);

        let roots: Vec<PhotoArchiveRoot> = crate::test_support::photo_roots()
            .into_iter()
            .map(|(label, path)| PhotoArchiveRoot { label, path })
            .collect();
        build(
            &crate::test_support::data_dir(),
            &out_db,
            &crate::test_support::images_dir(),
            &crate::test_support::multimedia_dir(),
            &roots,
            &crate::test_support::thumb_cache_dir(),
            None,
        )
        .expect("rust build failed");
        let conn = Connection::open(&out_db).unwrap();
        (out_db, conn)
    }

    #[cfg(feature = "live-data")]
    fn run_rust_build() -> Vec<DumpedRow> {
        let (out_db, conn) = build_test_index("rust_build");
        let mut stmt = conn
            .prepare(
                "SELECT catalog, accessno, title, body, image_path IS NOT NULL AS has_image, \
                 thumb_path IS NOT NULL AS has_thumb, detail_json FROM items ORDER BY id",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok(DumpedRow {
                    catalog: row.get(0)?,
                    accessno: row.get(1)?,
                    title: row.get(2)?,
                    body: row.get(3)?,
                    has_image: row.get(4)?,
                    has_thumb: row.get(5)?,
                    detail_json: row.get(6)?,
                })
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        drop(stmt);
        conn.close().unwrap();
        let _ = fs::remove_file(&out_db);
        rows
    }

    #[test]
    fn image_references_cannot_escape_the_images_folder() {
        // A real file outside the images folder, created here rather than
        // borrowed from the operating system: this test has to mean the same
        // thing on Windows, where "/etc/hosts" does not exist and the escape
        // would appear to be blocked when nothing had blocked it.
        let outside = std::env::temp_dir().join(format!("hs_escape_target_{}.txt", std::process::id()));
        fs::write(&outside, b"secret").expect("could not create the file to try to escape to");
        let images_dir = std::env::temp_dir().join(format!("hs_escape_images_{}", std::process::id()));
        let nested = images_dir.join("a").join("b");
        fs::create_dir_all(&nested).expect("could not create the images folder");

        // Normal relative references are fine, they simply do not exist here.
        assert!(
            resolve_pastperfect_image("005\\file.jpg", &images_dir).is_none(),
            "this file doesn't exist, so it should resolve to None -- but not by escaping"
        );

        // Prove the escape would otherwise work: from two levels down, "../.."
        // plus the filename really does reach the file outside.
        let file_name = outside.file_name().unwrap().to_string_lossy().to_string();
        let reachable = nested.join("..").join("..").join("..").join(&file_name);
        assert!(
            reachable.is_file(),
            "the test's own escape path does not reach the target, so it would prove nothing"
        );

        // Now the same references must all be refused.
        for escape in [
            outside.to_string_lossy().to_string(),
            format!("a\\b\\..\\..\\..\\{file_name}"),
            format!("a/b/../../../{file_name}"),
        ] {
            assert!(
                resolve_pastperfect_image(&escape, &images_dir).is_none(),
                "{escape:?} escaped the images folder"
            );
        }

        let _ = fs::remove_file(&outside);
        let _ = fs::remove_dir_all(&images_dir);
    }

    /// The finished index must replace the live one even while something is
    /// reading it -- which happens constantly during a rebuild, because the
    /// progress display polls the record count every 700ms.
    ///
    /// Deliberately in the set that runs everywhere, not the one needing the
    /// real collection: the behaviour this guards against is specific to
    /// Windows, where replacing an open file is refused outright, and this
    /// machine cannot demonstrate it. Running it on a Windows build server is
    /// the only way to know.
    /// Keeps a scratch pair of real SQLite files, and hammers the "live" one
    /// with short-lived readers exactly as the application does while a
    /// rebuild runs (the progress display opens the index every 700ms, and
    /// typing in the search box opens it again).
    struct SwapHarness {
        dir: PathBuf,
        live: PathBuf,
        built: PathBuf,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        reader: Option<std::thread::JoinHandle<u32>>,
    }

    impl SwapHarness {
        fn start(tag: &str) -> Self {
            use std::sync::atomic::AtomicBool;
            use std::sync::Arc;

            let dir = std::env::temp_dir()
                .join(format!("hs_swaptest_{tag}_{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("could not create the scratch folder");
            let live = dir.join("search_index.db");
            let built = dir.join("search_index.db.building");
            for path in [&live, &built] {
                let conn = Connection::open(path).expect("could not create a test database");
                conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY, title TEXT);")
                    .unwrap();
                conn.close().unwrap();
            }

            let stop = Arc::new(AtomicBool::new(false));
            let reader_stop = Arc::clone(&stop);
            let reader_path = live.clone();
            let reader = std::thread::spawn(move || {
                let mut opened = 0u32;
                while !reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(conn) = Connection::open(&reader_path) {
                        let _: Result<i64, _> =
                            conn.query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0));
                        let _ = conn.close();
                        opened += 1;
                    }
                }
                opened
            });
            // Let the readers actually get hold of the file.
            std::thread::sleep(std::time::Duration::from_millis(200));
            Self { dir, live, built, stop, reader: Some(reader) }
        }

        fn finish(&mut self) -> u32 {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            self.reader.take().map(|r| r.join().expect("reader thread panicked")).unwrap_or(0)
        }
    }

    impl Drop for SwapHarness {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(r) = self.reader.take() {
                let _ = r.join();
            }
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// The finished index must replace the live one even while something is
    /// reading it.
    ///
    /// Deliberately in the set that runs everywhere rather than the one
    /// needing the real collection: what this guards against is a property of
    /// the Windows filesystem, not of any particular data, so a build server
    /// running Windows can demonstrate it and this machine cannot.
    ///
    /// The first half measures whether a plain rename fails under these
    /// conditions, and reports it without asserting -- on Unix it will
    /// succeed, and on Windows it is expected to fail. Only the second half,
    /// the retrying version, is required to succeed. Without that first half
    /// this test could pass on Windows purely because the timing was lucky,
    /// and would then be proving nothing at all.
    #[test]
    fn the_finished_index_replaces_the_old_one_while_it_is_being_read() {
        // Half one: what does an unprotected rename do here?
        let mut plain = SwapHarness::start("plain");
        let plain_result = fs::rename(&plain.built, &plain.live);
        let plain_opens = plain.finish();
        assert!(
            plain_opens > 0,
            "the reader thread never opened the index, so this test proved nothing"
        );
        println!(
            "plain rename over a database being read: {} (after {plain_opens} reader opens) on {}",
            match &plain_result {
                Ok(()) => "SUCCEEDED (this platform allows it)".to_string(),
                Err(e) => format!("FAILED -- {e}"),
            },
            std::env::consts::OS
        );

        // Half two: the version the application actually uses must succeed
        // whatever the platform does.
        let mut guarded = SwapHarness::start("guarded");
        let result = replace_index_file(&guarded.built, &guarded.live);
        let opens = guarded.finish();
        assert!(opens > 0, "the reader thread never opened the index");
        assert!(
            result.is_ok(),
            "the finished index could not replace one that was being read: {:?} \
             (after {opens} reader opens). This is what a rebuild failing at the \
             very last step looks like.",
            result.err()
        );
        assert!(guarded.live.is_file(), "the live index is missing after the swap");
        assert!(!guarded.built.is_file(), "the temporary build file was left behind");
    }

    /// PastPerfect records some image references with a trailing dot and no
    /// extension. Windows strips trailing dots, so those resolve on the
    /// machine the records were written on; everywhere else the picture looks
    /// missing even though the file is present.
    #[test]
    fn an_image_recorded_with_a_trailing_dot_still_resolves() {
        let images = std::env::temp_dir().join(format!("hs_dottest_{}", std::process::id()));
        let _ = fs::remove_dir_all(&images);
        fs::create_dir_all(images.join("008")).expect("could not create the test folder");
        // Named without any extension, exactly as these arrive in the
        // collection; the reference in the database carries a trailing dot.
        fs::write(images.join("008").join("2012231"), b"not really a jpeg").unwrap();

        // Compared by what the path opens, not by how it is spelled. Windows
        // strips trailing dots itself, so there the original reference already
        // names the file and is returned unchanged; everywhere else the
        // trimmed form is returned. Both are correct, and asserting one exact
        // string only passed here by accident of platform.
        let resolved = resolve_pastperfect_image(r"008\2012231.", &images)
            .expect("a reference with a trailing dot should find the file beside it");
        assert!(resolved.is_file(), "{} does not exist", resolved.display());
        assert_eq!(
            fs::read_to_string(&resolved).unwrap(),
            "not really a jpeg",
            "resolved to some other file"
        );

        // A file genuinely named with a trailing dot must win over the trimmed
        // one. Written trimmed-first so the assertion holds on both kinds of
        // filesystem: where trailing dots are significant these are two files
        // and the exact one is preferred, and where they are not (Windows)
        // they are one file whose final contents are the same either way.
        fs::write(images.join("008").join("both"), b"trimmed-name-file").unwrap();
        fs::write(images.join("008").join("both."), b"exact-name-file").unwrap();
        let exact = resolve_pastperfect_image(r"008\both.", &images).expect("should resolve");
        assert_eq!(
            fs::read_to_string(&exact).unwrap(),
            "exact-name-file",
            "a reference ending in a dot resolved to the wrong file"
        );

        // Nothing invented when neither exists.
        assert!(resolve_pastperfect_image(r"008\missing.", &images).is_none());
        let _ = fs::remove_dir_all(&images);
    }

    #[test]
    fn trailing_dots_are_only_trimmed_when_there_are_any() {
        assert_eq!(trailing_dot_variant(Path::new("/a/b/name.")), Some(PathBuf::from("/a/b/name")));
        assert_eq!(trailing_dot_variant(Path::new("/a/b/name...")), Some(PathBuf::from("/a/b/name")));
        assert_eq!(trailing_dot_variant(Path::new("/a/b/name.jpg")), None);
        assert_eq!(trailing_dot_variant(Path::new("/a/b/name")), None);
        // All dots would leave nothing to look for.
        assert_eq!(trailing_dot_variant(Path::new("/a/b/...")), None);
    }

    /// Rebuilding replaces the existing index, so the folders have to be
    /// checked properly first. Uses folders made here rather than the real
    /// collection, so it runs everywhere -- including on Windows in CI, which
    /// is where a mistyped or moved folder is most likely to happen.
    #[test]
    fn a_rebuild_is_refused_when_it_would_throw_the_catalogue_away() {
        let base = std::env::temp_dir().join(format!("hs_ready_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let real_data = base.join("pp5/Data");
        let parent = base.join("pp5");
        let photos = base.join("photos/Scans");
        fs::create_dir_all(&real_data).unwrap();
        fs::create_dir_all(&photos).unwrap();
        // Enough of a table for find_file to see it; readiness only asks
        // which tables are present, not what is in them.
        fs::write(real_data.join("OBJECTS.DBF"), b"").unwrap();
        let root = |p: &Path| vec![PhotoArchiveRoot { label: "Scans".into(), path: p.to_path_buf() }];

        // The mistake this exists for: one folder too high. It is a real
        // directory, so the old check waved it through and every catalogued
        // record was replaced with nothing.
        let refused = rebuild_readiness(&parent.to_string_lossy(), &parent, &root(&photos))
            .expect_err("choosing the folder above Data must be refused");
        assert!(
            refused.contains("one folder too high"),
            "the refusal should explain the likely mistake, got: {refused}"
        );

        // A Data folder that has been moved or renamed since.
        let gone = base.join("pp5/NotThere");
        let missing = rebuild_readiness(&gone.to_string_lossy(), &gone, &root(&photos))
            .expect_err("a Data folder that is not there must be refused");
        assert!(missing.contains("not there any more"), "got: {missing}");

        // Nothing configured at all.
        let nothing = base.join("photos/Gone");
        rebuild_readiness("", Path::new(""), &root(&nothing))
            .expect_err("no readable folders at all must be refused");

        // And the arrangements that must still be allowed through.
        rebuild_readiness(&real_data.to_string_lossy(), &real_data, &root(&photos))
            .expect("a correct Data folder must be accepted");
        rebuild_readiness(&real_data.to_string_lossy(), &real_data, &[])
            .expect("a catalogue with no photo folders must be accepted");
        rebuild_readiness("", Path::new(""), &root(&photos))
            .expect("photo folders alone, with no catalogue, must be accepted");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn windows_paths_in_dbf_fields_split_on_backslashes() {
        // std::path on macOS/Linux does not treat '\' as a separator, so
        // Path::file_name() on one of these returns the whole string and every
        // high-resolution master silently fails to resolve.
        assert_eq!(
            stored_path_basename(r"c:\pp5\multimedia\Largeimages1\200700201314-Large.tif"),
            Some("200700201314-Large.tif")
        );
        assert_eq!(stored_path_basename("/var/pp5/Multimedia/x-large.jpg"), Some("x-large.jpg"));
        assert_eq!(stored_path_basename("  bare-name.tif  "), Some("bare-name.tif"));
        assert_eq!(stored_path_basename(""), None);
        assert_eq!(stored_path_basename(r"c:\pp5\ends-in-separator\"), None);
    }

    #[test]
    fn a_master_whose_name_is_not_unique_resolves_to_nothing() {
        // Masters are found by filename because the path recorded in MEDIA.DBF
        // belongs to a different machine. If two files share a name there is
        // no way to tell which was meant, and handing over the wrong one is
        // worse than falling back to the display copy.
        let dir = std::env::temp_dir().join(format!("hs_mmtest_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a")).unwrap();
        fs::create_dir_all(dir.join("b")).unwrap();
        fs::write(dir.join("a/dup-large.tif"), b"x").unwrap();
        fs::write(dir.join("b/dup-large.tif"), b"y").unwrap();
        fs::write(dir.join("a/unique-large.tif"), b"z").unwrap();

        let index = MultimediaIndex::build(&dir);
        assert_eq!(
            index.lookup(r"c:\pp5\multimedia\whatever\dup-large.tif"),
            None,
            "an ambiguous filename must not resolve to an arbitrary copy"
        );
        assert_eq!(
            index.lookup(r"c:\pp5\multimedia\whatever\unique-large.tif"),
            Some(dir.join("a/unique-large.tif")),
            "an unambiguous filename must resolve regardless of the recorded folder"
        );
        // Case differences between the DBF value and the filesystem are normal.
        assert!(index.lookup("UNIQUE-LARGE.TIF").is_some(), "lookup must be case-insensitive");
        assert_eq!(index.lookup("not-here.tif"), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn records_with_several_images_get_all_of_them() {
        let (out_db, conn) = build_test_index("multi_image");

        // Every image row must point at a file that really exists -- these
        // paths are handed straight to the image protocol and to Save As.
        let mut stmt = conn.prepare("SELECT image_path FROM item_images").unwrap();
        let paths: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        drop(stmt);
        assert!(!paths.is_empty(), "no image rows were written at all");
        for p in &paths {
            assert!(Path::new(p).is_file(), "indexed image does not exist on disk: {p}");
        }

        // The gap this closed: records really do have more than one image, and
        // the catalog tables' own IMAGEFILE column only ever names the first.
        let multi: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (SELECT item_id FROM item_images GROUP BY item_id HAVING COUNT(*) > 1)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            multi > 0,
            "expected at least one record with several images; if MEDIA.DBF stopped being read \
             this would drop to zero and every extra image would silently vanish again"
        );

        // Ordinals must be a gapless 1..n per item, since the UI shows them
        // as "Image 2 of 4" and uses them to step through the strip.
        let mut stmt = conn
            .prepare("SELECT item_id, ordinal FROM item_images ORDER BY item_id, ordinal")
            .unwrap();
        let rows: Vec<(i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        drop(stmt);
        let mut expected_item = -1i64;
        let mut expected_ordinal = 1i64;
        for (item_id, ordinal) in rows {
            if item_id != expected_item {
                expected_item = item_id;
                expected_ordinal = 1;
            }
            assert_eq!(ordinal, expected_ordinal, "item {item_id} has a gap in its image ordinals");
            expected_ordinal += 1;
        }

        // Per-image captions are the other half of what MEDIA.DBF carries.
        let captioned: i64 = conn
            .query_row("SELECT COUNT(*) FROM item_images WHERE caption != ''", [], |r| r.get(0))
            .unwrap();
        assert!(captioned > 0, "expected some images to carry their own caption");

        conn.close().unwrap();
        let _ = fs::remove_file(&out_db);
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn the_primary_image_still_leads_and_masters_resolve() {
        let (out_db, conn) = build_test_index("hires");

        // Image 1 must be the same file the record's own IMAGEFILE names, so
        // the picture on the detail card matches the thumbnail in the results
        // list rather than jumping to some other view of the object.
        let mismatched: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM items i
                 JOIN item_images m ON m.item_id = i.id AND m.ordinal = 1
                 WHERE i.image_path IS NOT NULL AND i.image_path != m.image_path",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mismatched, 0, "the first image is not the record's own IMAGEFILE");

        // Every item that has an image at all must have an image row, or the
        // detail card would show nothing where the results list showed a
        // thumbnail.
        let orphaned: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM items i
                 WHERE i.image_path IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM item_images m WHERE m.item_id = i.id)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphaned, 0, "items with an image are missing their image rows");

        let with_master: i64 = conn
            .query_row("SELECT COUNT(*) FROM item_images WHERE hires_path IS NOT NULL", [], |r| r.get(0))
            .unwrap();
        assert!(
            with_master > 0,
            "no high-resolution masters resolved -- either the Multimedia folder moved or \
             the Windows-path handling broke, and 'full resolution' would be serving the \
             downsized display copy for everything"
        );

        // A master must exist and be a genuinely bigger file than the display
        // copy it replaces; otherwise "full resolution" is a lie.
        let mut stmt = conn
            .prepare(
                "SELECT image_path, hires_path FROM item_images
                 WHERE hires_path IS NOT NULL LIMIT 25",
            )
            .unwrap();
        let pairs: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        drop(stmt);
        for (display, master) in &pairs {
            assert!(Path::new(master).is_file(), "master does not exist: {master}");
            let display_size = fs::metadata(display).unwrap().len();
            let master_size = fs::metadata(master).unwrap().len();
            assert!(
                master_size > display_size,
                "master {master} ({master_size} bytes) is not larger than the display copy \
                 {display} ({display_size} bytes)"
            );
        }

        conn.close().unwrap();
        let _ = fs::remove_file(&out_db);
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn matches_python_reference_ingest() {
        let mut rust_rows = run_rust_build();
        let mut python_rows = run_python_reference();

        assert_eq!(rust_rows.len(), python_rows.len(), "different total record count");

        // Compared as sets, not sequences: directory traversal order is
        // arbitrary filesystem order on both sides (and irrelevant to search,
        // which ranks by relevance). Sorting on the row's own content keeps
        // the comparison exact without depending on that ordering.
        let sort_key = |r: &DumpedRow| {
            (
                r.catalog.clone(),
                r.accessno.clone(),
                normalize_ws(&r.title),
                normalize_ws(&r.body),
                r.detail_json.clone(),
            )
        };
        rust_rows.sort_by_key(sort_key);
        python_rows.sort_by_key(sort_key);

        for (i, (r, p)) in rust_rows.iter().zip(python_rows.iter()).enumerate() {
            assert_eq!(r.catalog, p.catalog, "row #{i}: catalog differs");
            assert_eq!(r.accessno, p.accessno, "row #{i} ({}): accessno differs", r.catalog);
            // Compared on word content, not exact whitespace: this port
            // deliberately drops empty fields before joining, where ingest.py
            // joined them in regardless and so left stray gaps ("Print,
            // Photographic   THE SHADOWOOD PUPPETS"). That's an intentional
            // cosmetic improvement; any real content difference still fails.
            assert_eq!(
                normalize_ws(&r.title),
                normalize_ws(&p.title),
                "row #{i} ({}): title content differs",
                r.catalog
            );
            assert_eq!(
                normalize_ws(&r.body),
                normalize_ws(&p.body),
                "row #{i} ({}) {:?}: body content differs",
                r.catalog,
                r.title
            );
            assert_eq!(r.has_image, p.has_image, "row #{i} ({}) {:?}: has_image differs", r.catalog, r.title);
            assert_eq!(r.has_thumb, p.has_thumb, "row #{i} ({}) {:?}: has_thumb differs", r.catalog, r.title);

            let r_detail: serde_json::Value = serde_json::from_str(&r.detail_json).unwrap();
            let p_detail: serde_json::Value = serde_json::from_str(&p.detail_json).unwrap();
            assert_eq!(r_detail, p_detail, "row #{i} ({}) {:?}: detail_json differs", r.catalog, r.title);
        }
    }
}
