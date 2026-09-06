"""Test helper only: runs the real, already-proven ingest.py build() against
real data (reusing the real thumbnail cache for speed -- cache hits, not
regeneration) and dumps the resulting `items` rows as JSON, so the Rust port
can be compared against it.

Usage: python3 dump_ingest.py <prototype_dir> <data_dir> <images_dir> <thumb_cache_dir> <photo_roots_json> <out_db_path>
"""
import sys
import os
import json
import sqlite3
import contextlib

prototype_dir, data_dir, images_dir, thumb_cache_dir, photo_roots_json, out_db_path = sys.argv[1:7]
sys.path.insert(0, prototype_dir)
import ingest

photo_roots = json.loads(photo_roots_json)
# ingest.build() prints its own human-readable progress lines to stdout;
# redirect those elsewhere so the only thing on real stdout is our JSON.
with open(os.devnull, "w") as devnull:
    with contextlib.redirect_stdout(devnull):
        ingest.build(data_dir, out_db_path, images_dir, photo_roots, thumb_cache_dir)

conn = sqlite3.connect(out_db_path)
conn.row_factory = sqlite3.Row
rows = conn.execute(
    "SELECT catalog, accessno, title, body, image_path IS NOT NULL AS has_image, "
    "thumb_path IS NOT NULL AS has_thumb, detail_json FROM items ORDER BY id"
).fetchall()
conn.close()

print(json.dumps([dict(r) for r in rows]))
