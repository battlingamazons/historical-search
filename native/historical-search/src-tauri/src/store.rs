//! Reads from the generated search index, and the on-disk config.
//! This is what the UI talks to (via Tauri commands) -- there is no local
//! web server and no HTTP involved anywhere.

use crate::app_paths;
use crate::ingest::field_labels;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_PAGE_SIZE: i64 = 40;
/// Safety cap for "show everything" -- still far more than one page.
pub const MAX_ALL_RESULTS: i64 = 2000;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PhotoRootConfig {
    pub label: String,
    pub path: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub data_dir: String,
    #[serde(default)]
    pub images_dir: String,
    /// PastPerfect's "Multimedia" folder, which holds the high-resolution
    /// masters behind the downsized copies in Images. Optional: blank means
    /// the folder named "Multimedia" beside the Data folder.
    #[serde(default)]
    pub multimedia_dir: String,
    #[serde(default)]
    pub photo_archive_roots: Vec<PhotoRootConfig>,
    /// Folders of PDFs to search by their text. Same shape as a photo folder:
    /// a label to show, and where it is.
    #[serde(default)]
    pub document_roots: Vec<PhotoRootConfig>,
    /// Excel workbooks, or folders of them, whose rows are searched. The path
    /// is a file or a folder.
    #[serde(default)]
    pub spreadsheet_roots: Vec<PhotoRootConfig>,
    /// Read the words off the page pictures of scanned PDFs. On unless turned
    /// off: a scan with no recognition is not searchable by what it says,
    /// which is most of why the PDFs are here. It is slow the first time
    /// (about a second a page, once per file), hence the way to stop it.
    #[serde(default = "default_true")]
    pub recognize_scanned_pdfs: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Config {
            data_dir: String::new(),
            images_dir: String::new(),
            multimedia_dir: String::new(),
            photo_archive_roots: Vec::new(),
            document_roots: Vec::new(),
            spreadsheet_roots: Vec::new(),
            recognize_scanned_pdfs: true,
        }
    }
}

/// Cleans a folder path the way people actually supply them.
///
/// Two things arrive attached to pasted paths. Stray whitespace, which was
/// already half-handled -- the checks for "did you enter anything" trimmed,
/// while the path actually used did not, so a trailing space produced a
/// confident "Folder not found" for a folder that plainly exists. And quotes:
/// the ordinary way to get a path on Windows is Shift+right-click, "Copy as
/// path", which yields `"C:\pp5\Data"` with the quotation marks included.
/// Taken literally that names no folder at all, and nothing on screen would
/// suggest the quotes were the problem.
pub fn clean_path_input(raw: &str) -> String {
    let trimmed = raw.trim();
    // Only when balanced, so a path legitimately containing a quote is left
    // alone rather than mangled.
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| trimmed.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(trimmed);
    unquoted.trim().to_string()
}

/// Parses config.json text. Windows PowerShell and some editors save UTF-8
/// with a leading byte-order mark, which serde_json rejects as "expected
/// value at line 1 column 1" -- a file that looks fine to the person who
/// edited it, so the mark is dropped here rather than reported as damage.
fn parse_config(text: &str) -> Result<Config, serde_json::Error> {
    serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text))
}

pub fn load_config() -> Config {
    load_config_checked().0
}

/// Returns the config plus a message if the stored file existed but could
/// not be parsed. A corrupt config used to silently read as "no folders
/// configured", which looks identical to the user having cleared their
/// settings -- so it has to be reported, not quietly defaulted away.
pub fn load_config_checked() -> (Config, Option<String>) {
    let path = app_paths::config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match parse_config(&text) {
            // Cleaned once, here, so everything downstream -- rebuilding the
            // index, resolving the Images and Multimedia folders, reporting
            // in diagnostics -- works from the same tidied paths rather than
            // each doing its own partial trimming.
            Ok(mut config) => {
                config.data_dir = clean_path_input(&config.data_dir);
                config.images_dir = clean_path_input(&config.images_dir);
                config.multimedia_dir = clean_path_input(&config.multimedia_dir);
                for root in config
                    .photo_archive_roots
                    .iter_mut()
                    .chain(config.document_roots.iter_mut())
                    .chain(config.spreadsheet_roots.iter_mut()) {
                    root.path = clean_path_input(&root.path);
                    root.label = root.label.trim().to_string();
                }
                (config, None)
            }
            Err(e) => (
                Config::default(),
                Some(format!(
                    "Your saved folder settings could not be read ({e}). They have been left \
                     on disk untouched -- reopen Folder Settings to set them again."
                )),
            ),
        },
        Err(_) => (Config::default(), None), // no config yet: first run
    }
}

pub fn save_config(config: &Config) -> Result<(), String> {
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    // Write via a temp file and rename, so an interruption can't leave a
    // truncated config.json -- that would read as "no folders configured"
    // and lose every setting.
    let path = app_paths::config_path();
    // Unique per writer, so two copies of the application saving settings at
    // the same moment cannot interleave into one temporary file.
    let temp = path.with_extension(format!("json.partial.{}", app_paths::unique_temp_suffix()));
    std::fs::write(&temp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        e.to_string()
    })
}

#[derive(Serialize)]
pub struct SearchResult {
    pub id: i64,
    pub catalog: String,
    pub accessno: String,
    pub title: String,
    pub snippet: String,
    pub has_image: bool,
    /// A PDF rather than a catalogue record or a photograph. Marked so the
    /// window can label it as one -- it has no picture to tell it apart by.
    pub is_document: bool,
    /// This record's catalog number is exactly what was typed. Marked so the
    /// UI can say so outright -- being first is no help if the person can't
    /// tell it apart from the near-misses under it.
    pub is_exact_id: bool,
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub has_more: bool,
    /// Total number of records matching the query, not just the number
    /// loaded so far -- the UI shows real positions ("12 of 247") rather
    /// than a running "40+" that jumps as more pages load.
    pub total: i64,
    /// What was looked for, as typed, for marking it in the results; and the
    /// same as lower-case words. Words that were only excluded are in neither.
    pub terms: Vec<String>,
    pub words: Vec<String>,
    /// How many records of the whole search each dataset (catalog, photo
    /// folder, PDF folder, spreadsheet) holds, whatever datasets are chosen --
    /// so the window can offer the others. Only sent with the first page;
    /// empty after that.
    pub datasets: Vec<DatasetCount>,
}

#[derive(Serialize)]
pub struct DatasetCount {
    pub name: String,
    pub count: i64,
}

#[derive(Serialize)]
pub struct LabeledField {
    pub label: String,
    pub value: String,
}

/// One image on a detail card. `id` addresses this specific image (not the
/// item), so the UI can request its thumbnail and save its master without
/// ever handling a real filesystem path.
#[derive(Serialize)]
pub struct ItemImage {
    pub id: i64,
    /// Which `hsimg://` form addresses this image -- "imgthumb" for a row in
    /// item_images, "thumb" for an item's own leading image on an index built
    /// before that table existed. `id` is numbered in whichever space `kind`
    /// names, so the two always travel together.
    pub kind: String,
    pub caption: String,
    pub has_thumb: bool,
    /// Whether a high-resolution master exists behind this image, as opposed
    /// to only PastPerfect's downsized display copy. The download offer says
    /// which of the two the user is about to get.
    pub has_master: bool,
}

#[derive(Serialize)]
pub struct ItemDetail {
    pub id: i64,
    pub catalog: String,
    pub accessno: String,
    pub title: String,
    pub images: Vec<ItemImage>,
    pub fields: Vec<LabeledField>,
    /// Set for a PDF.
    pub document: Option<DocumentInfo>,
}

/// Turns what was typed into an FTS5 query (see `query` for what is understood).
#[cfg(test)]
fn fts_query(raw: &str) -> String {
    crate::query::parse(raw).match_expr
}

/// Punctuation- and case-insensitive form of a catalog number, so that
/// "2010.19.18", "2010-19-18" and "2010 19 18" all match the same record --
/// people copy these off labels, boxes and emails with whatever separator
/// happens to be there.
pub fn normalize_catalog_number(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Whether a query is shaped like a catalog number rather than a word: some
/// digits, and one of the separators these numbers are written with.
///
/// This gates only the *prefix* boost. Without it, typing "19" while looking
/// for "19th century" would promote every accession beginning 19 above the
/// records that actually talk about it. A query like "2010.19" is unambiguous
/// in a way that a bare "2010" is not.
fn looks_like_catalog_number(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.chars().any(|c| c.is_ascii_digit())
        && trimmed.chars().any(|c| matches!(c, '.' | '-' | '/' | '_'))
}

fn open_db() -> Option<Connection> {
    let path = app_paths::db_path();
    if !path.exists() {
        // Deliberately not an error. On a newly installed copy there is no
        // index until the first rebuild, and the status display asks for the
        // record count as soon as the window opens -- so a first run would
        // otherwise hand back a diagnostic report full of ERROR lines for a
        // situation that is entirely expected, and send anyone reading it
        // looking for a fault that is not there.
        crate::diagnostics::info_once(
            "index-missing",
            "search",
            format!("no index yet at {} -- nothing has been built", path.display()),
        );
        return None;
    }
    // Both of these used to fail into a plain "no results", which looks
    // exactly like an empty collection.
    let conn = match Connection::open(&path) {
        Ok(conn) => conn,
        Err(e) => {
            crate::diagnostics::error_repeating(
                "index-unopenable",
                "search",
                format!("the index at {} could not be opened: {e}", path.display()),
            );
            return None;
        }
    };
    if let Err(e) = register_sql_functions(&conn) {
        crate::diagnostics::error_repeating(
            "sql-function-failed",
            "search",
            format!("could not set up catalog-number matching: {e}"),
        );
        return None;
    }
    Some(conn)
}

/// Exposes `normalize_catalog_number` to SQL, so the comparison that ranks
/// results uses the very same normalisation the Rust side does. Writing it
/// twice -- once here, once as nested REPLACE() calls -- is how the two
/// quietly drift apart.
fn register_sql_functions(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "catalog_norm",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let raw: Option<String> = ctx.get(0)?;
            Ok(normalize_catalog_number(raw.as_deref().unwrap_or("")))
        },
    )
}

pub fn search(query: &str, offset: i64, show_all: bool) -> SearchResponse {
    search_in(query, offset, show_all, &[])
}

/// The same, narrowed to the named datasets (none named means all of them).
pub fn search_in(query: &str, offset: i64, show_all: bool, datasets: &[String]) -> SearchResponse {
    let empty = SearchResponse { results: Vec::new(), has_more: false, total: 0, terms: Vec::new(), words: Vec::new(), datasets: Vec::new() };
    if query.trim().is_empty() {
        return empty;
    }
    let Some(conn) = open_db() else { return empty };
    search_with(&conn, query, offset, show_all, datasets)
}

fn search_with(conn: &Connection, query: &str, offset: i64, show_all: bool, datasets: &[String]) -> SearchResponse {
    let empty = SearchResponse { results: Vec::new(), has_more: false, total: 0, terms: Vec::new(), words: Vec::new(), datasets: Vec::new() };
    let limit = if show_all { MAX_ALL_RESULTS } else { DEFAULT_PAGE_SIZE };
    let parsed = crate::query::parse(query);
    let match_expr = parsed.match_expr.clone();
    if match_expr.is_empty() {
        return empty;
    }

    // The chosen datasets as a JSON list, which SQL can test membership of
    // without building a query of a different shape for every choice.
    let chosen = serde_json::to_string(datasets).unwrap_or_else(|_| "[]".into());
    let narrowed = !datasets.is_empty();

    let total: i64 = if narrowed {
        conn.query_row(
            "SELECT COUNT(*) FROM items_fts JOIN items ON items.id = items_fts.rowid
             WHERE items_fts MATCH ?1 AND items.catalog IN (SELECT value FROM json_each(?2))",
            rusqlite::params![match_expr, chosen],
            |row| row.get(0),
        )
    } else {
        conn.query_row(
            "SELECT COUNT(*) FROM items_fts WHERE items_fts MATCH ?1",
            [&match_expr],
            |row| row.get(0),
        )
    }
    .unwrap_or(0);

    // Counted over the whole search, not the narrowed one: a dataset that is
    // not chosen still needs its number beside it to be worth choosing.
    let mut dataset_counts = Vec::new();
    if offset == 0 {
        if let Ok(mut stmt) = conn.prepare(
            "SELECT items.catalog, COUNT(*) FROM items_fts JOIN items ON items.id = items_fts.rowid
             WHERE items_fts MATCH ?1 GROUP BY items.catalog ORDER BY COUNT(*) DESC, items.catalog",
        ) {
            if let Ok(rows) = stmt.query_map([&match_expr], |row| {
                Ok(DatasetCount { name: row.get::<_, Option<String>>(0)?.unwrap_or_default(), count: row.get(1)? })
            }) {
                dataset_counts = rows.flatten().collect();
            }
        }
    }

    // One extra row tells us whether a next page exists, without a separate
    // (potentially slow for a broad match) COUNT(*).
    // Someone typing a catalog number wants *that* record, not the 230 others
    // that happen to contain the same numbers somewhere. Left to bm25 alone,
    // "2010.19.18" tokenises to 2010/19/18 and the record it names lands
    // around 150th -- so exact matches are pulled to the front, then the rest
    // of the lot, then ordinary relevance.
    let normalized_query = normalize_catalog_number(query);
    let prefix_boost = if looks_like_catalog_number(query) { 1 } else { 0 };
    let sql = "SELECT items.id, items.catalog, items.accessno, items.title, items.body,
                      items.thumb_path,
                      CASE
                        WHEN ?4 != '' AND catalog_norm(items.accessno) = ?4 THEN 0
                        WHEN ?4 != '' AND ?5 = 1
                             AND catalog_norm(items.accessno) LIKE ?4 || '%' THEN 1
                        ELSE 2
                      END AS id_rank
               FROM items_fts
               JOIN items ON items.id = items_fts.rowid
               WHERE items_fts MATCH ?1
                 AND (?6 = 0 OR items.catalog IN (SELECT value FROM json_each(?7)))
               -- items.id last purely to make the order total. Relevance
               -- scores tie in large groups here (a common word matches
               -- tens of thousands of records across a few hundred distinct scores, over a thousand
               -- of them sharing one), so almost every page boundary falls
               -- inside a tie. Within a tie SQLite may order however it
               -- likes, and each page is a separate query -- so without this
               -- 'Show more results' is free to repeat records and skip
               -- others. It happens not to today; this stops it depending on
               -- that.
               ORDER BY id_rank, bm25(items_fts), items.id
               LIMIT ?2 OFFSET ?3";
    let mut stmt = match conn.prepare(sql) {
        Ok(stmt) => stmt,
        Err(e) => {
            // An index built by an older version can lack a column this query
            // names, which would otherwise read as "nothing matches anything".
            crate::diagnostics::error_repeating(
                "search-query-failed",
                "search",
                format!("the search query could not be prepared: {e} -- the index may need rebuilding"),
            );
            return empty;
        }
    };
    let rows = stmt.query_map(
        rusqlite::params![match_expr, limit + 1, offset, normalized_query, prefix_boost, narrowed as i64, chosen],
        |row| {
        let body: Option<String> = row.get(4)?;
        let thumb_path: Option<String> = row.get(5)?;
        let id_rank: i64 = row.get(6)?;
        let snippet = body.unwrap_or_default().chars().take(220).collect::<String>();
        Ok(SearchResult {
            id: row.get(0)?,
            catalog: row.get(1)?,
            accessno: row.get(2)?,
            title: row.get(3)?,
            snippet,
            // Must reflect the *thumbnail*, not the original: the results
            // list loads `hsimg://.../thumb-<id>`, so an item whose image
            // exists but failed to decode would otherwise be rendered as a
            // broken image icon. (get_item already keys off thumb_path;
            // these two must agree.)
            has_image: thumb_path.is_some(),
            is_document: false,
            is_exact_id: id_rank == 0,
        })
    },
    );
    let Ok(rows) = rows else { return empty };
    let mut results: Vec<SearchResult> = rows.filter_map(|r| r.ok()).collect();
    let has_more = results.len() as i64 > limit;
    results.truncate(limit as usize);
    mark_documents(conn, &match_expr, &mut results);
    SearchResponse { results, has_more, total, terms: parsed.terms, words: parsed.words, datasets: dataset_counts }
}

/// Whether this index was built with the table of PDFs. One built by an
/// earlier version has none, and must go on working.
fn index_has_documents(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'documents'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

/// Flags the PDFs among a page of results and shows them the passage that
/// matched, not the start of the document.
///
/// For a catalogue record the first part of its description is what the
/// person wants to see. For a forty-page PDF the first 220 characters are a
/// title page, which says nothing about why it matched -- so these, and only
/// these, get a passage chosen by the search itself. Done per result on the
/// page being shown rather than inside the search query: computing a passage
/// for every match of a common word would mean reading every matching
/// document in full before showing the first.
fn mark_documents(conn: &Connection, match_expr: &str, results: &mut [SearchResult]) {
    if !index_has_documents(conn) {
        return;
    }
    for r in results.iter_mut() {
        let is_document = conn
            .query_row("SELECT 1 FROM documents WHERE item_id = ?1", [r.id], |_| Ok(()))
            .is_ok();
        if !is_document {
            continue;
        }
        r.is_document = true;
        let passage: Option<String> = conn
            .query_row(
                "SELECT snippet(items_fts, 1, '', '', ' … ', 30)
                 FROM items_fts WHERE items_fts MATCH ?1 AND rowid = ?2",
                rusqlite::params![match_expr, r.id],
                |row| row.get(0),
            )
            .ok();
        // The window adds its own ellipsis after a snippet, so one left on the
        // end by the search would show as two.
        let passage = passage.map(|p| p.trim_end_matches(|c: char| c == '…' || c.is_whitespace()).to_string());
        if let Some(passage) = passage.filter(|p| !p.is_empty()) {
            r.snippet = passage;
        }
    }
}

/// A page of a PDF that a search matched.
#[derive(Serialize)]
pub struct PageHit {
    pub page: i64,
    /// How many of the typed words are on this page.
    pub words: i64,
    /// A few words around the first match.
    pub snippet: String,
}

/// The most pages listed for one PDF. A common word is on nearly every page
/// of a long book; the best few are what a person can use.
const MAX_PAGE_HITS: usize = 40;

/// The pages of one PDF that match what was typed, best first.
///
/// "Best" is the pages with the most of the typed words, then those with the
/// most occurrences, then the earliest. Words match from their beginning --
/// "stew" finds "Stewart" -- as they do in the search itself. Done here, on
/// the one document being looked at, rather than in the search: the search
/// only needs to know which documents match.
pub fn find_pages(item_id: i64, query: &str) -> Vec<PageHit> {
    let mut terms = crate::query::parse(query).words;
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        return Vec::new();
    }
    let Some(conn) = open_db() else { return Vec::new() };
    pages_matching(&conn, item_id, &terms)
}

fn pages_matching(conn: &Connection, item_id: i64, terms: &[String]) -> Vec<PageHit> {
    let table_exists = conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'document_pages'", [], |_| Ok(()))
        .is_ok();
    if !table_exists {
        return Vec::new();
    }
    let Ok(mut stmt) = conn.prepare("SELECT page, text FROM document_pages WHERE item_id = ?1 ORDER BY page") else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([item_id], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))) else {
        return Vec::new();
    };

    // (distinct words, occurrences, page, snippet)
    let mut hits: Vec<(i64, i64, i64, String)> = Vec::new();
    for (page, text) in rows.flatten() {
        let mut matched = vec![false; terms.len()];
        let mut occurrences = 0i64;
        let mut first: Option<usize> = None;
        let mut start = None;
        let mut visit = |from: usize, to: usize| {
            let word = text[from..to].to_lowercase();
            for (i, term) in terms.iter().enumerate() {
                if word.starts_with(term.as_str()) {
                    matched[i] = true;
                    occurrences += 1;
                    first.get_or_insert(from);
                    break;
                }
            }
        };
        for (i, ch) in text.char_indices() {
            if ch.is_alphanumeric() {
                start.get_or_insert(i);
            } else if let Some(from) = start.take() {
                visit(from, i);
            }
        }
        if let Some(from) = start {
            visit(from, text.len());
        }
        let words = matched.iter().filter(|m| **m).count() as i64;
        if words == 0 {
            continue;
        }
        let at = first.unwrap_or(0);
        // A few words either side of the first match, cut at word boundaries
        // so the passage does not begin or end half-way through a word.
        let preceding: Vec<char> = text[..at].chars().collect();
        let following: Vec<char> = text[at..].chars().collect();
        let (cut_before, cut_after) = (preceding.len() > 60, following.len() > 110);
        let mut before: String = preceding[preceding.len().saturating_sub(60)..].iter().collect();
        if cut_before {
            if let Some(i) = before.find(|c: char| !c.is_alphanumeric()) {
                before = before[i..].to_string();
            }
        }
        let mut after: String = following.iter().take(110).collect();
        if cut_after {
            if let Some(i) = after.rfind(|c: char| !c.is_alphanumeric()) {
                after.truncate(i);
            }
        }
        let snippet = format!(
            "{}{}{}{}",
            if cut_before { "… " } else { "" },
            before.trim_start(),
            after.trim_end(),
            if cut_after { " …" } else { "" }
        );
        hits.push((words, occurrences, page, snippet));
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    hits.truncate(MAX_PAGE_HITS);
    hits.into_iter().map(|(words, _, page, snippet)| PageHit { page, words, snippet }).collect()
}

/// What the window shows about a PDF, and the file it opens.
#[derive(Serialize)]
pub struct DocumentInfo {
    pub file_name: String,
    pub pages: i64,
    pub has_text: bool,
    /// The file's size in bytes now, or 0 if it is not where it was -- the
    /// viewer needs it to ask for the file a piece at a time.
    pub size: u64,
}

fn document_info(conn: &Connection, item_id: i64) -> Option<DocumentInfo> {
    if !index_has_documents(conn) {
        return None;
    }
    conn.query_row(
        "SELECT path, pages, has_text FROM documents WHERE item_id = ?1",
        [item_id],
        |row| {
            let path: String = row.get(0)?;
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            Ok(DocumentInfo {
                file_name: Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or(path),
                pages: row.get(1)?,
                has_text: row.get::<_, i64>(2)? != 0,
                size,
            })
        },
    )
    .ok()
}

/// Where a PDF is, by the id the window knows it by. The window never holds
/// a path, so it can only ask for files the index recorded.
pub fn document_path(item_id: i64) -> Option<PathBuf> {
    let conn = open_db()?;
    if !index_has_documents(&conn) {
        return None;
    }
    conn.query_row("SELECT path FROM documents WHERE item_id = ?1", [item_id], |row| {
        row.get::<_, String>(0)
    })
    .ok()
    .map(PathBuf::from)
}

/// Whether this index was built with per-image rows.
///
/// An index built by an earlier version has no `item_images` table, and so
/// does the one left in place when a rebuild fails part-way -- `build` swaps
/// the finished database in atomically, so a failure keeps the previous one.
/// Either way the app must still show what that index does hold.
pub fn index_has_image_rows(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'item_images'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

/// The single image an older index knows about, presented in the same shape
/// as a modern one so the detail card has only one thing to render.
fn legacy_item_image(conn: &Connection, item_id: i64) -> Vec<ItemImage> {
    conn.query_row(
        "SELECT image_path, thumb_path FROM items WHERE id = ?1",
        [item_id],
        |row| {
            let image_path: Option<String> = row.get(0)?;
            let thumb_path: Option<String> = row.get(1)?;
            Ok(image_path.map(|_| ItemImage {
                id: item_id,
                kind: "thumb".into(),
                caption: String::new(),
                has_thumb: thumb_path.is_some(),
                // Masters are recorded per image row, which this index has
                // none of -- so there is nothing better than the display copy
                // to offer, and the button must not claim otherwise.
                has_master: false,
            }))
        },
    )
    .ok()
    .flatten()
    .into_iter()
    .collect()
}

/// Every image attached to an item, in display order.
fn item_images(conn: &Connection, item_id: i64) -> Vec<ItemImage> {
    if !index_has_image_rows(conn) {
        return legacy_item_image(conn, item_id);
    }
    let sql = "SELECT id, caption, thumb_path, hires_path FROM item_images
               WHERE item_id = ?1 ORDER BY ordinal";
    let Ok(mut stmt) = conn.prepare(sql) else { return Vec::new() };
    let rows = stmt.query_map([item_id], |row| {
        let caption: Option<String> = row.get(1)?;
        let thumb_path: Option<String> = row.get(2)?;
        let hires_path: Option<String> = row.get(3)?;
        Ok(ItemImage {
            id: row.get(0)?,
            kind: "imgthumb".into(),
            caption: caption.unwrap_or_default(),
            has_thumb: thumb_path.is_some(),
            has_master: hires_path.is_some(),
        })
    });
    match rows {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// True when there is an index but it predates per-image rows, so the user
/// is missing extra images and high-resolution originals until they rebuild.
pub fn index_needs_rebuild_for_images() -> bool {
    open_db().is_some_and(|conn| !index_has_image_rows(&conn))
}

/// A record's stored fields, labelled and in the order to show them.
///
/// Presented in the curated reading order for the catalogue, not whatever
/// order the stored JSON is keyed in (serde_json sorts keys, which would put
/// "Storage Location" above "Object"). A spreadsheet row carries its own
/// order -- its columns, left to right -- which no fixed list could know.
/// Anything unrecognised goes last rather than being dropped.
fn ordered_fields(
    catalog: &str,
    map: &serde_json::Map<String, serde_json::Value>,
    labels: &std::collections::HashMap<&'static str, &'static str>,
) -> Vec<LabeledField> {
    let own_order: Option<Vec<&str>> = map
        .get("__order")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect());
    let order = own_order.unwrap_or_else(|| crate::ingest::detail_field_order(catalog).to_vec());
    let rank = |key: &str| order.iter().position(|f| *f == key).unwrap_or(usize::MAX);

    let mut collected: Vec<(usize, LabeledField)> = Vec::new();
    for (key, value) in map.iter().filter(|(k, _)| k.as_str() != "__order") {
        let text = value.as_str().unwrap_or_default().trim().to_string();
        if text.is_empty() {
            continue;
        }
        collected.push((
            rank(key),
            LabeledField {
                label: labels.get(key.as_str()).copied().unwrap_or(key.as_str()).to_string(),
                value: text,
            },
        ));
    }
    collected.sort_by_key(|(r, _)| *r);
    collected.into_iter().map(|(_, f)| f).collect()
}

pub fn get_item(id: i64) -> Option<ItemDetail> {
    let conn = open_db()?;
    let labels = field_labels();
    let images = item_images(&conn, id);
    let document = document_info(&conn, id);
    conn.query_row(
        "SELECT id, catalog, accessno, title, detail_json
         FROM items WHERE id = ?1",
        [id],
        |row| {
            let detail_json: Option<String> = row.get(4)?;
            let detail: serde_json::Value =
                serde_json::from_str(&detail_json.unwrap_or_else(|| "{}".into())).unwrap_or_default();

            let catalog: String = row.get(1)?;
            let fields = detail
                .as_object()
                .map(|map| ordered_fields(&catalog, map, &labels))
                .unwrap_or_default();

            Ok(ItemDetail {
                id: row.get(0)?,
                catalog: row.get(1)?,
                accessno: row.get(2)?,
                title: row.get(3)?,
                images,
                fields,
                document,
            })
        },
    )
    .ok()
}

/// Returns (thumb_path, original_path) for an item.
pub fn image_paths(id: i64) -> Option<(Option<PathBuf>, Option<PathBuf>)> {
    let conn = open_db()?;
    conn.query_row(
        "SELECT thumb_path, image_path FROM items WHERE id = ?1",
        [id],
        |row| {
            let thumb: Option<String> = row.get(0)?;
            let original: Option<String> = row.get(1)?;
            Ok((thumb.map(PathBuf::from), original.map(PathBuf::from)))
        },
    )
    .ok()
}

/// Where a single image's files live: its cached thumbnail (if one was made),
/// the display copy, and the high-resolution master (if one exists).
pub struct ImageFiles {
    pub thumb: Option<PathBuf>,
    pub display: PathBuf,
    pub master: Option<PathBuf>,
}

impl ImageFiles {
    /// What "download" should hand over: the master when there is one, since
    /// the display copy is PastPerfect's downsized version of it.
    pub fn best_available(&self) -> &Path {
        self.master.as_deref().unwrap_or(&self.display)
    }
}

pub fn image_files(image_id: i64) -> Option<ImageFiles> {
    let conn = open_db()?;
    conn.query_row(
        "SELECT thumb_path, image_path, hires_path FROM item_images WHERE id = ?1",
        [image_id],
        |row| {
            let thumb: Option<String> = row.get(0)?;
            let display: String = row.get(1)?;
            let master: Option<String> = row.get(2)?;
            Ok(ImageFiles {
                thumb: thumb.map(PathBuf::from),
                display: PathBuf::from(display),
                master: master.map(PathBuf::from),
            })
        },
    )
    .ok()
}

// ------------------------------------------------------------ diagnostics

/// What the index actually contains, in the shape a person reading a bug
/// report needs: whether it is current, how much is in it, and whether the
/// images and masters made it in.
pub fn index_report() -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let Some(conn) = open_db() else {
        let _ = writeln!(
            &mut out,
            "no index at {} -- nothing has been built yet, so every search will \
             return nothing",
            app_paths::db_path().display()
        );
        return out;
    };

    let has_image_rows = index_has_image_rows(&conn);
    let has_norm = conn
        .query_row("SELECT accessno_norm FROM items LIMIT 1", [], |_| Ok(()))
        .is_ok();
    let _ = writeln!(
        &mut out,
        "schema:        per-image rows {}, run-together catalog numbers {}{}",
        if has_image_rows { "yes" } else { "NO" },
        if has_norm { "yes" } else { "NO" },
        if has_image_rows && has_norm { "" } else { "  <-- REBUILD NEEDED" }
    );

    let count = |sql: &str| -> String {
        conn.query_row(sql, [], |r| r.get::<_, i64>(0))
            .map(|n| n.to_string())
            .unwrap_or_else(|e| format!("(query failed: {e})"))
    };
    let _ = writeln!(&mut out, "records:       {}", count("SELECT COUNT(*) FROM items"));
    if has_image_rows {
        let _ = writeln!(
            &mut out,
            "images:        {} rows, {} with a high-resolution master, {} with no preview",
            count("SELECT COUNT(*) FROM item_images"),
            count("SELECT COUNT(*) FROM item_images WHERE hires_path IS NOT NULL"),
            count("SELECT COUNT(*) FROM item_images WHERE thumb_path IS NULL")
        );
    }
    if index_has_documents(&conn) {
        let _ = writeln!(
            &mut out,
            "PDFs:          {} listed, {} with searchable text, {} found by name only",
            count("SELECT COUNT(*) FROM documents"),
            count("SELECT COUNT(*) FROM documents WHERE has_text = 1"),
            count("SELECT COUNT(*) FROM documents WHERE has_text = 0")
        );
    }

    let _ = writeln!(&mut out, "by catalog:");
    match conn.prepare("SELECT catalog, COUNT(*) FROM items GROUP BY catalog ORDER BY 2 DESC") {
        Ok(mut stmt) => {
            match stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
                Ok(rows) => {
                    for row in rows.flatten() {
                        let _ = writeln!(&mut out, "    {:<24} {}", row.0, row.1);
                    }
                }
                Err(e) => {
                    let _ = writeln!(&mut out, "    (could not be read: {e})");
                }
            }
        }
        Err(e) => {
            let _ = writeln!(&mut out, "    (could not be read: {e})");
        }
    }
    out
}

/// A real image from the index for the frontend to try loading, proving the
/// custom image protocol works in this webview. Prefers a per-image row,
/// falling back to an item so the probe still runs on an older index.
pub fn sample_image_for_probe() -> Option<crate::diagnostics::ProbeImage> {
    let conn = open_db()?;
    if index_has_image_rows(&conn) {
        if let Ok(id) = conn.query_row(
            "SELECT id FROM item_images WHERE thumb_path IS NOT NULL LIMIT 1",
            [],
            |r| r.get::<_, i64>(0),
        ) {
            return Some(crate::diagnostics::ProbeImage { kind: "imgthumb".into(), id });
        }
    }
    conn.query_row(
        "SELECT id FROM items WHERE thumb_path IS NOT NULL LIMIT 1",
        [],
        |r| r.get::<_, i64>(0),
    )
    .ok()
    .map(|id| crate::diagnostics::ProbeImage { kind: "thumb".into(), id })
}

/// Decodes a real image from the collection and writes a preview, end to end.
/// "No pictures anywhere" is the most likely Windows complaint, and this says
/// whether the cause is reading, decoding, or writing the cache.
pub fn probe_thumbnail() -> Result<String, String> {
    let conn = open_db().ok_or("there is no index yet, so there is nothing to preview")?;
    let original: String = conn
        .query_row(
            "SELECT image_path FROM items WHERE image_path IS NOT NULL LIMIT 1",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("no image is recorded in the index ({e})"))?;

    let source = Path::new(&original);
    if !source.is_file() {
        return Err(format!(
            "the index records an image at {original}, but no file is there now \
             (was the folder moved, renamed, or is a drive not connected?)"
        ));
    }
    let bytes = std::fs::read(source)
        .map_err(|e| format!("{original} exists but could not be read: {e}"))?;

    let scratch = app_paths::cache_dir().join("probe_cache");
    let preview = crate::thumbnails::get_preview_jpeg(source, &scratch, 256).ok_or_else(|| {
        format!(
            "read {} bytes from {original} but could not turn it into a preview \
             (unsupported image format, or the cache folder is not writable)",
            bytes.len()
        )
    })?;
    let preview_size = std::fs::metadata(&preview).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(format!(
        "read {} bytes and produced a {preview_size}-byte preview",
        bytes.len()
    ))
}

/// Confirms that typing a catalog number still puts that record first -- the
/// central promise of the tool, and the thing most likely to be quietly wrong
/// if the index was built by an older version.
pub fn probe_catalog_number_lookup() -> String {
    let Some(conn) = open_db() else { return "no index to test".into() };
    let sample: Result<(i64, String), _> = conn.query_row(
        "SELECT id, accessno FROM items WHERE accessno != '' AND length(accessno) > 6 LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    let Ok((id, accessno)) = sample else {
        return "no catalog numbers in the index to test with".into();
    };
    let found = search(&accessno, 0, false);
    match found.results.first() {
        Some(first) if first.id == id => {
            format!("{accessno:?} is the first of {} matches -- correct", found.total)
        }
        Some(first) => format!(
            "PROBLEM: searching {accessno:?} put record {} first instead of {id} \
             (of {} matches) -- the index may predate catalog-number ranking",
            first.id, found.total
        ),
        None => format!("PROBLEM: searching {accessno:?} returned nothing at all"),
    }
}

pub fn record_count() -> i64 {
    let Some(conn) = open_db() else { return 0 };
    conn.query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0)).unwrap_or(0)
}

/// Seconds since the Unix epoch that the index was last written, if it exists.
pub fn last_updated() -> Option<f64> {
    let meta = std::fs::metadata(app_paths::db_path()).ok()?;
    let modified = meta.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs_f64())
}

#[derive(Serialize)]
pub struct ValidationResult {
    pub ok: bool,
    pub message: String,
}

pub fn validate_path(kind: &str, path: &str) -> ValidationResult {
    // Cleaned the same way the rebuild will clean it, so what is confirmed
    // here is exactly what gets used. Checking one string and using another
    // is how a folder comes to be reported as found and then not read.
    let cleaned = clean_path_input(path);
    if cleaned.is_empty() {
        return ValidationResult { ok: false, message: "No path given.".into() };
    }
    let resolved = Path::new(&cleaned);
    // A spreadsheet may be a single file; every other kind must be a folder.
    let is_file_kind = kind == "spreadsheet";
    if !(resolved.is_dir() || (is_file_kind && resolved.is_file())) {
        return ValidationResult {
            ok: false,
            message: if is_file_kind { "File or folder not found." } else { "Folder not found." }.into(),
        };
    }

    match kind {
        "pastperfect_data" => {
            let found = crate::ingest::catalog_labels_present(resolved);
            if found.is_empty() {
                ValidationResult {
                    ok: false,
                    message: "Folder exists, but no PastPerfect tables (OBJECTS.DBF etc.) found in it.".into(),
                }
            } else {
                ValidationResult {
                    ok: true,
                    message: format!("Found {} PastPerfect table(s): {}.", found.len(), found.join(", ")),
                }
            }
        }
        "photo_folder" => {
            let count = crate::ingest::count_image_files(resolved);
            if count == 0 {
                ValidationResult {
                    ok: false,
                    message: "Folder exists, but no image files found in it (checked subfolders too).".into(),
                }
            } else {
                ValidationResult { ok: true, message: format!("Found {count} image file(s).") }
            }
        }
        "pdf_folder" => {
            let count = crate::ingest::count_pdf_files(resolved);
            if count == 0 {
                ValidationResult {
                    ok: false,
                    message: "Folder exists, but no PDF files found in it (checked subfolders too).".into(),
                }
            } else {
                ValidationResult { ok: true, message: format!("Found {count} PDF file(s).") }
            }
        }
        "spreadsheet" => {
            let count = crate::ingest::count_spreadsheet_files(resolved);
            if count == 0 {
                ValidationResult {
                    ok: false,
                    message: if resolved.is_file() {
                        "That is not an Excel workbook (.xlsx or .xls).".into()
                    } else {
                        "Folder exists, but no Excel workbooks (.xlsx or .xls) found in it (checked subfolders too).".into()
                    },
                }
            } else {
                ValidationResult { ok: true, message: format!("Found {count} Excel workbook(s).") }
            }
        }
        "multimedia_folder" => {
            let count = crate::ingest::count_multimedia_masters(resolved);
            if count == 0 {
                ValidationResult {
                    ok: false,
                    message: "Folder exists, but nothing in it (checked subfolders too).".into(),
                }
            } else {
                ValidationResult {
                    ok: true,
                    message: format!("Found {count} file(s) to use as high-resolution originals."),
                }
            }
        }
        _ => ValidationResult { ok: true, message: "Folder found.".into() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These run against the real index built from the live collection, so
    /// they assert on behaviour (ordering, paging, field shape) rather than
    /// on specific record counts, which change as the museum catalogues.
    /// Fails loudly rather than skipping. A test that quietly passes when
    /// its fixture is missing is worse than no test: it reports green while
    /// checking nothing.
    #[cfg(feature = "live-data")]
    fn require_index() {
        assert!(
            app_paths::db_path().exists(),
            "no search index at {} -- these tests need a built index, and must not silently pass without one",
            app_paths::db_path().display()
        );
        assert!(record_count() > 0, "search index exists but contains no records");
    }

    fn blank_result(id: i64) -> SearchResult {
        SearchResult {
            id,
            catalog: String::new(),
            accessno: String::new(),
            title: String::new(),
            snippet: "the start of the body".into(),
            has_image: false,
            is_document: false,
            is_exact_id: false,
        }
    }

    #[test]
    fn a_pdf_result_is_marked_and_shows_the_passage_that_matched() {
        let dir = std::env::temp_dir().join(format!("hs_pdfsnip_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let docs = dir.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        // The match sits far from the start, past what a 220-character head
        // of the body would show.
        let page = format!("{} The Hartwell family ledger {}", "filler ".repeat(80), "padding ".repeat(80));
        std::fs::write(docs.join("Mill Ledger.pdf"), crate::pdf::fixture::make_pdf(&[&page, "Second page"])).unwrap();
        let db = dir.join("index.db");
        let empty = PathBuf::new();
        let roots = [crate::ingest::DocumentRoot { label: "PDFs".into(), path: docs }];
        crate::ingest::build(
            &empty, &db, &empty, &empty, &[],
            &crate::ingest::DocumentSettings::new(&roots), &dir.join("cache"), None,
        )
        .unwrap();

        let conn = Connection::open(&db).unwrap();
        let id: i64 = conn.query_row("SELECT id FROM items", [], |r| r.get(0)).unwrap();
        let mut results = vec![blank_result(id), blank_result(id + 1000)];
        mark_documents(&conn, &fts_query("hartwell"), &mut results);

        assert!(results[0].is_document);
        assert!(results[0].snippet.contains("Hartwell"), "got {:?}", results[0].snippet);
        assert!(results[0].snippet.len() < 400, "a passage, not the document");
        assert!(!results[0].snippet.ends_with('…'), "the window adds its own ellipsis");
        // A record that is not a PDF is left exactly as it was.
        assert!(!results[1].is_document);
        assert_eq!(results[1].snippet, "the start of the body");

        let info = document_info(&conn, id).unwrap();
        assert_eq!((info.file_name.as_str(), info.pages, info.has_text), ("Mill Ledger.pdf", 2, true));
        assert!(document_info(&conn, id + 1000).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pages_are_ranked_by_how_many_of_the_words_they_have() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE document_pages (item_id INTEGER NOT NULL, page INTEGER NOT NULL, text TEXT NOT NULL,
                                          PRIMARY KEY (item_id, page));",
        )
        .unwrap();
        let add = |page: i64, text: &str| {
            conn.execute("INSERT INTO document_pages VALUES (7, ?1, ?2)", rusqlite::params![page, text]).unwrap();
        };
        add(1, "Minutes of the annual meeting");
        add(2, "The Hartwell family donated a loom");
        add(3, "Hartwell Mill: the Hartwell family ledger lists every Hartwell account");
        add(4, "A page about the mill alone");
        conn.execute("INSERT INTO document_pages VALUES (8, 1, 'Hartwell in another document')", []).unwrap();

        let words = |q: &[&str]| q.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        // One word: the page that says it most comes first.
        let one = pages_matching(&conn, 7, &words(&["hartwell"]));
        assert_eq!(one.iter().map(|h| h.page).collect::<Vec<_>>(), vec![3, 2]);
        // Two words: the page with both beats the page with more of one.
        let two = pages_matching(&conn, 7, &words(&["hartwell", "mill"]));
        assert_eq!(two.iter().map(|h| (h.page, h.words)).collect::<Vec<_>>(), vec![(3, 2), (2, 1), (4, 1)]);
        // Words match from the start, as they do in the search.
        assert_eq!(pages_matching(&conn, 7, &words(&["hart"])).len(), 2);
        assert!(pages_matching(&conn, 7, &words(&["art"])).is_empty(), "not from the middle of a word");
        // Another document's pages are not mixed in.
        assert!(pages_matching(&conn, 9, &words(&["hartwell"])).is_empty());
        assert!(one[0].snippet.contains("Hartwell"), "got {:?}", one[0].snippet);

        // A long page: the passage is cut at word boundaries, not mid-word.
        let long = format!("{} Hartwell {}", "alpha beta gamma ".repeat(12), "delta epsilon zeta ".repeat(12));
        conn.execute("INSERT INTO document_pages VALUES (7, 9, ?1)", [long]).unwrap();
        let hit = pages_matching(&conn, 7, &words(&["hartwell"])).into_iter().find(|h| h.page == 9).unwrap();
        assert!(hit.snippet.starts_with("… ") && hit.snippet.ends_with(" …"), "got {:?}", hit.snippet);
        let inner = hit.snippet.trim_start_matches("… ").trim_end_matches(" …");
        assert!(inner.split_whitespace().all(|w| ["alpha", "beta", "gamma", "Hartwell", "delta", "epsilon", "zeta"].contains(&w)),
            "a word was cut: {inner:?}");
    }

    #[test]
    fn an_index_without_page_text_gives_no_pages_rather_than_an_error() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(pages_matching(&conn, 1, &["x".to_string()]).is_empty());
    }

    #[test]
    fn an_index_from_before_pdfs_still_works() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY, title TEXT);").unwrap();
        let mut results = vec![blank_result(1)];
        mark_documents(&conn, "\"x\"*", &mut results);
        assert!(!results[0].is_document);
        assert!(document_info(&conn, 1).is_none());
    }

    #[test]
    fn saved_settings_from_before_pdfs_read_with_no_pdf_folders() {
        let old = r#"{"data_dir":"D","photo_archive_roots":[{"path":"P","label":"L"}]}"#;
        let config = parse_config(old).unwrap();
        assert!(config.document_roots.is_empty());
        assert!(config.recognize_scanned_pdfs, "recognition is on unless someone turns it off");
        assert_eq!(config.photo_archive_roots.len(), 1);
    }

    #[test]
    fn config_with_a_byte_order_mark_is_read() {
        let plain = r#"{"data_dir":"D","photo_archive_roots":[{"path":"P","label":"L"}]}"#;
        let with_bom = format!("\u{feff}{plain}");
        let a = parse_config(plain).unwrap();
        let b = parse_config(&with_bom).unwrap();
        assert_eq!(b.data_dir, a.data_dir);
        assert_eq!(b.photo_archive_roots.len(), 1);
        assert!(parse_config("\u{feff}{not json").is_err());
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn search_finds_results_and_pages_consistently() {
        require_index();
        let first = search("photo", 0, false);
        assert!(!first.results.is_empty(), "expected matches for a common word");
        assert!(first.results.len() as i64 <= DEFAULT_PAGE_SIZE);

        if first.has_more {
            let second = search("photo", DEFAULT_PAGE_SIZE, false);
            assert!(!second.results.is_empty(), "second page should not be empty when has_more was true");
            let first_ids: Vec<i64> = first.results.iter().map(|r| r.id).collect();
            let overlap = second.results.iter().filter(|r| first_ids.contains(&r.id)).count();
            assert_eq!(overlap, 0, "pages must not repeat the same records");
        }
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn total_is_the_real_match_count_not_the_page_size() {
        require_index();
        // The total must be the same no matter which page you ask for --
        // that's the whole point: "12 of 247", not a running "40+" that
        // jumps to "80+" as more pages load.
        let page1 = search("photo", 0, false);
        let page2 = search("photo", DEFAULT_PAGE_SIZE, false);
        assert_eq!(page1.total, page2.total, "total must not change between pages");
        assert!(
            page1.total >= page1.results.len() as i64,
            "total ({}) must be at least the number returned ({})",
            page1.total,
            page1.results.len()
        );
        if page1.has_more {
            assert!(
                page1.total > DEFAULT_PAGE_SIZE,
                "has_more implies more matches than one page"
            );
        }

        // Cross-check the total against actually walking every page.
        let mut walked = 0i64;
        let mut offset = 0i64;
        loop {
            let page = search("photo", offset, false);
            walked += page.results.len() as i64;
            if !page.has_more {
                break;
            }
            offset += DEFAULT_PAGE_SIZE;
            assert!(offset < 100_000, "pagination failed to terminate");
        }
        assert_eq!(walked, page1.total, "total must equal the number of records actually reachable");
    }

    /// Walking every page must visit each matching record exactly once.
    ///
    /// This is not guaranteed for free. Relevance scores tie heavily -- on
    /// a real collection a common word matches tens of thousands of records across
    /// only a few hundred distinct scores, with over a thousand records sharing a single score --
    /// so nearly every page boundary falls inside a large group of equally
    /// ranked rows. SQLite gives no ordering guarantee within such a group,
    /// and each page is a separate query, so without a deterministic
    /// tiebreaker "Show more results" can repeat records and skip others.
    #[test]
    #[cfg(feature = "live-data")]
    fn paging_through_every_result_visits_each_record_exactly_once() {
        require_index();
        let first = search("photo", 0, false);
        assert!(first.total > DEFAULT_PAGE_SIZE * 3, "need a query with several pages");

        let mut seen: Vec<i64> = Vec::new();
        let mut offset = 0i64;
        loop {
            let page = search("photo", offset, false);
            seen.extend(page.results.iter().map(|r| r.id));
            if !page.has_more {
                break;
            }
            offset += DEFAULT_PAGE_SIZE;
            assert!(offset < 200_000, "pagination failed to terminate");
        }

        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            seen.len(),
            "paging returned {} records but only {} distinct ones -- some appeared \
             on more than one page, which means others were skipped entirely",
            seen.len(),
            unique.len()
        );
        assert_eq!(
            seen.len() as i64,
            first.total,
            "paging reached {} records but the reported total is {}",
            seen.len(),
            first.total
        );
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn counting_matches_is_fast_enough_to_do_on_every_keystroke() {
        require_index();
        // This count runs on every search, including as-you-type, so it
        // needs to stay cheap. (Avoiding it was the original reason the UI
        // showed "40+" instead of a real total.)
        //
        // Two things keep this from measuring the wrong quantity. It runs with
        // the machine to itself, because a test that rebuilds an index uses
        // every core and this otherwise timed that instead -- 167ms against a
        // 100ms limit while a rebuild ran, 1ms on its own. And it takes the
        // quickest of several runs rather than the average, so a moment's
        // interference from anything else still leaves an honest figure.
        let _machine = crate::test_support::exclusive_use_of_the_machine();
        let mut quickest = std::time::Duration::MAX;
        for _ in 0..20 {
            let start = std::time::Instant::now();
            let _ = search("photo", 0, false);
            quickest = quickest.min(start.elapsed());
        }
        assert!(
            quickest < std::time::Duration::from_millis(100),
            "search with total count took {quickest:?} at its quickest -- too slow for as-you-type"
        );
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn show_all_returns_at_least_a_full_page() {
        require_index();
        let paged = search("photo", 0, false);
        let all = search("photo", 0, true);
        assert!(
            all.results.len() >= paged.results.len(),
            "'show all' should return at least as many as one page"
        );
    }

    #[test]
    fn empty_query_returns_nothing() {
        assert!(search("", 0, false).results.is_empty());
        assert!(search("   ", 0, false).results.is_empty());
    }

    #[test]
    fn punctuation_only_query_is_handled_safely() {
        // Must not be interpreted as FTS syntax or panic.
        let r = search("\"*(", 0, false);
        assert!(r.results.is_empty());
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn get_item_returns_labeled_fields() {
        require_index();
        let results = search("photo", 0, false);
        let id = results.results.first().expect("expected at least one result").id;
        let item = get_item(id).expect("item should be retrievable by id");
        assert_eq!(item.id, id);
        assert!(!item.title.is_empty());
        // Labels should be human-readable, not raw DBF column names.
        for f in &item.fields {
            assert!(!f.label.is_empty());
            assert!(!f.value.is_empty());
        }
    }

    /// Pulls a real record straight from the index to test against, so these
    /// tests keep working as the collection grows rather than pinning values
    /// that will drift.
    #[cfg(feature = "live-data")]
    fn sample_record_with_accession() -> (i64, String, String) {
        let conn = open_db().expect("index must be present");
        conn.query_row(
            "SELECT id, accessno, title FROM items
             WHERE accessno != '' AND length(accessno) > 4 AND title != ''
             LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("expected at least one record with an accession number")
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn searching_an_exact_accession_number_finds_that_record() {
        require_index();
        // This is the core promise of the whole tool: a volunteer types the
        // number written on the box and gets that item.
        let (id, accessno, _title) = sample_record_with_accession();
        let found = search(&accessno, 0, true);
        assert!(
            found.results.iter().any(|r| r.id == id),
            "searching for accession {accessno:?} did not return the record it belongs to \
             (got {} results)",
            found.results.len()
        );
    }

    /// Catalog numbers are made of common numbers, so a search for one also
    /// matches every record that mentions those numbers anywhere. This finds
    /// a number that really is buried that way, so the test is exercising the
    /// hard case rather than one that would rank first regardless.
    #[cfg(feature = "live-data")]
    fn sample_contested_accession() -> (i64, String, i64) {
        let conn = open_db().expect("index must be present");
        let mut stmt = conn
            .prepare(
                "SELECT id, accessno FROM items
                 WHERE accessno LIKE '____.__.__' AND title != ''
                 LIMIT 400",
            )
            .unwrap();
        let candidates: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        let mut best: Option<(i64, String, i64)> = None;
        for (id, accessno) in candidates {
            let total = search(&accessno, 0, false).total;
            if total > 50 && best.as_ref().is_none_or(|(_, _, t)| total > *t) {
                best = Some((id, accessno, total));
            }
        }
        best.expect(
            "expected some accession number that also matches many other records; \
             without one this test would prove nothing",
        )
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn an_exact_catalog_number_is_the_first_result() {
        require_index();
        // The core promise of the tool: someone reads a number off a box,
        // types it, and gets that item -- not page four of 232 near-misses.
        let (id, accessno, total) = sample_contested_accession();
        let found = search(&accessno, 0, false);
        assert!(
            total > 50,
            "test needs a genuinely contested number to be meaningful (got {total} matches)"
        );
        let first = found.results.first().expect("expected results");
        assert_eq!(
            first.id, id,
            "searching {accessno:?} put record {} first instead of {id}, among {total} matches",
            first.id
        );
        assert!(first.is_exact_id, "the exact match should be flagged as one");
        assert!(
            found.results.iter().skip(1).all(|r| !r.is_exact_id),
            "only the record whose number was typed may be flagged exact"
        );
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn a_catalog_number_is_found_however_its_separators_are_written() {
        require_index();
        // These get copied off labels, emails and handwriting; the separator
        // is not something a volunteer should have to get right.
        let (id, accessno, _) = sample_contested_accession();
        let bare = accessno.replace('.', "");
        for variant in [accessno.replace('.', "-"), accessno.replace('.', " "), bare] {
            let found = search(&variant, 0, false);
            let Some(first) = found.results.first() else {
                panic!("searching {variant:?} returned nothing at all");
            };
            assert_eq!(
                first.id, id,
                "searching {variant:?} (a rewriting of {accessno:?}) did not put record {id} first"
            );
        }
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn searching_a_lot_number_surfaces_that_lot_first() {
        require_index();
        // Typing the lot ("2010.19") should bring up its items, not records
        // that merely mention those numbers.
        let (_, accessno, _) = sample_contested_accession();
        let lot = accessno.rsplit_once('.').expect("accession should have separators").0.to_string();
        let found = search(&lot, 0, false);
        assert!(!found.results.is_empty(), "searching lot {lot:?} returned nothing");
        let lot_prefix = normalize_catalog_number(&lot);
        assert!(
            found.results.iter().take(5).any(|r| normalize_catalog_number(&r.accessno)
                .starts_with(&lot_prefix)),
            "none of the top 5 results for lot {lot:?} are actually in that lot"
        );
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn ranking_catalog_numbers_does_not_disturb_ordinary_word_searches() {
        require_index();
        // The boost must be inert for text. A word query normalises to
        // letters, which no accession number begins with, so relevance order
        // has to be exactly what it was before.
        for word in ["scrapbook", "school", "photo"] {
            let results = search(word, 0, false);
            assert!(!results.results.is_empty(), "expected matches for {word:?}");
            assert!(
                results.results.iter().all(|r| !r.is_exact_id),
                "a plain word search flagged an exact catalog-number match ({word:?})"
            );
        }
        // And a bare year must not promote every accession starting with it
        // over the records that actually discuss that year.
        assert!(!looks_like_catalog_number("19"), "a bare number is not a catalog number");
        assert!(!looks_like_catalog_number("2010"), "a bare year is not a catalog number");
        assert!(!looks_like_catalog_number("scrapbook"), "a word is not a catalog number");
        assert!(looks_like_catalog_number("2010.19"), "a lot number is a catalog number");
        assert!(looks_like_catalog_number("2010.19.18"), "an item number is a catalog number");
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn searching_words_from_a_title_finds_that_record() {
        require_index();
        let (id, _accessno, title) = sample_record_with_accession();
        let words: Vec<&str> = title
            .split_whitespace()
            .filter(|w| w.chars().all(|c| c.is_alphanumeric()) && w.len() > 3)
            .take(2)
            .collect();
        assert!(!words.is_empty(), "test needs a title with usable words: {title:?}");
        let query = words.join(" ");
        let found = search(&query, 0, true);
        assert!(
            found.results.iter().any(|r| r.id == id),
            "searching {query:?} (words from its own title) did not return record {id}"
        );
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn extra_words_narrow_the_search_rather_than_widening_it() {
        require_index();
        // Multiple words must be AND, not OR -- otherwise adding a word to
        // refine a search would return *more* noise instead of less.
        //
        // Both words are taken from a record actually in the collection, so
        // this holds for any collection rather than depending on the places
        // and subjects of one particular society.
        let conn = open_db().expect("index must be present");
        let mut stmt = conn
            .prepare("SELECT title FROM items WHERE title LIKE '% %' LIMIT 200")
            .unwrap();
        let titles: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        let usable = |t: &str| -> Vec<String> {
            t.split_whitespace()
                .filter(|w| w.chars().all(|c| c.is_alphanumeric()) && w.len() > 3)
                .map(|w| w.to_string())
                .take(2)
                .collect()
        };
        let words = titles
            .iter()
            .map(|t| usable(t))
            .find(|w| w.len() == 2)
            .expect("expected some record with two usable words in its title");
        let one = search(&words[0], 0, false).total;
        let two = search(&words.join(" "), 0, false).total;
        assert!(one > 0, "expected {:?} to match something", words[0]);
        assert!(
            two <= one,
            "adding a word widened the search ({one} -> {two}); terms are being OR'd, not AND'd"
        );
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn search_is_case_insensitive() {
        require_index();
        let lower = search("scrapbook", 0, false).total;
        let upper = search("SCRAPBOOK", 0, false).total;
        let mixed = search("ScRaPbOoK", 0, false).total;
        assert!(lower > 0, "expected matches for a common word");
        assert_eq!(lower, upper, "uppercase query returned a different count");
        assert_eq!(lower, mixed, "mixed-case query returned a different count");
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn a_word_that_appears_nowhere_returns_nothing() {
        require_index();
        let found = search("zzzqqxnotarealword", 0, false);
        assert_eq!(found.total, 0, "nonsense query matched records");
        assert!(found.results.is_empty());
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn detail_fields_follow_the_curated_reading_order() {
        require_index();
        // Presentation order is an editorial choice, not whatever order the
        // stored JSON happens to be keyed in. serde_json sorts object keys,
        // which put "Storage Location" above "Object" -- so this pins the
        // intended order rather than trusting serialisation details.
        let results = search("scrapbook", 0, false);
        let mut checked = 0;
        for r in results.results.iter().take(20) {
            let Some(item) = get_item(r.id) else { continue };
            let order = crate::ingest::detail_field_order(&item.catalog);
            if order.is_empty() {
                continue;
            }
            let labels = field_labels();
            // Map each displayed field back to its position in the curated
            // list; those positions must be non-decreasing.
            let mut last_rank = 0usize;
            for f in &item.fields {
                let rank = order
                    .iter()
                    .position(|key| {
                        labels.get(*key).copied().unwrap_or(*key) == f.label
                    })
                    .unwrap_or(usize::MAX);
                assert!(
                    rank >= last_rank,
                    "item {} ({}): field {:?} appears out of curated order",
                    item.id,
                    item.catalog,
                    f.label
                );
                last_rank = rank;
            }
            checked += 1;
        }
        assert!(checked > 0, "expected to check at least one item's field order");
    }

    /// An index built before per-image rows existed -- also what is left in
    /// place when a rebuild fails, since `build` only swaps the new database
    /// in once it is complete.
    fn legacy_schema_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items (
                id INTEGER PRIMARY KEY, catalog TEXT, accessno TEXT, title TEXT,
                body TEXT, image_path TEXT, thumb_path TEXT, detail_json TEXT
             );
             INSERT INTO items (id, catalog, title, image_path, thumb_path)
             VALUES (1, 'Photo', 'Has an image', '/some/photo.jpg', '/cache/photo.jpg'),
                    (2, 'Photo', 'Has none', NULL, NULL);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn an_index_without_image_rows_still_shows_the_image_it_has() {
        let conn = legacy_schema_db();
        assert!(!index_has_image_rows(&conn), "this fixture is the old schema");

        let images = item_images(&conn, 1);
        assert_eq!(images.len(), 1, "the item's own image should still be offered");
        // Must be addressed as an item id, since there is no image row to
        // point at -- getting this wrong serves a picture from an unrelated
        // record rather than failing visibly.
        assert_eq!(images[0].kind, "thumb");
        assert_eq!(images[0].id, 1);
        assert!(images[0].has_thumb);
        assert!(!images[0].has_master, "an old index records no masters, so none may be promised");

        assert!(item_images(&conn, 2).is_empty(), "an item with no image must offer none");
    }

    #[test]
    fn a_current_index_addresses_images_individually_and_in_order() {
        let conn = legacy_schema_db();
        conn.execute_batch(
            "CREATE TABLE item_images (
                id INTEGER PRIMARY KEY, item_id INTEGER NOT NULL, ordinal INTEGER NOT NULL,
                caption TEXT, image_path TEXT NOT NULL, thumb_path TEXT, hires_path TEXT
             );
             -- Inserted out of order on purpose: display order comes from
             -- `ordinal`, not from however the rows happen to be stored.
             INSERT INTO item_images (id, item_id, ordinal, caption, image_path, thumb_path, hires_path)
             VALUES (30, 1, 2, 'Back Of Pony Express Monument', '/b.jpg', '/cb.jpg', NULL),
                    (10, 1, 1, 'Front Of Pony Express Monument', '/f.jpg', '/cf.jpg', '/master/f-large.tif');",
        )
        .unwrap();
        assert!(index_has_image_rows(&conn));

        let images = item_images(&conn, 1);
        assert_eq!(images.len(), 2, "both images of the record should be offered");
        assert_eq!(images[0].caption, "Front Of Pony Express Monument", "wrong image leads");
        assert_eq!(images[1].caption, "Back Of Pony Express Monument");
        assert_eq!(images[0].kind, "imgthumb");
        assert_eq!(images[0].id, 10, "images are addressed by image id, not item id");
        assert!(images[0].has_master, "this image has a high-resolution master");
        assert!(!images[1].has_master, "this one does not, and must not claim to");
    }

    #[test]
    fn download_prefers_the_master_over_the_display_copy() {
        // The whole point of reading the Multimedia folder: the copy in Images
        // is PastPerfect's downsized display version.
        let with_master = ImageFiles {
            thumb: None,
            display: PathBuf::from("/images/001/x.jpg"),
            master: Some(PathBuf::from("/multimedia/Largeimages1/x-large.tif")),
        };
        assert_eq!(
            with_master.best_available(),
            Path::new("/multimedia/Largeimages1/x-large.tif")
        );

        let without = ImageFiles {
            thumb: None,
            display: PathBuf::from("/images/001/x.jpg"),
            master: None,
        };
        assert_eq!(without.best_available(), Path::new("/images/001/x.jpg"));
    }

    #[test]
    fn a_pasted_path_is_accepted_however_it_arrives() {
        // The ordinary way to get a path on Windows is Shift+right-click,
        // "Copy as path", which includes the quotation marks. Taken literally
        // that folder does not exist, and the message would say so without
        // hinting at the cause.
        let real = std::env::temp_dir();
        let real_text = real.to_string_lossy().to_string();

        for supplied in [
            format!("\"{real_text}\""),
            format!("  {real_text}  "),
            format!("  \"{real_text}\"  "),
            format!("'{real_text}'"),
        ] {
            let result = validate_path("generic", &supplied);
            assert!(
                result.ok,
                "a folder that exists was rejected when supplied as {supplied:?}: {}",
                result.message
            );
        }
    }

    #[test]
    fn cleaning_a_path_leaves_legitimate_ones_untouched() {
        // Only balanced quotes are stripped; anything else is the user's path
        // and must survive verbatim.
        assert_eq!(clean_path_input("/plain/path"), "/plain/path");
        assert_eq!(clean_path_input(r"C:\pp5\Data"), r"C:\pp5\Data");
        assert_eq!(clean_path_input("  /spaced/  "), "/spaced/");
        assert_eq!(clean_path_input("\"/quoted/\""), "/quoted/");
        // A lone or unbalanced quote is not a quoting attempt.
        assert_eq!(clean_path_input("\""), "\"");
        assert_eq!(clean_path_input("\"/unbalanced"), "\"/unbalanced");
        assert_eq!(clean_path_input("/has\"quote/inside"), "/has\"quote/inside");
        assert_eq!(clean_path_input(""), "");
        assert_eq!(clean_path_input("   "), "");
    }

    #[test]
    fn validate_path_reports_useful_messages() {
        let missing = validate_path("pastperfect_data", "/definitely/not/a/real/folder");
        assert!(!missing.ok);
        assert!(missing.message.contains("not found"));

        let empty = validate_path("photo_folder", "");
        assert!(!empty.ok);
    }

    #[test]
    fn a_spreadsheet_rows_fields_follow_its_columns_not_the_alphabet() {
        let detail = serde_json::json!({
            "Name": "Thaddeus Marsh", "Plot": "A-13", "Died": "1934-11-30",
            "SPREADSHEET": "register.xlsx", "SHEET": "Register", "ROW": "4",
            "__order": ["Name", "Died", "Plot", "Empty", "SPREADSHEET", "SHEET", "ROW"],
        });
        let fields = ordered_fields("Cemetery", detail.as_object().unwrap(), &field_labels());
        let shown: Vec<_> = fields.iter().map(|f| (f.label.as_str(), f.value.as_str())).collect();
        assert_eq!(
            shown,
            [
                ("Name", "Thaddeus Marsh"), ("Died", "1934-11-30"), ("Plot", "A-13"),
                ("Spreadsheet", "register.xlsx"), ("Sheet", "Register"), ("Row", "4"),
            ]
        );
    }

    #[test]
    fn a_single_workbook_file_passes_the_spreadsheet_check() {
        let support = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support");
        let file = validate_path("spreadsheet", &support.join("donors.xls").to_string_lossy());
        assert!(file.ok, "got {}", file.message);
        let folder = validate_path("spreadsheet", &support.to_string_lossy());
        assert!(folder.ok, "got {}", folder.message);
        let pdf = validate_path("spreadsheet", &support.join("scan_g4.pdf").to_string_lossy());
        assert!(!pdf.ok && pdf.message.contains("not an Excel"), "got {}", pdf.message);
        let gone = validate_path("spreadsheet", &support.join("nope.xls").to_string_lossy());
        assert!(!gone.ok && gone.message.contains("not found"), "got {}", gone.message);
        // The other kinds still want a folder.
        assert!(!validate_path("pdf_folder", &support.join("scan_g4.pdf").to_string_lossy()).ok);
    }

    #[test]
    fn a_search_can_be_narrowed_to_chosen_datasets_and_still_counts_the_rest() {
        let conn = Connection::open_in_memory().unwrap();
        conn.create_scalar_function("catalog_norm", 1, rusqlite::functions::FunctionFlags::SQLITE_UTF8, |ctx| {
            Ok(normalize_catalog_number(&ctx.get::<String>(0)?))
        })
        .unwrap();
        conn.execute_batch(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, catalog TEXT, accessno TEXT, title TEXT, body TEXT,
                                 image_path TEXT, thumb_path TEXT, detail_json TEXT, accessno_norm TEXT);
             CREATE VIRTUAL TABLE items_fts USING fts5(title, body, accessno, accessno_norm,
                                                       content='items', content_rowid='id');
             INSERT INTO items (catalog, accessno, title, body) VALUES
               ('Photo', '', 'Mill pond', 'mill'), ('Photo', '', 'Mill crew', 'mill'),
               ('Object', '', 'Mill saw', 'mill'), ('Newsletters', '', 'Mill news', 'mill'),
               ('Object', '', 'A teapot', 'china');
             INSERT INTO items_fts (rowid, title, body, accessno, accessno_norm)
               SELECT id, title, body, accessno, accessno_norm FROM items;",
        )
        .unwrap();

        let all = search_with(&conn, "mill", 0, false, &[]);
        assert_eq!(all.total, 4);
        let counts: Vec<_> = all.datasets.iter().map(|d| (d.name.as_str(), d.count)).collect();
        assert_eq!(counts, [("Photo", 2), ("Newsletters", 1), ("Object", 1)]);

        let narrowed = search_with(&conn, "mill", 0, false, &["Photo".into(), "Object".into()]);
        assert_eq!(narrowed.total, 3);
        assert_eq!(narrowed.results.len(), 3);
        assert!(narrowed.results.iter().all(|r| r.catalog != "Newsletters"));
        // The dataset left out is still offered, with its number.
        assert_eq!(narrowed.datasets.len(), 3);

        // A choice with nothing in it: no results, but the others still listed.
        let none = search_with(&conn, "teapot", 0, false, &["Photo".into()]);
        assert_eq!((none.total, none.results.len()), (0, 0));
        assert_eq!(none.datasets.len(), 1);

        // Counts are sent with the first page only.
        assert!(search_with(&conn, "mill", 1, false, &[]).datasets.is_empty());
    }
}
