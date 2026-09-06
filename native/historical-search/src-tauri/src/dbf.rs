//! Reader for Visual FoxPro .DBF/.FPT tables, as used by PastPerfect.
//!
//! This is a direct port of the prototype's `pp_dbf.py` (itself validated
//! against the real PastPerfect data during the prototype phase), not a
//! reimplementation from scratch -- the byte-level format decisions here
//! (memo pointer encoding, block header layout, encoding) are known-correct
//! against real files, not guesses.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

struct Memo {
    file: fs::File,
    block_size: u32,
}

impl Memo {
    fn open(path: &Path) -> std::io::Result<Self> {
        let mut file = fs::File::open(path)?;
        let mut header = [0u8; 512];
        file.read_exact(&mut header)?;
        let block_size = u16::from_be_bytes([header[6], header[7]]) as u32;
        let block_size = if block_size == 0 { 512 } else { block_size };
        Ok(Memo { file, block_size })
    }

    fn get(&mut self, block_num: u32) -> String {
        if block_num == 0 {
            return String::new();
        }
        let offset = (block_num as u64) * (self.block_size as u64);
        if self.file.seek(SeekFrom::Start(offset)).is_err() {
            return String::new();
        }
        let mut block_header = [0u8; 8];
        if self.file.read_exact(&mut block_header).is_err() {
            return String::new();
        }
        let length = u32::from_be_bytes([block_header[4], block_header[5], block_header[6], block_header[7]]);
        if length == 0 || length > 10_000_000 {
            return String::new();
        }
        let mut buf = vec![0u8; length as usize];
        if self.file.read_exact(&mut buf).is_err() {
            return String::new();
        }
        latin1_to_string(&buf)
    }
}

fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// Rust's default f64 Display drops the decimal point for whole numbers
/// (0.0 -> "0"); Python's str(float) always keeps it (0.0 -> "0.0"). Match
/// Python's convention since that's the reference this parser is ported from.
fn format_float_like_python(v: f64) -> String {
    let s = v.to_string();
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
        s
    } else {
        format!("{s}.0")
    }
}

#[derive(Clone)]
struct FieldDef {
    name: String,
    ftype: char,
    len: usize,
    offset: usize,
}

pub struct DbfTable {
    data: Vec<u8>,
    num_records: u32,
    header_len: u16,
    record_len: u16,
    fields: Vec<FieldDef>,
    memo: Option<Memo>,
    /// True when this table has memo (`M`) fields but the companion .FPT
    /// file could not be opened. Every memo value then reads as empty --
    /// descriptions, notes, subjects -- while the record count still looks
    /// perfectly healthy, so this has to be surfaced rather than swallowed.
    memo_unavailable: bool,
}

fn sibling_with_ext(path: &Path, ext: &str) -> Option<std::path::PathBuf> {
    let stem = path.file_stem()?.to_str()?;
    let dir = path.parent()?;
    for candidate_ext in [ext, &ext.to_uppercase()] {
        let candidate = dir.join(format!("{stem}.{candidate_ext}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // Case-insensitive fallback scan, same as the Python version's find_file().
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.to_lowercase() == format!("{}.{}", stem.to_lowercase(), ext.to_lowercase()) {
                return Some(entry.path());
            }
        }
    }
    None
}

impl DbfTable {
    /// Opens a DBF table, validating the header rather than trusting it.
    ///
    /// Everything here is bounds-checked deliberately: these files can be
    /// read while PastPerfect is mid-write (they live on a shared folder),
    /// and a zero-byte, truncated, or simply non-DBF file must produce a
    /// clean error, not a panic -- this app is built with `panic = "abort"`,
    /// so a single out-of-bounds index would take down the whole program.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        fn invalid(msg: &str) -> std::io::Error {
            std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
        }

        let data = fs::read(path)?;

        // 32-byte file header, then at least one 32-byte field descriptor
        // and the 0x0D terminator.
        if data.len() < 32 {
            return Err(invalid("file is too short to be a DBF table"));
        }

        let num_records = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let header_len = u16::from_le_bytes([data[8], data[9]]);
        let record_len = u16::from_le_bytes([data[10], data[11]]);

        if header_len < 33 || record_len == 0 {
            return Err(invalid("DBF header is malformed (bad header/record length)"));
        }

        let mut fields = Vec::new();
        let mut pos = 32usize;
        let mut offset = 1usize; // byte 0 of each record is the deletion flag
        let mut terminated = false;
        // Each descriptor is 32 bytes; stop at the 0x0D terminator, and never
        // read past the end of the buffer if that terminator is missing.
        while pos < data.len() {
            if data[pos] == 0x0D {
                terminated = true;
                break;
            }
            if pos + 32 > data.len() {
                break; // truncated mid-descriptor
            }
            let name_bytes = &data[pos..pos + 11];
            let name_end = name_bytes.iter().position(|&b| b == 0).unwrap_or(11);
            let name = latin1_to_string(&name_bytes[..name_end]);
            let ftype = data[pos + 11] as char;
            let len = data[pos + 16] as usize;
            fields.push(FieldDef { name, ftype, len, offset });
            offset += len;
            pos += 32;
        }

        if !terminated {
            return Err(invalid("DBF field descriptors are truncated (no terminator found)"));
        }
        if fields.is_empty() {
            return Err(invalid("DBF table declares no fields"));
        }

        let memo = sibling_with_ext(path, "fpt").and_then(|p| Memo::open(&p).ok());
        let memo_unavailable = memo.is_none() && fields.iter().any(|f| f.ftype == 'M');

        Ok(DbfTable { data, num_records, header_len, record_len, fields, memo, memo_unavailable })
    }

    /// True when memo text (descriptions, notes, ...) cannot be read even
    /// though this table has memo fields -- i.e. the index will be built
    /// with those fields silently blank.
    /// Record count from the header, including any marked deleted. Reported
    /// in diagnostics so a table that opens but reads as empty can be told
    /// apart from one that genuinely has no rows.
    pub fn record_count(&self) -> u32 {
        self.num_records
    }

    pub fn memo_unavailable(&self) -> bool {
        self.memo_unavailable
    }

    fn coerce(&mut self, ftype: char, raw: &[u8]) -> String {
        match ftype {
            'M' => {
                // Matches pp_dbf.py's `raw.strip()` precisely: strip leading/
                // trailing ASCII whitespace (space/tab/CR/LF/etc) ONLY, never
                // null bytes. A field like [0x20, 0x00, 0x00, 0x00] (block
                // number 32, encoded little-endian) is NOT blank -- treating
                // any zero-or-space byte as "blank" (an earlier version of
                // this code did) silently dropped real memo content whenever
                // the block number's bytes happened to include a 0x00 or
                // 0x20, which is common for small block numbers.
                fn is_ascii_ws(b: u8) -> bool {
                    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
                }
                let mut start = 0;
                let mut end = raw.len();
                while start < end && is_ascii_ws(raw[start]) { start += 1; }
                while end > start && is_ascii_ws(raw[end - 1]) { end -= 1; }
                let block_txt = &raw[start..end];
                if block_txt.is_empty() {
                    return String::new();
                }

                // A memo pointer is 4 bytes; a truncated record could hand us
                // fewer, so read defensively rather than indexing blindly.
                if raw.len() < 4 {
                    return String::new();
                }
                let mut block_num = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
                if block_num == 0 {
                    let mut s = 0;
                    let mut e = block_txt.len();
                    while s < e && block_txt[s] == 0 { s += 1; }
                    while e > s && block_txt[e - 1] == 0 { e -= 1; }
                    let digits = &block_txt[s..e];
                    if !digits.is_empty() && digits.iter().all(|b| b.is_ascii_digit()) {
                        let text: String = digits.iter().map(|&b| b as char).collect();
                        block_num = text.parse().unwrap_or(0);
                    }
                }
                match &mut self.memo {
                    Some(m) => m.get(block_num),
                    None => String::new(),
                }
            }
            'D' => {
                let text = latin1_to_string(raw);
                let text = text.trim();
                if text.len() == 8 && text.chars().all(|c| c.is_ascii_digit()) {
                    let year = &text[0..4];
                    let month = &text[4..6];
                    let day = &text[6..8];
                    format!("{year}-{month}-{day}")
                } else {
                    String::new()
                }
            }
            'N' => {
                // Match pp_dbf.py exactly: parse through float/int and
                // re-stringify (so "0.00" -> "0.0"), not a raw-text passthrough.
                let text = latin1_to_string(raw);
                let text = text.trim();
                if text.is_empty() {
                    String::new()
                } else if text.contains('.') {
                    match text.parse::<f64>() {
                        Ok(v) => format_float_like_python(v),
                        Err(_) => String::new(),
                    }
                } else {
                    match text.parse::<i64>() {
                        Ok(v) => v.to_string(),
                        Err(_) => String::new(),
                    }
                }
            }
            'L' => {
                // pp_dbf.py returns a Python bool here, which stringifies as
                // "True"/"False", not the raw "T"/"F" character.
                let text = latin1_to_string(raw);
                let up = text.trim().to_uppercase();
                if up == "T" || up == "Y" { "True".to_string() } else { "False".to_string() }
            }
            _ => latin1_to_string(raw).trim_end().to_string(),
        }
    }

    /// Reads every non-deleted record into a Vec of field-name -> text-value
    /// maps. PastPerfect tables here max out around a few thousand rows, so
    /// loading a whole table at once is simpler than a lazy iterator and not
    /// a real cost.
    pub fn records(&mut self) -> Vec<HashMap<String, String>> {
        // The record count comes from the file header, so a corrupt header
        // could claim billions of records; only trust it as far as the file
        // could actually hold, so we never pre-allocate on a bad number.
        let max_possible = self
            .data
            .len()
            .saturating_sub(self.header_len as usize)
            / (self.record_len as usize).max(1);
        let record_count = (self.num_records as usize).min(max_possible);

        let mut out = Vec::with_capacity(record_count);
        let fields = self.fields.clone();
        for i in 0..record_count {
            let start = self.header_len as usize + i * self.record_len as usize;
            if start >= self.data.len() {
                break;
            }
            let rec = &self.data[start..(start + self.record_len as usize).min(self.data.len())];
            if rec.is_empty() || rec[0] == b'*' {
                continue; // soft-deleted record
            }
            // Copy the record's bytes out before calling coerce() (which
            // needs &mut self for the memo file), since `rec` otherwise
            // holds an immutable borrow of self.data across that call.
            let rec_owned = rec.to_vec();
            let mut row = HashMap::with_capacity(fields.len());
            for f in &fields {
                // A short/truncated record can leave a field starting past
                // the end of the row; slicing that directly would panic.
                let start = f.offset.min(rec_owned.len());
                let end = (f.offset + f.len).min(rec_owned.len()).max(start);
                let value = self.coerce(f.ftype, &rec_owned[start..end]);
                row.insert(f.name.clone(), value);
            }
            out.push(row);
        }
        out
    }
}

/// Case-insensitive file lookup within a directory, matching the Python
/// version's `find_file()` (PastPerfect table names appear inconsistently
/// upper/lower-cased across installs).
pub fn find_file(dir: &Path, filename: &str) -> Option<std::path::PathBuf> {
    let direct = dir.join(filename);
    if direct.exists() {
        return Some(direct);
    }
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().to_lowercase() == filename.to_lowercase() {
            return Some(entry.path());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "live-data")]
    use std::collections::HashMap as Map;
    #[cfg(feature = "live-data")]
    use std::process::Command;

    // Deliberately NOT hardcoding record counts or specific accession
    // numbers here: this is the historical society's live, actively-growing
    // database (volunteers add records to it continuously), so any such
    // number would start failing the moment someone catalogs a new item --
    // that's the test asserting the wrong thing, not the code being wrong.
    //
    // Instead: compare the Rust parser's output against the already-proven
    // Python parser (pp_dbf.py) on whatever the real data happens to be
    // *right now*. That stays valid forever, and it's a stronger check than
    // a few hardcoded values -- every field of every record, not spot checks.

    #[cfg(feature = "live-data")]
    fn dump_via_python(dbf_path: &Path) -> Vec<Map<String, String>> {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/dump_dbf.py");
        let prototype_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../prototype");
        let output = Command::new("python3")
            .arg(&script)
            .arg(dbf_path)
            .arg(&prototype_dir)
            .output()
            .expect("failed to run python3 reference dump");
        assert!(
            output.status.success(),
            "python reference dump failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("invalid JSON from python reference dump")
    }

    #[cfg(feature = "live-data")]
    fn assert_matches_python_reference(filename: &str) {
        let path = find_file(&crate::test_support::data_dir(), filename)
            .unwrap_or_else(|| panic!("{filename} not found -- is the PastPerfect data folder available?"));

        let mut table = DbfTable::open(&path).expect("failed to open table");
        let rust_records = table.records();
        let python_records = dump_via_python(&path);

        assert_eq!(
            rust_records.len(),
            python_records.len(),
            "{filename}: Rust and Python parsed a different number of records"
        );

        for (i, (rust_rec, py_rec)) in rust_records.iter().zip(python_records.iter()).enumerate() {
            for (field, py_value) in py_rec {
                let rust_value = rust_rec.get(field).map(|s| s.as_str()).unwrap_or("<missing>");
                assert_eq!(
                    rust_value, py_value,
                    "{filename} record #{i}, field {field}: Rust={rust_value:?} Python={py_value:?}"
                );
            }
        }
    }

    /// Malformed input must produce errors, never panics. This app is built
    /// with `panic = "abort"`, so an out-of-bounds read here would kill the
    /// whole program -- and these files are read from a shared folder that
    /// PastPerfect may be writing to at the same time.
    #[test]
    fn malformed_files_error_instead_of_panicking() {
        let dir = std::env::temp_dir().join(format!("hs_dbf_malformed_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);

        let mut header_no_terminator = vec![0u8; 32];
        header_no_terminator[0] = 0x30;
        header_no_terminator.extend(std::iter::repeat_n(0x41, 200));

        let mut truncated = vec![0u8; 32];
        truncated[0] = 0x30;
        truncated[4] = 10; // claims 10 records that aren't there
        truncated[8] = 65;
        truncated[10] = 50;
        let mut field = vec![0u8; 32];
        field[0] = b'A';
        field[11] = b'C';
        field[16] = 10;
        truncated.extend(field);
        truncated.push(0x0D);

        let mut absurd_record_count = truncated.clone();
        absurd_record_count[4] = 0xFF; // ~4 billion records claimed
        absurd_record_count[5] = 0xFF;
        absurd_record_count[6] = 0xFF;
        absurd_record_count[7] = 0xFF;

        for (name, bytes) in [
            ("empty", Vec::new()),
            ("tiny", vec![0x30, 0x00, 0x00]),
            ("no_terminator", header_no_terminator),
            ("truncated", truncated),
            ("absurd_record_count", absurd_record_count),
        ] {
            let path = dir.join(format!("{name}.dbf"));
            fs::write(&path, &bytes).unwrap();
            // Must not panic; either a clean Err or a readable (possibly
            // empty) table is acceptable.
            if let Ok(mut table) = DbfTable::open(&path) {
                let _ = table.records();
            }
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn objects_match_python_reference() {
        assert_matches_python_reference("OBJECTS.DBF");
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn people_match_python_reference() {
        assert_matches_python_reference("PEOPLE.DBF");
    }

    #[test]
    #[cfg(feature = "live-data")]
    fn photos_match_python_reference() {
        assert_matches_python_reference("PHOTOS.DBF");
    }

    /// A genuine regression guard for the "shared accession lot vs. unique
    /// item id" bug found earlier -- but expressed as a property that holds
    /// no matter how many objects or lots exist, rather than a specific
    /// count, so it never goes stale as the collection grows.
    #[test]
    #[cfg(feature = "live-data")]
    fn objectid_is_unique_within_every_accession_lot() {
        let path = find_file(&crate::test_support::data_dir(), "OBJECTS.DBF")
            .expect("OBJECTS.DBF not found");
        let mut table = DbfTable::open(&path).expect("failed to open OBJECTS.DBF");

        let mut seen_by_lot: Map<String, std::collections::HashSet<String>> = Map::new();
        for rec in table.records() {
            let lot = rec.get("ACCESSNO").cloned().unwrap_or_default();
            let object_id = rec.get("OBJECTID").cloned().unwrap_or_default();
            if lot.is_empty() || object_id.is_empty() {
                continue;
            }
            let seen = seen_by_lot.entry(lot.clone()).or_default();
            assert!(
                seen.insert(object_id.clone()),
                "duplicate OBJECTID {object_id:?} within accession lot {lot:?}"
            );
        }
    }
}
