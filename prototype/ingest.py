"""Build a searchable SQLite database from a PastPerfect Data folder, plus
any standalone photo archive folders (not linked to PastPerfect records).

Usage:
    python3 ingest.py
(reads paths from config.json; see server.py for how this is invoked)
"""
import sys
import os
import json
import sqlite3
from pp_dbf import DBFTable
from thumbnails import get_preview_jpeg

IMAGE_EXTENSIONS = {".jpg", ".jpeg", ".tif", ".tiff", ".bmp", ".png", ".gif"}

# (catalog label, dbf filename, title field(s), body field(s) to index, detail field(s) to show)
CATALOG_TABLES = [
    ("Object", "OBJECTS.DBF", ["OBJNAME", "OBJNAME2", "OBJNAME3"],
     ["DESCRIP", "CREATOR", "CREATOR2", "CREATOR3", "MATERIAL", "MEDIUM",
      "SUBJECTS", "PEOPLE", "SITE", "NOTES", "COLLECTION", "PROVENANCE",
      "OTHERNAME", "HOMELOC", "STERMS", "CLASSES"],
     ["OBJNAME", "OBJNAME2", "OBJNAME3", "ACCESSNO", "DATE", "DESCRIP",
      "CREATOR", "CREATOR2", "CREATOR3", "MATERIAL", "MEDIUM", "SUBJECTS",
      "PEOPLE", "SITE", "COLLECTION", "PROVENANCE", "CONDITION", "HOMELOC",
      "NOTES"]),
    ("Photo", "PHOTOS.DBF", ["OBJNAME", "OBJNAME2", "OBJNAME3", "TITLE"],
     ["CAPTION", "DESCRIP", "CREATOR", "SUBJECTS", "PEOPLE", "SITE", "NOTES",
      "COLLECTION", "PLACE", "EVENT", "STERMS", "CLASSES"],
     ["OBJNAME", "TITLE", "ACCESSNO", "DATE", "CAPTION", "DESCRIP", "CREATOR",
      "SUBJECTS", "PEOPLE", "SITE", "PLACE", "EVENT", "COLLECTION",
      "CONDITION", "HOMELOC", "NOTES"]),
    ("Archive", "ARCHIVES.DBF", ["OBJNAME", "OBJNAME2", "OBJNAME3", "TITLE"],
     ["DESCRIP", "CREATOR", "SUBJECTS", "PEOPLE", "SITE", "NOTES",
      "COLLECTION", "SERIES", "NARRATOR", "STERMS", "CLASSES"],
     ["OBJNAME", "TITLE", "ACCESSNO", "DATE", "DESCRIP", "CREATOR",
      "SUBJECTS", "PEOPLE", "SITE", "SERIES", "NARRATOR", "COLLECTION",
      "CONDITION", "HOMELOC", "NOTES"]),
    ("Library Item", "LIBRARY.DBF", ["OBJNAME", "OBJNAME2", "OBJNAME3", "TITLE", "TITLEX"],
     ["DESCRIP", "AUTHORX", "SUBJECTS", "PEOPLE", "SITE", "NOTES",
      "COLLECTION", "SERIES", "PUBLISHER", "STERMS", "CLASSES"],
     ["OBJNAME", "TITLE", "TITLEX", "ACCESSNO", "DATE", "AUTHORX", "DESCRIP",
      "SUBJECTS", "PEOPLE", "SITE", "SERIES", "PUBLISHER", "COLLECTION",
      "CONDITION", "HOMELOC", "NOTES"]),
]

PEOPLE_TABLE = ("Person", "PEOPLE.DBF", ["NAME", "OTHERNAME"],
                ["OCCUPATION", "NOTES", "BIRTHPLACE", "LASTPLACE",
                 "NATIONAL", "ROLE", "CAPTION"],
                ["NAME", "OTHERNAME", "BORN", "DIED", "BIRTHPLACE",
                 "LASTPLACE", "OCCUPATION", "NATIONAL", "ROLE", "FATHER",
                 "MOTHER", "NOTES"])

FIELD_LABELS = {
    "OBJNAME": "Object", "OBJNAME2": "Object (alt)", "OBJNAME3": "Object (alt)",
    "TITLE": "Title", "TITLEX": "Title", "ACCESSNO": "Accession Lot",
    "DATE": "Date", "DESCRIP": "Description", "CAPTION": "Caption",
    "CREATOR": "Creator/Maker", "CREATOR2": "Creator/Maker",
    "CREATOR3": "Creator/Maker", "AUTHORX": "Author",
    "MATERIAL": "Materials", "MEDIUM": "Medium", "SUBJECTS": "Subjects",
    "PEOPLE": "People", "SITE": "Site", "PLACE": "Place", "EVENT": "Event",
    "COLLECTION": "Collection", "PROVENANCE": "Provenance",
    "CONDITION": "Condition", "HOMELOC": "Storage Location",
    "NOTES": "Notes", "SERIES": "Series", "NARRATOR": "Narrator",
    "PUBLISHER": "Publisher", "NAME": "Name", "OTHERNAME": "Other Name",
    "BORN": "Born", "DIED": "Died", "BIRTHPLACE": "Birthplace",
    "LASTPLACE": "Last Residence", "OCCUPATION": "Occupation",
    "NATIONAL": "Nationality", "ROLE": "Role", "FATHER": "Father",
    "MOTHER": "Mother", "FOLDER": "Folder", "CATEGORY": "Category",
    "FILENAME": "Original Filename",
}


def find_file(directory, filename):
    path = os.path.join(directory, filename)
    if os.path.exists(path):
        return path
    if os.path.isdir(directory):
        for f in os.listdir(directory):
            if f.lower() == filename.lower():
                return os.path.join(directory, f)
    return None


def build(data_dir, db_path, images_dir, photo_archive_roots=None, thumb_cache_dir=None, progress=None):
    """progress, if given, is called as progress(phase, current, total) --
    total is None for the quick PastPerfect table phases (no useful count to
    show), and a real number for the (slow) photo archive phases."""
    photo_archive_roots = photo_archive_roots or []
    thumb_cache_dir = thumb_cache_dir or os.path.join(os.path.dirname(db_path), "thumb_cache")
    if progress is None:
        progress = lambda phase, current, total: None

    # Build into a temp file and swap it into place atomically at the end, so
    # anything reading db_path concurrently (e.g. the status endpoint) only
    # ever sees a fully-formed database -- the old one, or the finished new
    # one -- never a half-built one with no tables in it yet.
    build_path = db_path + ".building"
    if os.path.exists(build_path):
        os.remove(build_path)
    conn = sqlite3.connect(build_path)
    conn.execute("""
        CREATE TABLE items (
            id INTEGER PRIMARY KEY,
            catalog TEXT,
            accessno TEXT,
            title TEXT,
            body TEXT,
            image_path TEXT,
            thumb_path TEXT,
            detail_json TEXT
        )
    """)
    conn.execute("""
        CREATE VIRTUAL TABLE items_fts USING fts5(
            title, body, accessno,
            content='items', content_rowid='id'
        )
    """)

    def resolve_pastperfect_image(rec):
        raw = (rec.get("IMAGEFILE") or "").strip()
        if not raw:
            return None
        rel = raw.replace("\\", os.sep).replace("/", os.sep)
        full = os.path.join(images_dir, rel)
        if os.path.isfile(full):
            return full
        # PastPerfect records some filenames with a trailing dot and no
        # extension ("008\2012231." for a file actually named "2012231").
        # Windows, where those records were written, strips trailing dots, so
        # they resolve there; every other platform takes the dot literally and
        # the image looks missing though it is sitting right beside its
        # neighbours. Tried only after the exact name, so a file genuinely
        # named with a trailing dot still wins.
        head, tail = os.path.split(full)
        trimmed = tail.rstrip(".")
        if trimmed and trimmed != tail:
            candidate = os.path.join(head, trimmed)
            if os.path.isfile(candidate):
                return candidate
        return None

    def index_table(catalog_label, filename, title_fields, body_fields, detail_fields, id_field="OBJECTID", fallback_id_field="ACCESSNO"):
        progress(f"Reading {catalog_label} records", None, None)
        path = find_file(data_dir, filename)
        if not path:
            print(f"  skip {filename}: not found")
            return 0
        t = DBFTable(path)
        n = 0
        rows = []
        for rec in t.records():
            title = " ".join(str(rec.get(f) or "").strip() for f in title_fields).strip()
            if not title:
                continue
            # OBJECTID is the full, unique per-item catalog number (e.g. "2010.2.7").
            # ACCESSNO is only the accession *lot* it arrived in and can be shared
            # by many separate objects, so it's not a safe stand-in for the real ID.
            object_id = str(rec.get(id_field) or "").strip() if id_field else ""
            if not object_id and fallback_id_field:
                object_id = str(rec.get(fallback_id_field) or "").strip()
            body = "\n".join(str(rec.get(f) or "").strip() for f in body_fields if rec.get(f))
            image_path = resolve_pastperfect_image(rec)
            thumb_path = get_preview_jpeg(image_path, thumb_cache_dir) if image_path else None
            detail = {f: rec.get(f) for f in detail_fields if str(rec.get(f) or "").strip()}
            rows.append((catalog_label, object_id, title, body, image_path, thumb_path, json.dumps(detail)))
            n += 1
        t.close()
        conn.executemany(
            "INSERT INTO items (catalog, accessno, title, body, image_path, thumb_path, detail_json) VALUES (?, ?, ?, ?, ?, ?, ?)",
            rows
        )
        print(f"  {catalog_label}: {n} records ({sum(1 for r in rows if r[4])} with image)")
        return n

    def index_photo_archive_root(source_label, root_path):
        """Index a standalone folder of photos that is NOT part of PastPerfect.
        These are indexed purely by filename/folder text -- deliberately no
        accession number is assigned, since any resemblance to a PastPerfect
        ID in a filename is not a verified link."""
        if not os.path.isdir(root_path):
            print(f"  skip {source_label}: not found at {root_path}")
            return 0

        progress(f"Scanning {source_label}…", None, None)
        all_files = []
        for dirpath, _dirnames, filenames in os.walk(root_path):
            rel_dir = os.path.relpath(dirpath, root_path)
            category = None if rel_dir == "." else rel_dir.split(os.sep)[0]
            for fname in filenames:
                if os.path.splitext(fname)[1].lower() in IMAGE_EXTENSIONS:
                    all_files.append((dirpath, fname, category))
        total_files = len(all_files)

        n = 0
        rows = []
        for i, (dirpath, fname, category) in enumerate(all_files, start=1):
            if i % 10 == 0 or i == total_files:
                progress(source_label, i, total_files)
            full_path = os.path.join(dirpath, fname)
            name_no_ext = os.path.splitext(fname)[0]
            readable_name = name_no_ext.replace("_", " ").replace("-", " ").strip()
            title = readable_name or fname
            body_parts = [source_label]
            if category:
                body_parts.append(category)
            body = "\n".join(body_parts)
            thumb_path = get_preview_jpeg(full_path, thumb_cache_dir)
            detail = {
                "FOLDER": source_label,
                "CATEGORY": category or "",
                "FILENAME": fname,
            }
            detail = {k: v for k, v in detail.items() if v}
            rows.append((source_label, "", title, body, full_path, thumb_path, json.dumps(detail)))
            n += 1
        conn.executemany(
            "INSERT INTO items (catalog, accessno, title, body, image_path, thumb_path, detail_json) VALUES (?, ?, ?, ?, ?, ?, ?)",
            rows
        )
        print(f"  {source_label}: {n} files ({sum(1 for r in rows if r[5])} with preview)")
        return n

    total = 0
    for label, filename, title_fields, body_fields, detail_fields in CATALOG_TABLES:
        total += index_table(label, filename, title_fields, body_fields, detail_fields)

    label, filename, title_fields, body_fields, detail_fields = PEOPLE_TABLE
    total += index_table(label, filename, title_fields, body_fields, detail_fields)

    for root in photo_archive_roots:
        total += index_photo_archive_root(root["label"], root["path"])

    progress("Finishing up…", None, None)
    conn.execute("INSERT INTO items_fts (rowid, title, body, accessno) SELECT id, title, body, accessno FROM items")
    conn.commit()
    conn.close()
    os.replace(build_path, db_path)  # atomic on both POSIX and Windows
    print(f"Total indexed: {total} records -> {db_path}")


if __name__ == "__main__":
    data_dir = sys.argv[1] if len(sys.argv) > 1 else "."
    db_path = sys.argv[2] if len(sys.argv) > 2 else "search_index.db"
    images_dir = sys.argv[3] if len(sys.argv) > 3 else os.path.join(os.path.dirname(os.path.normpath(data_dir)), "Images")
    print(f"Images dir: {images_dir}")
    build(data_dir, db_path, images_dir)
