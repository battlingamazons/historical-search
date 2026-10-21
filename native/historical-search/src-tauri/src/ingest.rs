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
const DOCUMENT_EXTENSIONS: &[&str] = &["pdf", "docx", "txt", "md"];
const SPREADSHEET_EXTENSIONS: &[&str] = &["xlsx", "xlsm", "xls"];
pub const THUMB_MAX_DIM: u32 = 1600;

pub struct PhotoArchiveRoot {
    pub label: String,
    pub path: PathBuf,
}

/// A folder of PDF documents to make searchable. Like a photo folder it is
/// read and never changed, and nothing in it is assumed to be a PastPerfect
/// record.
pub struct DocumentRoot {
    pub label: String,
    pub path: PathBuf,
}

/// An Excel workbook, or a folder of them, whose rows are made searchable.
/// Each row becomes a record of its own.
pub struct SpreadsheetRoot {
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

/// The two identifiers a catalog record has: the one images are attached by,
/// and the one shown to the reader.
///
/// OBJECTID is the full, unique per-item catalog number ("2010.2.7") and is
/// what MEDIA.DBF keys images on. ACCESSNO is only the accession *lot* the
/// item arrived in, shared by everything that arrived with it, so it is not a
/// stand-in for the real id.
///
/// They are returned separately because a record with no object id should
/// still show its accession number rather than look anonymous -- but must not
/// be looked up by it. Accession numbers and object ids are not drawn from
/// the same set: in this collection 170 accession numbers are also some
/// *other* record's object id, so one used as a stand-in would quietly hang
/// that other record's photographs on this one, with nothing on screen
/// looking wrong.
fn record_ids(rec: &HashMap<String, String>) -> (String, String) {
    let object_id = rec.get("OBJECTID").map(|s| s.trim().to_string()).unwrap_or_default();
    let displayed = if object_id.is_empty() {
        rec.get("ACCESSNO").map(|s| s.trim().to_string()).unwrap_or_default()
    } else {
        object_id.clone()
    };
    (object_id, displayed)
}

/// The record's title, built from whichever of the table's title fields are
/// filled in. A record with none of them is not indexed at all, which is why
/// this is needed twice: once to decide which records' pictures are worth
/// making previews for, and again when the rows are built.
fn record_title(rec: &HashMap<String, String>, spec: &CatalogSpec) -> String {
    spec.title_fields
        .iter()
        .map(|f| rec.get(*f).map(|s| s.trim()).unwrap_or(""))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Makes previews for a list of images, showing progress under `label` and
/// recording what it cost.
///
/// Duplicates are removed first: a photograph that several records share, and
/// a record's own IMAGEFILE that MEDIA.DBF also lists, would otherwise be
/// counted more than once and make the total on screen larger than the work
/// actually remaining.
fn warm_previews(label: &str, paths: &[PathBuf], thumb_cache_dir: &Path, progress: &mut ProgressFn) {
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<PathBuf> = paths.iter().filter(|p| seen.insert((*p).clone())).cloned().collect();
    if unique.is_empty() {
        return;
    }

    let owned_label = label.to_string();
    let stats = crate::thumbnails::warm_previews(
        &unique,
        thumb_cache_dir,
        THUMB_MAX_DIM,
        &mut |done, total| progress(&owned_label, Some(done), Some(total)),
    );
    // Only worth a line in the log when there was something to do; on every
    // run after the first this pass finds everything already made.
    if stats.made > 0 || stats.failed > 0 {
        crate::diagnostics::info("index", format!("{label}: {}", stats.summary()));
    }
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
    /// Set for a PDF; the rest of the row then has no pictures.
    document: Option<DocumentRow>,
}

/// What the index keeps about a PDF beyond its searchable text.
struct DocumentRow {
    path: String,
    pages: usize,
    has_text: bool,
    /// The text of each page (empty when there is none), so a search can say
    /// which pages a word is on.
    page_text: Vec<String>,
}

/// Builds the ordered image list for one catalog record.
///
/// `primary` is the image named by the record's own IMAGEFILE field, which is
/// all the app used to know about. When MEDIA.DBF has entries for this record
/// they supersede it -- MEDIA is where the second and subsequent images live,
/// along with their captions and their links to the high-resolution masters.
fn images_for_record(object_id: &str, primary: Option<&Path>, ctx: &IngestContext) -> Vec<ImageRow> {
    image_sources_for_record(object_id, primary, ctx)
        .into_iter()
        .map(|(path, caption, hires)| ImageRow {
            caption,
            thumb_path: get_preview_jpeg(&path, &ctx.thumb_cache_dir, THUMB_MAX_DIM)
                .map(|p| p.to_string_lossy().to_string()),
            image_path: path.to_string_lossy().to_string(),
            hires_path: hires.map(|p| p.to_string_lossy().to_string()),
        })
        .collect()
}

/// Which files a record's images actually come from, in the order they will be
/// shown, with the caption and high-resolution master belonging to each.
///
/// Kept separate from building the rows so that the pass which makes previews
/// ahead of time works from exactly the same list. If the two disagreed, an
/// image would be missed by the bulk pass and then decoded one at a time while
/// the index was being written -- the slow behaviour this is here to avoid,
/// reappearing silently for some records only.
type ImageSource = (PathBuf, String, Option<PathBuf>);

fn image_sources_for_record(
    object_id: &str,
    primary: Option<&Path>,
    ctx: &IngestContext,
) -> Vec<ImageSource> {
    if let Some(entries) = ctx.media.get(&media_key(object_id)) {
        let sources: Vec<ImageSource> = entries
            .iter()
            .filter_map(|e| {
                let path = resolve_pastperfect_image(&e.imagefile, &ctx.images_dir)?;
                Some((path, e.caption.clone(), ctx.multimedia.lookup(&e.hires_ref)))
            })
            .collect();
        if !sources.is_empty() {
            return sources;
        }
        // Every file MEDIA names is missing from disk; fall through so the
        // record still shows its own IMAGEFILE if that one happens to exist.
    }

    primary
        .map(|p| vec![(p.to_path_buf(), String::new(), None)])
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

    let records = table.records();

    // Every preview this catalogue needs, made before the rows are built
    // rather than one at a time while building them. Two reasons: several can
    // then be decoded at once, and there is a number to show while it happens
    // -- the whole catalogue used to pass under one unchanging "Reading Photo
    // records", which on a first run meant no sign of movement for an hour.
    let mut wanted: Vec<PathBuf> = Vec::new();
    for rec in &records {
        if record_title(rec, spec).is_empty() {
            continue;
        }
        let (object_id, _) = record_ids(rec);
        let primary =
            rec.get("IMAGEFILE").and_then(|v| resolve_pastperfect_image(v, &ctx.images_dir));
        for (path, _, _) in image_sources_for_record(&object_id, primary.as_deref(), ctx) {
            wanted.push(path);
        }
        if let Some(p) = primary {
            wanted.push(p);
        }
    }
    warm_previews(
        &format!("Preparing pictures for {} records", spec.catalog),
        &wanted,
        &ctx.thumb_cache_dir,
        progress,
    );

    let mut rows = Vec::new();
    for rec in &records {
        let title = record_title(rec, spec);
        if title.is_empty() {
            continue;
        }

        let (object_id, displayed_id) = record_ids(rec);

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
            accessno: displayed_id,
            title,
            body,
            image_path: image_path.map(|p| p.to_string_lossy().to_string()),
            thumb_path: thumb_path.map(|p| p.to_string_lossy().to_string()),
            detail_json: serde_json::Value::Object(detail).to_string(),
            images,
            document: None,
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

    // As with the catalogue: all the previews first, several at a time, so the
    // count on screen moves through the slow part rather than through the
    // quick one that follows it.
    let wanted: Vec<PathBuf> = all_files.iter().map(|(dir, name, _)| dir.join(name)).collect();
    warm_previews(&format!("Preparing pictures in {}", root.label), &wanted, thumb_cache_dir, progress);

    let mut rows = Vec::with_capacity(all_files.len());
    for (dirpath, fname, category) in all_files.into_iter() {
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
            document: None,
        });
    }
    rows
}

/// Whether scanned PDFs get their words read off the page pictures.
pub enum Recognition<'a> {
    /// Turned off in Folder Settings.
    Off,
    /// Wanted, but there is no way to do it here, and why.
    Unavailable(String),
    On(&'a crate::ocr::Engine),
}

/// What a rebuild is told about PDFs: which folders, and how to read the
/// scanned ones.
pub struct DocumentSettings<'a> {
    pub roots: &'a [DocumentRoot],
    /// Workbooks to read the rows of. Carried here because it is, like the
    /// PDF folders, a kind of file the index reads that is not PastPerfect.
    pub spreadsheets: &'a [SpreadsheetRoot],
    pub recognition: Recognition<'a>,
    /// Where recognised text is kept between rebuilds.
    pub saved_dir: PathBuf,
    /// Scratch space for the page pictures while they are being read.
    pub work_dir: PathBuf,
}

impl<'a> DocumentSettings<'a> {
    /// PDF folders with no text recognition: a scanned PDF is found by name.
    pub fn new(roots: &'a [DocumentRoot]) -> Self {
        DocumentSettings {
            roots,
            spreadsheets: &[],
            recognition: Recognition::Off,
            saved_dir: std::env::temp_dir().join("hs_unused_ocr_saved"),
            work_dir: std::env::temp_dir().join("hs_unused_ocr_work"),
        }
    }
}

/// What a rebuild found out about the PDFs, for the report afterwards.
#[derive(Default)]
struct DocumentStats {
    read: usize,
    /// PDFs with no text layer whose words were read off the pages.
    recognized: usize,
    /// PDFs that are only pictures and were not read (see `recognition_off`)
    /// or had no words on them.
    scanned: Vec<String>,
    failed: Vec<(String, String)>,
    /// Why scanned PDFs were not recognised, when that was by choice or by
    /// lack of means rather than by failure.
    recognition_off: Option<String>,
}

/// Indexes a folder of PDFs by the text inside them, plus their names and
/// the folders they sit in. Like standalone photos they are not linked to
/// any PastPerfect record.
///
/// A PDF whose text cannot be read -- a scan that is only pictures of pages,
/// or a file too damaged or too large -- is still listed, and still found by
/// its name; it is only the words inside that are missing, and the record
/// says so rather than leaving the cause to be guessed.
fn index_document_root(
    root: &DocumentRoot,
    settings: &DocumentSettings,
    progress: &mut ProgressFn,
    warnings: &mut Vec<String>,
    stats: &mut DocumentStats,
) -> Vec<ItemRow> {
    if !root.path.is_dir() {
        warnings.push(format!(
            "The PDF folder {:?} ({}) could not be read, so none of its documents are in \
             the index.",
            root.label,
            root.path.display()
        ));
        return Vec::new();
    }

    progress(&format!("Scanning {}…", root.label), None, None);
    let mut all_files: Vec<(PathBuf, String, Option<String>)> = Vec::new();
    collect_files(&root.path, &root.path, DOCUMENT_EXTENSIONS, &mut all_files);

    // All the reading first, several at a time, with the count on screen
    // moving through it; the rows are then put together in folder order.
    let label = format!("Reading PDFs in {}", root.label);
    let paths: Vec<PathBuf> = all_files.iter().map(|(dir, name, _)| dir.join(name)).collect();
    let read = crate::pdf::read_many(&paths, &mut |done, total| progress(&label, Some(done), Some(total)));

    // How many of them will need recognising, so the count on screen can say
    // "scan 3 of 12" through what is the slow part.
    let scans_total = read.iter().filter(|r| matches!(r, Ok(pdf) if !pdf.has_text())).count();
    let mut scan_number = 0usize;

    let mut rows = Vec::with_capacity(all_files.len());
    for ((dirpath, fname, category), outcome) in all_files.into_iter().zip(read) {
        let full_path = dirpath.join(&fname);
        let name_no_ext = Path::new(&fname)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| fname.clone());
        let readable: String =
            name_no_ext.chars().map(|c| if c == '_' || c == '-' { ' ' } else { c }).collect();
        let readable = readable.trim().to_string();
        let title = if readable.is_empty() { fname.clone() } else { readable };

        let mut ocr_note: Option<String> = None;
        let mut page_text: Vec<String> = Vec::new();
        let (pages, text, note) = match outcome {
            Ok(pdf) if pdf.has_text() => {
                stats.read += 1;
                page_text = pdf.page_text;
                (pdf.pages, pdf.text, None)
            }
            // A Word or text file with nothing in it: there are no page
            // pictures to read, so it is listed by name only.
            Ok(pdf) if !crate::wordtext::is_pdf(&full_path) => (
                pdf.pages,
                String::new(),
                Some("None -- this file has no words in it, so just its name and folder are searchable"),
            ),
            // No text layer: a scan. Read the words off the page pictures if
            // that is wanted and possible.
            Ok(pdf) => match &settings.recognition {
                Recognition::On(engine) => {
                    scan_number += 1;
                    let label = format!("Recognizing text in {fname} (scan {scan_number} of {scans_total})");
                    match crate::ocr::recognize_pdf(
                        engine,
                        &full_path,
                        &settings.saved_dir,
                        &settings.work_dir,
                        &mut |done, total| progress(&label, Some(done), Some(total)),
                    ) {
                        Ok(found) if found.text.has_text() => {
                            stats.recognized += 1;
                            ocr_note = Some(if found.pages_not_read > 0 {
                                format!(
                                    "Recognized from the scanned pages (OCR), so it may contain mistakes. \
                                     {} page(s) could not be read.",
                                    found.pages_not_read
                                )
                            } else {
                                "Recognized from the scanned pages (OCR), so it may contain mistakes.".to_string()
                            });
                            page_text = found.text.page_text;
                            (found.text.pages, found.text.text, None)
                        }
                        Ok(found) => {
                            stats.scanned.push(fname.clone());
                            (
                                found.text.pages,
                                String::new(),
                                Some("None -- no words could be recognized on the pages, so just its name and folder are searchable"),
                            )
                        }
                        Err(reason) => {
                            crate::diagnostics::warn(
                                "ocr",
                                format!("could not recognize the text of {}: {reason}", full_path.display()),
                            );
                            stats.failed.push((fname.clone(), reason));
                            (pdf.pages, String::new(), Some("None -- the pages could not be read for text, so just its name and folder are searchable"))
                        }
                    }
                }
                Recognition::Off => {
                    stats.scanned.push(fname.clone());
                    stats
                        .recognition_off
                        .get_or_insert_with(|| "text recognition is turned off in Folder Settings".to_string());
                    (
                        pdf.pages,
                        String::new(),
                        Some("None -- this PDF is only pictures of pages and text recognition is off, so just its name and folder are searchable"),
                    )
                }
                Recognition::Unavailable(why) => {
                    stats.scanned.push(fname.clone());
                    stats
                        .recognition_off
                        .get_or_insert_with(|| format!("text recognition is not available ({why})"));
                    (
                        pdf.pages,
                        String::new(),
                        Some("None -- this PDF is only pictures of pages and text recognition is not available, so just its name and folder are searchable"),
                    )
                }
            },
            Err(reason) => {
                crate::diagnostics::warn(
                    "index",
                    format!("could not read the text of {}: {reason}", full_path.display()),
                );
                stats.failed.push((fname.clone(), reason));
                (0, String::new(), Some("None -- the text of this file could not be read, so just its name and folder are searchable"))
            }
        };

        // The document's own words first, then the folder it came from: the
        // passage shown under a result is cut from this, and one that opened
        // with the folder name on every PDF would be noise before the text.
        let mut body_parts = Vec::new();
        if !text.is_empty() {
            body_parts.push(text.clone());
        }
        body_parts.push(root.label.clone());
        if let Some(c) = &category {
            body_parts.push(c.clone());
        }

        let mut detail = serde_json::Map::new();
        detail.insert("FOLDER".to_string(), json!(root.label));
        if let Some(c) = &category {
            if !c.is_empty() {
                detail.insert("CATEGORY".to_string(), json!(c));
            }
        }
        detail.insert("FILENAME".to_string(), json!(fname));
        if pages > 0 && crate::wordtext::is_pdf(&full_path) {
            detail.insert("PAGES".to_string(), json!(pages.to_string()));
        }
        if let Some(note) = note {
            detail.insert("TEXT".to_string(), json!(note));
        } else if let Some(note) = &ocr_note {
            detail.insert("TEXT".to_string(), json!(note));
        }

        rows.push(ItemRow {
            catalog: root.label.clone(),
            accessno: String::new(),
            title,
            body: body_parts.join("\n"),
            image_path: None,
            thumb_path: None,
            detail_json: serde_json::Value::Object(detail).to_string(),
            images: Vec::new(),
            document: Some(DocumentRow {
                path: full_path.to_string_lossy().to_string(),
                pages,
                has_text: !text.is_empty(),
                page_text,
            }),
        });
    }
    rows
}

/// What a rebuild found out about the workbooks, for the report afterwards.
#[derive(Default)]
struct SheetStats {
    failed: Vec<(String, String)>,
    /// Workbooks that opened but held no rows to make records of.
    empty: Vec<String>,
    rows_dropped: usize,
}

/// The workbook files named by `root`: the file itself, or every workbook
/// under the folder.
fn spreadsheet_files(root: &Path) -> Vec<(PathBuf, String)> {
    if root.is_file() {
        let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        return vec![(root.to_path_buf(), name)];
    }
    let mut found: Vec<(PathBuf, String, Option<String>)> = Vec::new();
    collect_files(root, root, SPREADSHEET_EXTENSIONS, &mut found);
    found
        .into_iter()
        // Excel leaves "~$name.xlsx" beside a workbook that is open. It is a
        // few bytes of lock, not a workbook.
        .filter(|(_, name, _)| !name.starts_with("~$"))
        .map(|(dir, name, _)| (dir.join(&name), name))
        .collect()
}

/// How many workbooks a candidate path holds, for the confirmation shown
/// while typing it in Folder Settings.
pub fn count_spreadsheet_files(path: &Path) -> usize {
    if path.is_file() {
        return usize::from(
            path.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| SPREADSHEET_EXTENSIONS.contains(&e.to_lowercase().as_str())),
        );
    }
    spreadsheet_files(path).len()
}

/// Header words that name what a row is about, best first.
const TITLE_HEADINGS: &[&str] = &["title", "name", "object", "item", "subject", "description"];

/// Headings that hold the row's own catalogue number. The same few spellings
/// PastPerfect's users give it; anything else is just another column.
const NUMBER_HEADINGS: &[&str] = &[
    "accession", "catalog no", "catalog num", "catalog #", "catalogue no", "catalogue num",
    "object id", "object no", "object num",
];

fn heading_words(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '#' { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Indexes the rows of Excel workbooks: one record per row, found by what is
/// in its cells and shown with its column headings.
fn index_spreadsheet_root(
    root: &SpreadsheetRoot,
    progress: &mut ProgressFn,
    warnings: &mut Vec<String>,
    stats: &mut SheetStats,
) -> Vec<ItemRow> {
    if !root.path.exists() {
        warnings.push(format!(
            "The spreadsheet {:?} ({}) could not be found, so none of its rows are in the index.",
            root.label,
            root.path.display()
        ));
        return Vec::new();
    }
    progress(&format!("Scanning {}…", root.label), None, None);
    let files = spreadsheet_files(&root.path);
    let total = files.len() as u64;
    let mut rows = Vec::new();
    for (i, (full_path, fname)) in files.into_iter().enumerate() {
        progress(&format!("Reading spreadsheets in {}", root.label), Some(i as u64), Some(total));
        let book = match crate::sheets::read(&full_path) {
            Ok(book) => book,
            Err(reason) => {
                crate::diagnostics::warn(
                    "index",
                    format!("could not read the spreadsheet {}: {reason}", full_path.display()),
                );
                stats.failed.push((fname, reason));
                continue;
            }
        };
        if book.hidden_sheets > 0 {
            crate::diagnostics::info(
                "index",
                format!("{fname}: left out {} hidden sheet(s)", book.hidden_sheets),
            );
        }
        stats.rows_dropped += book.rows_dropped;
        if book.sheets.is_empty() {
            stats.empty.push(fname);
            continue;
        }
        let several_sheets = book.sheets.len() > 1;
        let stem = Path::new(&fname)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| fname.clone());
        for sheet in &book.sheets {
            rows.extend(sheet_rows(root, &fname, &stem, several_sheets, sheet));
        }
    }
    rows
}

fn sheet_rows(
    root: &SpreadsheetRoot,
    fname: &str,
    stem: &str,
    several_sheets: bool,
    sheet: &crate::sheets::Sheet,
) -> Vec<ItemRow> {
    let words: Vec<String> = sheet.columns.iter().map(|c| heading_words(c)).collect();
    let title_column = TITLE_HEADINGS
        .iter()
        .find_map(|want| words.iter().position(|w| w == want || w.split(' ').any(|x| x == *want)));
    let number_column = words.iter().position(|w| NUMBER_HEADINGS.iter().any(|n| w.contains(n)));
    let order = json!(sheet
        .columns
        .iter()
        .map(String::as_str)
        .chain(["SPREADSHEET", "SHEET", "ROW"])
        .collect::<Vec<_>>());

    let mut out = Vec::new();
    for row in &sheet.rows {
        let cell = |i: usize| row.cells.get(i).map(String::as_str).unwrap_or("");
        let first_filled = row.cells.iter().find(|c| !c.is_empty()).map(String::as_str).unwrap_or("");
        let title_source = title_column.map(cell).filter(|t| !t.is_empty()).unwrap_or(first_filled);
        let mut title: String = title_source.split_whitespace().collect::<Vec<_>>().join(" ");
        if title.chars().count() > 100 {
            title = title.chars().take(100).collect::<String>() + "…";
        }
        if title.is_empty() {
            continue;
        }

        let mut body_parts: Vec<&str> = row.cells.iter().map(String::as_str).filter(|c| !c.is_empty()).collect();
        if !sheet.preamble.is_empty() {
            body_parts.push(&sheet.preamble);
        }
        body_parts.push(&root.label);
        body_parts.push(stem);
        if several_sheets {
            body_parts.push(&sheet.name);
        }

        let mut detail = serde_json::Map::new();
        for (heading, value) in sheet.columns.iter().zip(&row.cells) {
            if !value.is_empty() {
                detail.insert(heading.clone(), json!(value));
            }
        }
        detail.insert("SPREADSHEET".to_string(), json!(fname));
        detail.insert("SHEET".to_string(), json!(sheet.name));
        detail.insert("ROW".to_string(), json!(row.number.to_string()));
        detail.insert("__order".to_string(), order.clone());

        out.push(ItemRow {
            catalog: root.label.clone(),
            accessno: number_column.map(cell).unwrap_or("").to_string(),
            title,
            body: body_parts.join("\n"),
            image_path: None,
            thumb_path: None,
            detail_json: serde_json::Value::Object(detail).to_string(),
            images: Vec::new(),
            document: None,
        });
    }
    out
}

/// Folder Settings lets the user point at any folder, and a symlink (or
/// Windows junction) pointing back at an ancestor would otherwise recurse
/// until the stack overflows -- which aborts the whole app. Real photo
/// folders here nest ~3 deep; 32 is far beyond any legitimate layout.
const MAX_WALK_DEPTH: usize = 32;

fn collect_image_files(dir: &Path, root: &Path, out: &mut Vec<(PathBuf, String, Option<String>)>) {
    collect_files(dir, root, IMAGE_EXTENSIONS, out)
}

/// Every file under `dir` whose extension is one of `extensions`, as
/// (folder, file name, first folder below `root`).
fn collect_files(
    dir: &Path,
    root: &Path,
    extensions: &[&str],
    out: &mut Vec<(PathBuf, String, Option<String>)>,
) {
    collect_files_depth(dir, root, extensions, out, 0)
}

fn collect_files_depth(
    dir: &Path,
    root: &Path,
    extensions: &[&str],
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
            collect_files_depth(&path, root, extensions, out, depth + 1);
        } else if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
            // A Mac that copies a folder to a Windows or exFAT drive leaves a
            // small "._" file beside every file it has extra information
            // about. It carries the real file's name and extension but is not
            // a PDF or a picture, and used to be reported as unreadable.
            // Word keeps a "~$" file beside a document that is open.
            if fname.starts_with("._") || fname.starts_with("~$") {
                continue;
            }
            let ext = Path::new(fname).extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase());
            if ext.as_deref().is_some_and(|e| extensions.contains(&e)) {
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
//
// Each argument is one folder the settings name, and every caller has all of
// them to hand; bundling them into a struct would add a type that exists only
// to be unpacked on the next line.
#[allow(clippy::too_many_arguments)]
pub fn build(
    data_dir: &Path,
    db_path: &Path,
    images_dir: &Path,
    multimedia_dir: &Path,
    photo_archive_roots: &[PhotoArchiveRoot],
    documents: &DocumentSettings,
    thumb_cache_dir: &Path,
    progress: Option<&mut ProgressFn>,
) -> rusqlite::Result<BuildResult> {
    let mut fallback = noop_progress;
    let progress: &mut ProgressFn = match progress {
        Some(p) => p,
        None => &mut fallback,
    };

    let started = std::time::Instant::now();
    let started_at = std::time::SystemTime::now();
    crate::diagnostics::reset_repeat_counts();
    crate::diagnostics::info(
        "index",
        format!(
            "rebuild starting\n    data:       {}\n    images:     {}\n    multimedia: {}\n    \
             PDF folders: {}\n    photo folders: {}",
            data_dir.display(),
            images_dir.display(),
            multimedia_dir.display(),
            if documents.roots.is_empty() {
                "(none)".to_string()
            } else {
                documents
                    .roots
                    .iter()
                    .map(|r| format!("{:?} at {}", r.label, r.path.display()))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
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
        CREATE INDEX item_images_by_item ON item_images(item_id, ordinal);
        -- One row per PDF. Its words are in items.body like everything else's;
        -- this is what the window needs to open the file and describe it.
        CREATE TABLE documents (
            item_id INTEGER PRIMARY KEY,
            path TEXT NOT NULL,
            pages INTEGER NOT NULL,
            has_text INTEGER NOT NULL
        );
        -- The text of each page of each PDF, for saying which pages a search
        -- matched. Not searched itself (items_fts is); only read for the one
        -- PDF being looked at.
        CREATE TABLE document_pages (
            item_id INTEGER NOT NULL,
            page INTEGER NOT NULL,
            text TEXT NOT NULL,
            PRIMARY KEY (item_id, page)
        ) WITHOUT ROWID;",
    )?;

    // Before any previews are made: saved previews that are blank or damaged
    // would otherwise be trusted and kept (see `repair_saved_previews`).
    let repaired = crate::thumbnails::repair_saved_previews(thumb_cache_dir, &mut |done, total| {
        progress("Checking saved previews", Some(done), Some(total))
    });
    if repaired.checked > 0 {
        crate::diagnostics::info(
            "index",
            format!(
                "checked {} saved previews: removed {} blank and {} damaged ones, which are made again below",
                repaired.checked, repaired.blank, repaired.damaged
            ),
        );
    }

    let mut total: u64 = 0;
    let mut warnings: Vec<String> = Vec::new();
    let mut document_stats = DocumentStats::default();
    let mut sheet_stats = SheetStats::default();
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
            let mut document_stmt = tx.prepare(
                "INSERT INTO documents (item_id, path, pages, has_text) VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut page_stmt = tx.prepare(
                "INSERT INTO document_pages (item_id, page, text) VALUES (?1, ?2, ?3)",
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
                    if let Some(doc) = &r.document {
                        document_stmt.execute(rusqlite::params![
                            item_id,
                            doc.path,
                            doc.pages as i64,
                            doc.has_text as i64
                        ])?;
                        for (i, page) in doc.page_text.iter().enumerate().filter(|(_, p)| !p.is_empty()) {
                            page_stmt.execute(rusqlite::params![item_id, (i + 1) as i64, page])?;
                        }
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

            warnings.extend(overlapping_folder_warnings(images_dir, photo_archive_roots));
            for root in photo_archive_roots {
                let rows = index_photo_archive_root(root, thumb_cache_dir, progress, &mut warnings);
                insert_all(rows)?;
            }
            for root in documents.roots {
                let rows = index_document_root(root, documents, progress, &mut warnings, &mut document_stats);
                insert_all(rows)?;
            }
            for root in documents.spreadsheets {
                let rows = index_spreadsheet_root(root, progress, &mut warnings, &mut sheet_stats);
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
    // Only after a clean build. A folder that could not be read (a network
    // share that is offline) contributes no rows, and pruning then would throw
    // away its previews and make the next refresh redo them from scratch.
    if warnings.is_empty() {
        prune_thumb_cache(db_path, thumb_cache_dir, started_at);
    }
    // Said after the decision above, not before it: these describe PDFs that
    // could not give up their words, which says nothing about whether any
    // folder was missing, and must not stop the previews being tidied.
    warnings.extend(document_notices(&document_stats));
    warnings.extend(sheet_notices(&sheet_stats));
    for w in &warnings {
        crate::diagnostics::warn("index", w);
    }
    for (key, count) in crate::diagnostics::repeat_totals() {
        crate::diagnostics::info("index", format!("during this rebuild, '{key}' happened {count} times"));
    }

    Ok(BuildResult { total_records: total, warnings })
}

/// What to tell the person about the PDFs that could not be searched by their
/// contents. Silence would read as "nothing in that one matched".
fn sheet_notices(stats: &SheetStats) -> Vec<String> {
    let mut notices = Vec::new();
    if !stats.failed.is_empty() {
        let shown = stats.failed.iter().take(3).map(|(n, why)| format!("{n} ({why})")).collect::<Vec<_>>().join("; ");
        notices.push(format!(
            "{} spreadsheet{} could not be read, so {} rows are not in the index: {shown}.",
            stats.failed.len(),
            if stats.failed.len() == 1 { "" } else { "s" },
            if stats.failed.len() == 1 { "its" } else { "their" },
        ));
    }
    if !stats.empty.is_empty() {
        notices.push(format!(
            "No rows were found in {}.",
            stats.empty.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if stats.rows_dropped > 0 {
        notices.push(format!(
            "{} spreadsheet rows beyond the first 500,000 were left out of the index.",
            stats.rows_dropped
        ));
    }
    notices
}

fn document_notices(stats: &DocumentStats) -> Vec<String> {
    // Up to three names, then a count of the rest.
    let names = |all: Vec<&str>| -> String {
        let shown = all.iter().take(3).copied().collect::<Vec<_>>().join(", ");
        match all.len().saturating_sub(3) {
            0 => shown,
            more => format!("{shown} and {more} more"),
        }
    };
    let mut notices = Vec::new();
    if !stats.scanned.is_empty() {
        let n = stats.scanned.len();
        let reason = match &stats.recognition_off {
            Some(why) => format!("{why}, so the words on the pages were not read"),
            None => "no words could be recognized on the pages".to_string(),
        };
        notices.push(format!(
            "{n} PDF{} {} no searchable text ({}): {reason}. {} can still be found by name.",
            if n == 1 { "" } else { "s" },
            if n == 1 { "has" } else { "have" },
            names(stats.scanned.iter().map(String::as_str).collect()),
            if n == 1 { "It" } else { "They" },
        ));
    }
    if !stats.failed.is_empty() {
        notices.push(format!(
            "The text of {} PDF{} could not be read ({}). {} can still be found by name; \
             the Diagnostics page has the details.",
            stats.failed.len(),
            if stats.failed.len() == 1 { "" } else { "s" },
            names(stats.failed.iter().map(|(n, _)| n.as_str()).collect()),
            if stats.failed.len() == 1 { "It" } else { "They" },
        ));
    }
    notices
}

/// Deletes previews that nothing in the finished index points at -- what is
/// left behind when a photo folder is removed from the settings or its files
/// are deleted. Left alone they cost disk space forever and are never shown.
///
/// Safe by construction: the cache holds only regenerable stand-ins (the
/// window remakes a missing one on demand), only `<xx>/<hash>.jpg` files are
/// considered, and anything written since `built_before` -- by this rebuild
/// or by the window making one on demand meanwhile -- is kept. If the index
/// cannot be read or lists no previews at all, nothing is deleted.
pub fn prune_thumb_cache(db_path: &Path, cache_dir: &Path, built_before: std::time::SystemTime) -> (usize, u64) {
    let Ok(conn) = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return (0, 0);
    };
    let wanted: std::collections::HashSet<PathBuf> = {
        let Ok(mut stmt) = conn.prepare(
            "SELECT thumb_path FROM items WHERE thumb_path IS NOT NULL
             UNION SELECT thumb_path FROM item_images WHERE thumb_path IS NOT NULL",
        ) else {
            return (0, 0);
        };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else { return (0, 0) };
        let mut set = std::collections::HashSet::new();
        for row in rows {
            match row {
                Ok(path) => {
                    set.insert(PathBuf::from(path));
                }
                Err(_) => return (0, 0),
            }
        }
        set
    };
    if wanted.is_empty() {
        return (0, 0);
    }

    let (mut removed, mut bytes) = (0usize, 0u64);
    let Ok(subdirs) = fs::read_dir(cache_dir) else { return (0, 0) };
    for sub in subdirs.flatten() {
        if !sub.path().is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(sub.path()) else { continue };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jpg") || wanted.contains(&path) {
                continue;
            }
            let Ok(meta) = file.metadata() else { continue };
            if !meta.is_file() || meta.modified().map_or(true, |m| m >= built_before) {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                removed += 1;
                bytes += meta.len();
            }
        }
    }
    if removed > 0 {
        crate::diagnostics::info(
            "index",
            format!(
                "removed {removed} preview(s) no longer in the index (a folder taken out of the \
                 settings, or deleted pictures), freeing {:.1}MB",
                bytes as f64 / 1_048_576.0
            ),
        );
    }
    (removed, bytes)
}

/// How old an abandoned index-in-progress must be before it is deleted.
///
/// Comfortably longer than any real rebuild -- the slowest observed took two
/// hours -- because a second copy of the application may be part-way through
/// one right now, writing to a file of its own. The file's timestamp is set
/// when the rebuild starts and not touched again while the previews are being
/// made, so age here means "started this long ago", not "idle this long".
pub const ABANDONED_BUILD_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Deletes half-built indexes left behind by a rebuild that never finished.
///
/// A rebuild writes to `search_index.db.building.<something>` and renames it
/// into place at the end. If the application stops before that -- closed,
/// force-quit, or killed, all of which happened on the society's computer
/// while the first rebuild was taking two hours and looking hung -- the file
/// stays there for good. Nothing ever mentioned it and nothing ever removed
/// it, so every abandoned attempt quietly left another twenty megabytes in the
/// application's own folder.
///
/// Returns what was removed, for the log and the diagnostic report.
pub fn remove_abandoned_build_files(
    data_dir: &Path,
    older_than: std::time::Duration,
) -> (usize, u64) {
    let Ok(entries) = fs::read_dir(data_dir) else { return (0, 0) };
    let mut removed = 0usize;
    let mut bytes = 0u64;

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // The journal beside it belongs to the same abandoned attempt.
        if !name.contains(".db.building.") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let age = meta
            .modified()
            .ok()
            .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
            .unwrap_or_default();
        if age < older_than {
            continue;
        }
        let size = meta.len();
        if fs::remove_file(&path).is_ok() {
            removed += 1;
            bytes += size;
        }
    }

    if removed > 0 {
        crate::diagnostics::info(
            "index",
            format!(
                "removed {removed} half-built index file(s) left by a rebuild that did not \
                 finish, freeing {:.1}MB",
                bytes as f64 / 1_048_576.0
            ),
        );
    }
    (removed, bytes)
}

/// Half-built indexes present right now, for the diagnostic report: recent
/// ones are not deleted, so something has to say they are there.
pub fn abandoned_build_files(data_dir: &Path) -> Vec<(String, u64)> {
    let Ok(entries) = fs::read_dir(data_dir) else { return Vec::new() };
    let mut found: Vec<(String, u64)> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".db.building."))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| (e.file_name().to_string_lossy().to_string(), meta.len()))
        })
        .collect();
    found.sort();
    found
}

/// Warns when configured folders overlap, which quietly lists the same
/// photograph more than once.
///
/// Adding a folder and then also one of the folders inside it is an easy
/// thing to do, and so is pointing a photo folder at PastPerfect's own Images
/// folder -- whose pictures are already indexed against their catalogue
/// records. Neither does any harm beyond the duplicates, so this says so
/// rather than refusing; but without saying so the only symptom is the same
/// picture appearing twice in a search with no indication why.
pub fn overlapping_folder_warnings(images_dir: &Path, roots: &[PhotoArchiveRoot]) -> Vec<String> {
    // Resolved where possible so that two different spellings of one folder
    // are recognised as the same place.
    let resolve = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let resolved: Vec<(String, PathBuf)> =
        roots.iter().map(|r| (r.label.clone(), resolve(&r.path))).collect();
    let mut warnings = Vec::new();

    for (i, (label, path)) in resolved.iter().enumerate() {
        for (other_label, other) in resolved.iter().skip(i + 1) {
            if path == other {
                warnings.push(format!(
                    "The photo folders {label:?} and {other_label:?} are the same folder, so \
                     every picture in it is listed twice."
                ));
            } else if other.starts_with(path) {
                warnings.push(format!(
                    "The photo folder {other_label:?} is inside {label:?}, so its pictures are \
                     listed twice. Remove one of them if that was not intended."
                ));
            } else if path.starts_with(other) {
                warnings.push(format!(
                    "The photo folder {label:?} is inside {other_label:?}, so its pictures are \
                     listed twice. Remove one of them if that was not intended."
                ));
            }
        }
    }

    if images_dir.as_os_str().is_empty() {
        return warnings;
    }
    let images = resolve(images_dir);
    for (label, path) in &resolved {
        if path == &images || path.starts_with(&images) || images.starts_with(path) {
            warnings.push(format!(
                "The photo folder {label:?} overlaps PastPerfect's own Images folder. Those \
                 pictures are already indexed against their catalogue records, so they are \
                 listed a second time with no catalogue number."
            ));
        }
    }
    warnings
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
    document_roots: &[DocumentRoot],
    spreadsheet_roots: &[SpreadsheetRoot],
) -> Result<(), String> {
    let has_data_setting = !data_dir_configured.trim().is_empty();
    let readable_photo_roots = photo_roots.iter().filter(|r| r.path.is_dir()).count()
        + document_roots.iter().filter(|r| r.path.is_dir()).count()
        + spreadsheet_roots.iter().filter(|r| r.path.exists()).count();

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
                    choose your PastPerfect Data folder (and any photo folders, PDF folders or spreadsheets) \
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

/// How many PDFs a candidate folder holds, for the same confirmation.
pub fn count_pdf_files(dir: &Path) -> usize {
    let mut found = Vec::new();
    collect_files(dir, dir, DOCUMENT_EXTENSIONS, &mut found);
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
        ("FILENAME", "Original Filename"), ("SPREADSHEET", "Spreadsheet"), ("SHEET", "Sheet"), ("ROW", "Row"), ("PAGES", "Pages"), ("TEXT", "Searchable text"),
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
        // A real build makes previews on every core, which would otherwise be
        // measured by whichever timing test happens to run beside it.
        let _machine = crate::test_support::exclusive_use_of_the_machine();
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
            &DocumentSettings::new(&[]),
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

    #[test]
    fn a_rebuild_that_never_finished_stops_leaving_its_index_behind() {
        let dir = std::env::temp_dir().join(format!("hs_leftovers_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let abandoned = dir.join("search_index.db.building.4242.0");
        let journal = dir.join("search_index.db.building.4242.0-journal");
        let real_index = dir.join("search_index.db");
        let config = dir.join("config.json");
        for path in [&abandoned, &journal, &real_index, &config] {
            fs::write(path, vec![b'x'; 2048]).unwrap();
        }

        // Nothing is touched while it could still belong to a rebuild that is
        // running right now -- which is the whole reason for the age at all.
        let (removed, _) = remove_abandoned_build_files(&dir, ABANDONED_BUILD_AGE);
        assert_eq!(removed, 0, "a file made a moment ago may be a live rebuild");
        assert!(abandoned.exists(), "it should still be there");

        // But it is reported, so a leftover is never simply invisible.
        let listed = abandoned_build_files(&dir);
        assert_eq!(listed.len(), 2, "both the index and its journal should be listed: {listed:?}");
        assert!(listed.iter().all(|(_, size)| *size == 2048));

        // Once old enough, it goes -- and nothing else does.
        let (removed, bytes) = remove_abandoned_build_files(&dir, std::time::Duration::ZERO);
        assert_eq!(removed, 2, "the abandoned index and its journal should both be removed");
        assert_eq!(bytes, 4096, "it should report what it freed");
        assert!(!abandoned.exists());
        assert!(!journal.exists());
        assert!(real_index.exists(), "the real index must never be removed");
        assert!(config.exists(), "nor anything else in the folder");
        assert!(abandoned_build_files(&dir).is_empty());

        // A folder that is not there at all is not an error.
        assert_eq!(remove_abandoned_build_files(&dir.join("gone"), std::time::Duration::ZERO), (0, 0));

        let _ = fs::remove_dir_all(&dir);
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
        let refused = rebuild_readiness(&parent.to_string_lossy(), &parent, &root(&photos), &[], &[])
            .expect_err("choosing the folder above Data must be refused");
        assert!(
            refused.contains("one folder too high"),
            "the refusal should explain the likely mistake, got: {refused}"
        );

        // A Data folder that has been moved or renamed since.
        let gone = base.join("pp5/NotThere");
        let missing = rebuild_readiness(&gone.to_string_lossy(), &gone, &root(&photos), &[], &[])
            .expect_err("a Data folder that is not there must be refused");
        assert!(missing.contains("not there any more"), "got: {missing}");

        // Nothing configured at all.
        let nothing = base.join("photos/Gone");
        rebuild_readiness("", Path::new(""), &root(&nothing), &[], &[])
            .expect_err("no readable folders at all must be refused");

        // And the arrangements that must still be allowed through.
        rebuild_readiness(&real_data.to_string_lossy(), &real_data, &root(&photos), &[], &[])
            .expect("a correct Data folder must be accepted");
        rebuild_readiness(&real_data.to_string_lossy(), &real_data, &[], &[], &[])
            .expect("a catalogue with no photo folders must be accepted");
        rebuild_readiness("", Path::new(""), &root(&photos), &[], &[])
            .expect("photo folders alone, with no catalogue, must be accepted");

        let _ = fs::remove_dir_all(&base);
    }

    /// Overlapping folders list the same picture more than once.
    #[test]
    fn overlapping_photo_folders_are_pointed_out() {
        let base = std::env::temp_dir().join(format!("hs_overlapwarn_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let outer = base.join("photos");
        let inner = outer.join("Scans");
        let separate = base.join("elsewhere");
        let images = base.join("pp5/Images");
        for d in [&inner, &separate, &images] {
            fs::create_dir_all(d).unwrap();
        }
        let root = |label: &str, p: &Path| PhotoArchiveRoot { label: label.into(), path: p.to_path_buf() };

        // A folder and one inside it.
        let nested = overlapping_folder_warnings(&images, &[root("All", &outer), root("Scans", &inner)]);
        assert_eq!(nested.len(), 1, "the nesting should be pointed out once: {nested:?}");
        assert!(nested[0].contains("is inside"), "got {:?}", nested[0]);

        // The same folder twice under different names.
        let same = overlapping_folder_warnings(&images, &[root("A", &outer), root("B", &outer)]);
        assert_eq!(same.len(), 1);
        assert!(same[0].contains("the same folder"), "got {:?}", same[0]);

        // A photo folder pointed at PastPerfect's own Images folder.
        let doubled = overlapping_folder_warnings(&images, &[root("Pictures", &images)]);
        assert_eq!(doubled.len(), 1);
        assert!(doubled[0].contains("already indexed"), "got {:?}", doubled[0]);

        // Folders that merely share a parent are fine, and so is a name that
        // begins with another's -- "Scans2" is not inside "Scans".
        let sibling = base.join("photos2");
        fs::create_dir_all(&sibling).unwrap();
        assert!(
            overlapping_folder_warnings(&images, &[root("A", &outer), root("B", &separate)]).is_empty(),
            "unrelated folders should not be reported"
        );
        assert!(
            overlapping_folder_warnings(&images, &[root("A", &outer), root("B", &sibling)]).is_empty(),
            "a folder whose name merely starts with another's is not inside it"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn the_identifier_images_are_attached_by_is_never_the_accession_number() {
        let record = |object: &str, accession: &str| {
            let mut r = HashMap::new();
            r.insert("OBJECTID".to_string(), object.to_string());
            r.insert("ACCESSNO".to_string(), accession.to_string());
            r
        };

        // The ordinary case: both are the object id.
        assert_eq!(record_ids(&record("2010.2.7", "2010.2")), ("2010.2.7".into(), "2010.2.7".into()));

        // No object id: the accession number is shown so the record is not
        // anonymous, but the key stays empty so nothing is attached to it.
        let (key, shown) = record_ids(&record("", "1900.15"));
        assert_eq!(shown, "1900.15", "the reader should still see something");
        assert_eq!(
            key, "",
            "an accession number must never become the key images are attached by -- \
             1900.15 is also another record's object id"
        );

        // Neither: nothing to show, nothing to attach.
        assert_eq!(record_ids(&record("", "")), (String::new(), String::new()));
        // And spacing around either is not significant.
        assert_eq!(record_ids(&record("  2010.2.7  ", "")).0, "2010.2.7");
        assert_eq!(record_ids(&record("", "  1900.15  ")).1, "1900.15");
    }

    /// Images are attached by object id, and nothing else may stand in for it.
    ///
    /// A record with no object id still shows its accession number so it is
    /// not left anonymous, but that number must never be used to look up
    /// pictures: accession numbers and object ids are not drawn from the same
    /// set, and in this collection 170 accession numbers are also some other
    /// record's object id. Using one as a stand-in would hang that other
    /// record's photographs on this one, and nothing on screen would look
    /// wrong.
    #[test]
    fn a_record_without_an_object_id_does_not_borrow_another_records_pictures() {
        let dir = std::env::temp_dir().join(format!("hs_idmix_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("001")).unwrap();
        fs::write(dir.join("001").join("belongs-to-someone-else.jpg"), b"x").unwrap();

        // MEDIA has pictures for the record whose object id is "1900.15".
        let mut media: MediaIndex = HashMap::new();
        media.insert(
            media_key("1900.15"),
            vec![MediaEntry {
                sort_key: 1.0,
                caption: "someone else's photograph".into(),
                imagefile: r"001\belongs-to-someone-else.jpg".into(),
                hires_ref: String::new(),
            }],
        );
        let ctx = IngestContext {
            images_dir: dir.clone(),
            thumb_cache_dir: dir.join("cache"),
            media,
            multimedia: MultimediaIndex::default(),
        };

        // That record finds its own pictures.
        assert_eq!(images_for_record("1900.15", None, &ctx).len(), 1,
            "the record the pictures belong to should still get them");

        // A record with no object id must not, even though a different
        // record's accession number happens to read the same.
        assert!(
            images_for_record("", None, &ctx).is_empty(),
            "a record with no object id picked up another record's photographs"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    fn write_test_jpeg(path: &Path, shade: u8) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::RgbImage::from_fn(16, 16, |x, _| image::Rgb([shade, x as u8, 0]))
            .save(path)
            .unwrap();
    }

    fn photo_titles(db: &Path) -> Vec<String> {
        let conn = Connection::open(db).unwrap();
        let mut stmt = conn.prepare("SELECT title FROM items ORDER BY title").unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn cached_jpgs(cache: &Path) -> usize {
        let mut n = 0;
        for sub in fs::read_dir(cache).into_iter().flatten().flatten() {
            n += fs::read_dir(sub.path()).into_iter().flatten().flatten().count();
        }
        n
    }

    /// Four PDFs of the kinds a society really has: one with text, one inside
    /// a subfolder, a scan that is only pictures, and a file that is not a PDF
    /// at all despite its name.
    fn write_sample_pdfs(root: &Path) {
        fs::create_dir_all(root.join("Minutes")).unwrap();
        fs::write(
            root.join("Mill Ledger.pdf"),
            crate::pdf::fixture::make_pdf(&["Ledger of the Mill Company 1887", "Accounts of the Hartwell family"]),
        )
        .unwrap();
        fs::write(
            root.join("Minutes").join("minutes_1920.pdf"),
            crate::pdf::fixture::make_pdf(&["Meeting called to order regarding the new bridge"]),
        )
        .unwrap();
        fs::write(root.join("scanned page.pdf"), crate::pdf::fixture::make_pdf(&[""])).unwrap();
        fs::write(root.join("damaged.pdf"), "this is not a pdf").unwrap();
    }

    fn fts_titles(db: &Path, query: &str) -> Vec<String> {
        let conn = Connection::open(db).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT items.title FROM items_fts JOIN items ON items.id = items_fts.rowid
                 WHERE items_fts MATCH ?1 ORDER BY items.title",
            )
            .unwrap();
        stmt.query_map([query], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn pdfs_are_found_by_their_contents_and_unreadable_ones_by_name() {
        let dir = std::env::temp_dir().join(format!("hs_pdfs_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let docs = dir.join("docs");
        write_sample_pdfs(&docs);
        let (db, cache) = (dir.join("index.db"), dir.join("cache"));
        let empty = PathBuf::new();
        let roots = [DocumentRoot { label: "Society PDFs".into(), path: docs.clone() }];

        let result = build(&empty, &db, &empty, &empty, &[], &DocumentSettings::new(&roots), &cache, None).unwrap();
        assert_eq!(result.total_records, 4, "every PDF is listed, readable or not");

        // Words from inside a document, including its second page.
        assert_eq!(fts_titles(&db, "\"hartwell\"*"), vec!["Mill Ledger"]);
        assert_eq!(fts_titles(&db, "\"bridge\"*"), vec!["minutes 1920"]);
        // By file name and by folder, for the ones with no words inside.
        assert_eq!(fts_titles(&db, "\"scanned\"*"), vec!["scanned page"]);
        assert_eq!(fts_titles(&db, "\"damaged\"*"), vec!["damaged"]);
        assert!(fts_titles(&db, "\"minutes\"*").contains(&"minutes 1920".to_string()));

        let conn = Connection::open(&db).unwrap();
        let facts: Vec<(String, i64, i64)> = conn
            .prepare(
                "SELECT items.title, documents.pages, documents.has_text
                 FROM documents JOIN items ON items.id = documents.item_id ORDER BY items.title",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            facts,
            vec![
                ("Mill Ledger".to_string(), 2, 1),
                ("damaged".to_string(), 0, 0),
                ("minutes 1920".to_string(), 1, 1),
                ("scanned page".to_string(), 1, 0),
            ]
        );

        // Said out loud, not left to look like "nothing matched".
        let said = result.warnings.join("\n");
        assert!(said.contains("1 PDF has no searchable text") && said.contains("scanned page.pdf") && said.contains("turned off"), "got: {said}");
        assert!(said.contains("text of 1 PDF could not be read") && said.contains("damaged.pdf"), "got: {said}");
        assert!(result.warnings.len() == 2, "got: {:?}", result.warnings);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A .docx holding one paragraph per entry of `paragraphs`.
    fn make_docx(paragraphs: &[&str]) -> Vec<u8> {
        use std::io::Write;
        let body: String =
            paragraphs.iter().map(|p| format!("<w:p><w:r><w:t>{p}</w:t></w:r></w:p>")).collect();
        let xml = format!(
            "<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body>{body}</w:body></w:document>"
        );
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(xml.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn word_and_text_files_are_found_by_their_words() {
        let dir = std::env::temp_dir().join(format!("hs_words_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let docs = dir.join("docs");
        fs::create_dir_all(&docs).unwrap();
        fs::write(docs.join("Board Minutes.docx"), make_docx(&["Meeting of the Hartwell trustees", "Bridge fund approved"])).unwrap();
        fs::write(docs.join("Oral history notes.txt"), "Interview about the Tannery on Mill Road").unwrap();
        fs::write(docs.join("Register.md"), "# Village School Register\n\nPupils named **Beattie** and Dougherty").unwrap();
        fs::write(docs.join("blank.docx"), make_docx(&[""])).unwrap();
        fs::write(docs.join("broken.docx"), "not a zip").unwrap();
        fs::write(docs.join("~$Board Minutes.docx"), "Word's lock file").unwrap();
        fs::write(docs.join("old format.doc"), "ignored").unwrap();
        let (db, cache) = (dir.join("index.db"), dir.join("cache"));
        let empty = PathBuf::new();
        let roots = [DocumentRoot { label: "Society files".into(), path: docs.clone() }];

        let result = build(&empty, &db, &empty, &empty, &[], &DocumentSettings::new(&roots), &cache, None).unwrap();
        assert_eq!(result.total_records, 5, "three readable, a blank and a broken one; no lock file, no .doc");

        assert_eq!(fts_titles(&db, "\"hartwell\"*"), vec!["Board Minutes"]);
        assert_eq!(fts_titles(&db, "\"bridge\"*"), vec!["Board Minutes"]);
        assert_eq!(fts_titles(&db, "\"tannery\"*"), vec!["Oral history notes"]);
        assert_eq!(fts_titles(&db, "\"dougherty\"*"), vec!["Register"]);
        assert_eq!(fts_titles(&db, "\"blank\"*"), vec!["blank"]);
        assert_eq!(fts_titles(&db, "\"broken\"*"), vec!["broken"]);

        let conn = Connection::open(&db).unwrap();
        let has_text: Vec<(String, i64)> = conn
            .prepare("SELECT items.title, documents.has_text FROM documents JOIN items ON items.id = documents.item_id ORDER BY items.title")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            has_text,
            vec![("Board Minutes".into(), 1), ("Oral history notes".into(), 1), ("Register".into(), 1), ("blank".into(), 0), ("broken".into(), 0)]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn taking_a_pdf_folder_out_removes_its_documents_on_the_next_rebuild() {
        let dir = std::env::temp_dir().join(format!("hs_pdfrm_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let docs = dir.join("docs");
        write_sample_pdfs(&docs);
        let (db, cache) = (dir.join("index.db"), dir.join("cache"));
        let empty = PathBuf::new();
        let roots = [DocumentRoot { label: "Society PDFs".into(), path: docs }];
        assert_eq!(build(&empty, &db, &empty, &empty, &[], &DocumentSettings::new(&roots), &cache, None).unwrap().total_records, 4);

        // A rebuild with the folder gone from the settings. Another folder
        // keeps the settings valid, as it would for a real person.
        let other = dir.join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("note.pdf"), crate::pdf::fixture::make_pdf(&["Only this one remains"])).unwrap();
        let roots = [DocumentRoot { label: "Other".into(), path: other }];
        assert_eq!(build(&empty, &db, &empty, &empty, &[], &DocumentSettings::new(&roots), &cache, None).unwrap().total_records, 1);
        assert!(fts_titles(&db, "\"hartwell\"*").is_empty());
        let conn = Connection::open(&db).unwrap();
        let documents: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)).unwrap();
        assert_eq!(documents, 1, "the document rows go with the folder");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn taking_a_photo_folder_out_removes_its_pictures_and_previews_on_the_next_rebuild() {
        let dir = std::env::temp_dir().join(format!("hs_rmroot_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (kept, dropped) = (dir.join("kept"), dir.join("dropped"));
        write_test_jpeg(&kept.join("keep one.jpg"), 10);
        write_test_jpeg(&dropped.join("drop one.jpg"), 90);
        write_test_jpeg(&dropped.join("drop two.jpg"), 170);
        let roots = |names: &[(&str, &PathBuf)]| -> Vec<PhotoArchiveRoot> {
            names.iter().map(|(l, p)| PhotoArchiveRoot { label: l.to_string(), path: (*p).clone() }).collect()
        };
        let (db, cache) = (dir.join("index.db"), dir.join("cache"));
        let empty = PathBuf::new();
        let run = |roots: &[PhotoArchiveRoot]| {
            build(&empty, &db, &empty, &empty, roots, &DocumentSettings::new(&[]), &cache, None).unwrap()
        };

        let both = run(&roots(&[("Kept", &kept), ("Dropped", &dropped)]));
        assert_eq!(both.total_records, 3);
        assert_eq!(photo_titles(&db).len(), 3);
        assert_eq!(cached_jpgs(&cache), 3);

        // An unreadable folder is not the same as a removed one: nothing is
        // pruned, so the previews are still there when the share comes back.
        let gone = dir.join("share-offline");
        let offline = run(&roots(&[("Kept", &kept), ("Dropped", &gone)]));
        assert!(!offline.warnings.is_empty());
        assert_eq!(cached_jpgs(&cache), 3, "an offline folder must not cost its previews");

        // Removed from the settings: gone from the index, previews pruned.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let only_kept = run(&roots(&[("Kept", &kept)]));
        assert_eq!(only_kept.total_records, 1);
        let titles = photo_titles(&db);
        assert_eq!(titles.len(), 1);
        assert!(titles[0].contains("keep one"), "{titles:?}");
        assert_eq!(cached_jpgs(&cache), 1, "the removed folder's previews should be cleaned up");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pruning_keeps_recent_files_and_refuses_to_run_on_an_empty_index() {
        let dir = std::env::temp_dir().join(format!("hs_prune_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cache = dir.join("cache");
        fs::create_dir_all(cache.join("ab")).unwrap();
        let (used, stray, other) = (cache.join("ab/used.jpg"), cache.join("ab/stray.jpg"), cache.join("ab/notes.txt"));
        for f in [&used, &stray, &other] {
            fs::write(f, b"x").unwrap();
        }
        let db = dir.join("index.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE items (thumb_path TEXT); CREATE TABLE item_images (thumb_path TEXT);",
        )
        .unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);

        // No previews listed at all: could be a failed read, so delete nothing.
        assert_eq!(prune_thumb_cache(&db, &cache, later), (0, 0));
        assert!(stray.exists());

        conn.execute("INSERT INTO items VALUES (?1)", [used.to_string_lossy().to_string()]).unwrap();
        // Written after the rebuild began: kept, as an on-demand preview would be.
        assert_eq!(prune_thumb_cache(&db, &cache, std::time::SystemTime::UNIX_EPOCH), (0, 0));
        assert!(stray.exists());

        assert_eq!(prune_thumb_cache(&db, &cache, later).0, 1);
        assert!(used.exists() && other.exists() && !stray.exists());

        let _ = fs::remove_dir_all(&dir);
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



    fn support(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support").join(name)
    }

    /// One workbook named directly, and a folder holding another: every row
    /// becomes a record, found by what is in its cells, with its own headings.
    #[test]
    fn each_row_of_a_workbook_becomes_a_record() {
        let dir = std::env::temp_dir().join(format!("hs_sheetrows_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let folder = dir.join("old");
        fs::create_dir_all(&folder).unwrap();
        fs::copy(support("donors.xls"), folder.join("donors.xls")).unwrap();
        // An open workbook's lock file must not be taken for a workbook.
        fs::write(folder.join("~$donors.xls"), b"lock").unwrap();
        let db = dir.join("index.db");
        let empty = PathBuf::new();
        let sheets = [
            SpreadsheetRoot { label: "Cemetery".into(), path: support("register.xlsx") },
            SpreadsheetRoot { label: "Donors".into(), path: folder },
        ];
        let settings = DocumentSettings { spreadsheets: &sheets, ..DocumentSettings::new(&[]) };
        let result = build(&empty, &db, &empty, &empty, &[], &settings, &dir.join("cache"), None).unwrap();
        // 3 cemetery rows + 1 veteran + 2 donors; the hidden sheet and the
        // blank row are not records.
        assert_eq!(result.total_records, 6, "warnings: {:?}", result.warnings);

        let conn = Connection::open(&db).unwrap();
        let (catalog, accessno, title, detail): (String, String, String, String) = conn
            .query_row(
                "SELECT catalog, accessno, title, detail_json FROM items WHERE title = 'Thaddeus Marsh'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((catalog.as_str(), accessno.as_str()), ("Cemetery", "1987.1.2"));
        assert_eq!(title, "Thaddeus Marsh");
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        assert_eq!(detail["Plot"], "A-13");
        assert_eq!(detail["ROW"], "4");
        assert_eq!(detail["SHEET"], "Register");

        // Found by a word in any cell, by the file's title line, and by the
        // label -- but not by a column heading, which every row has.
        let found = |q: &str| -> i64 {
            conn.query_row("SELECT count(*) FROM items_fts WHERE items_fts MATCH ?1", [q], |r| r.get(0)).unwrap()
        };
        assert_eq!(found("orchard"), 1);
        assert_eq!(found("quilt"), 1);
        assert_eq!(found("1987"), 3, "every row under the 1987 title line, and no row of another sheet");
        assert_eq!(found("plot"), 0, "a heading is not a word in the row");
        // Without a heading that names the row, its first filled cell does.
        let donor: String =
            conn.query_row("SELECT title FROM items WHERE catalog = 'Donors' ORDER BY id LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(donor, "Mrs. Annabel Quint");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_workbook_that_cannot_be_read_is_reported_and_the_rest_still_indexed() {
        let dir = std::env::temp_dir().join(format!("hs_sheetbad_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("broken.xlsx"), b"not a workbook").unwrap();
        fs::copy(support("donors.xls"), dir.join("donors.xls")).unwrap();
        let db = dir.join("index.db");
        let empty = PathBuf::new();
        let sheets = [SpreadsheetRoot { label: "Mixed".into(), path: dir.clone() }];
        let settings = DocumentSettings { spreadsheets: &sheets, ..DocumentSettings::new(&[]) };
        let result = build(&empty, &db, &empty, &empty, &[], &settings, &dir.join("cache"), None).unwrap();
        assert_eq!(result.total_records, 2);
        assert!(
            result.warnings.iter().any(|w| w.contains("broken.xlsx") && w.contains("could not be read")),
            "got {:?}",
            result.warnings
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_spreadsheet_that_is_gone_is_a_warning_not_a_silent_loss() {
        let dir = std::env::temp_dir().join(format!("hs_sheetgone_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let empty = PathBuf::new();
        let sheets = [SpreadsheetRoot { label: "Lost".into(), path: dir.join("gone.xlsx") }];
        let settings = DocumentSettings { spreadsheets: &sheets, ..DocumentSettings::new(&[]) };
        let result =
            build(&empty, &dir.join("i.db"), &empty, &empty, &[], &settings, &dir.join("cache"), None).unwrap();
        assert_eq!(result.total_records, 0);
        assert!(result.warnings.iter().any(|w| w.contains("could not be found")), "got {:?}", result.warnings);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn counting_workbooks_for_the_settings_check() {
        assert_eq!(count_spreadsheet_files(&support("register.xlsx")), 1);
        assert_eq!(count_spreadsheet_files(&support("scan_g4.pdf")), 0, "a PDF is not a workbook");
        assert_eq!(count_spreadsheet_files(&support("")), 2, "the folder holds two");
    }

    #[test]
    fn the_companion_files_a_mac_leaves_on_other_drives_are_not_collected() {
        let dir = std::env::temp_dir().join(format!("hs_appledouble_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let sub = dir.join("Directory 1994");
        fs::create_dir_all(&sub).unwrap();
        // The stub begins with the AppleDouble marker, under the real file's
        // name.
        fs::write(sub.join("Directory 1994.pdf"), b"%PDF-1.3\n").unwrap();
        fs::write(sub.join("._Directory 1994.pdf"), [0x00, 0x05, 0x16, 0x07, 0, 2, 0, 0]).unwrap();
        fs::write(sub.join("page.jpg"), b"x").unwrap();
        fs::write(sub.join("._page.jpg"), b"x").unwrap();

        let mut pdfs = Vec::new();
        collect_files(&dir, &dir, DOCUMENT_EXTENSIONS, &mut pdfs);
        let names: Vec<_> = pdfs.iter().map(|(_, name, _)| name.as_str()).collect();
        assert_eq!(names, ["Directory 1994.pdf"]);

        let mut pictures = Vec::new();
        collect_image_files(&dir, &dir, &mut pictures);
        let names: Vec<_> = pictures.iter().map(|(_, name, _)| name.as_str()).collect();
        assert_eq!(names, ["page.jpg"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
