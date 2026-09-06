"""Tiny local search server for the Historical Society Search app. Stdlib only."""
import sqlite3
import json
import os
import re
import time
import threading
import socketserver
import mimetypes
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

import ingest
import app_paths
from ingest import FIELD_LABELS

BASE_DIR = os.path.dirname(__file__)  # source tree -- only static/ lives here now
STATIC_DIR = os.path.join(BASE_DIR, "static")

DB_PATH = os.path.join(app_paths.data_dir(), "search_index.db")
CONFIG_PATH = os.path.join(app_paths.data_dir(), "config.json")
THUMB_CACHE_DIR = os.path.join(app_paths.cache_dir(), "thumb_cache")

DEFAULT_PAGE_SIZE = 40
MAX_ALL_RESULTS = 2000  # safety cap for "show everything" -- still a lot more than 40

DEFAULT_CONFIG = {"data_dir": "", "images_dir": "", "photo_archive_roots": []}

if not os.path.exists(CONFIG_PATH):
    with open(CONFIG_PATH, "w") as f:
        json.dump(DEFAULT_CONFIG, f, indent=2)

with open(CONFIG_PATH) as f:
    CONFIG = json.load(f)

refresh_lock = threading.Lock()
refresh_state = {"in_progress": False, "last_error": None, "phase": None, "current": None, "total": None}


def save_config(new_config):
    global CONFIG
    with open(CONFIG_PATH, "w") as f:
        json.dump(new_config, f, indent=2)
    CONFIG = new_config


def validate_path(kind, path):
    if not path or not path.strip():
        return {"ok": False, "message": "No path given."}
    # Paths are expected to be absolute (the native folder picker always
    # returns one). There's no longer a source-tree location to sensibly
    # resolve a relative path against, so this just falls back to whatever
    # the process's current directory happens to be.
    resolved = os.path.abspath(path)
    if not os.path.isdir(resolved):
        return {"ok": False, "message": "Folder not found."}

    if kind == "pastperfect_data":
        found = [label for label, filename, *_ in ingest.CATALOG_TABLES + [ingest.PEOPLE_TABLE]
                 if ingest.find_file(resolved, filename)]
        if not found:
            return {"ok": False, "message": "Folder exists, but no PastPerfect tables (OBJECTS.DBF etc.) found in it."}
        return {"ok": True, "message": f"Found {len(found)} PastPerfect table(s): {', '.join(found)}."}

    if kind == "photo_folder":
        count = 0
        for _dirpath, _dirnames, filenames in os.walk(resolved):
            count += sum(1 for f in filenames if os.path.splitext(f)[1].lower() in ingest.IMAGE_EXTENSIONS)
        if count == 0:
            return {"ok": False, "message": "Folder exists, but no image files found in it (checked subfolders too)."}
        return {"ok": True, "message": f"Found {count:,} image file(s)."}

    # generic folder (e.g. PastPerfect Images folder) -- existing is enough
    return {"ok": True, "message": "Folder found."}


def get_status():
    exists = os.path.exists(DB_PATH)
    count = 0
    if exists:
        try:
            conn = sqlite3.connect(DB_PATH)
            count = conn.execute("SELECT COUNT(*) FROM items").fetchone()[0]
            conn.close()
        except sqlite3.Error:
            # The index is mid-rebuild (or otherwise transiently unreadable) --
            # this is expected while in_progress is true, not a real failure.
            pass
    return {
        "last_updated": os.path.getmtime(DB_PATH) if exists else None,
        "record_count": count,
        "data_dir": CONFIG["data_dir"],
        "in_progress": refresh_state["in_progress"],
        "last_error": refresh_state["last_error"],
        "phase": refresh_state["phase"],
        "phase_current": refresh_state["current"],
        "phase_total": refresh_state["total"],
    }


def _on_progress(phase, current, total):
    refresh_state["phase"] = phase
    refresh_state["current"] = current
    refresh_state["total"] = total


def run_refresh():
    if not refresh_lock.acquire(blocking=False):
        return False  # a refresh is already running
    try:
        refresh_state["in_progress"] = True
        refresh_state["last_error"] = None
        refresh_state["phase"] = "Starting…"
        refresh_state["current"] = None
        refresh_state["total"] = None
        data_dir = CONFIG["data_dir"]
        images_dir = CONFIG.get("images_dir") or os.path.join(os.path.dirname(os.path.normpath(data_dir)), "Images")
        photo_archive_roots = [r for r in CONFIG.get("photo_archive_roots", []) if r.get("path")]
        ingest.build(data_dir, DB_PATH, images_dir, photo_archive_roots, THUMB_CACHE_DIR, progress=_on_progress)
        return True
    except Exception as e:
        refresh_state["last_error"] = str(e)
        return False
    finally:
        refresh_state["in_progress"] = False
        refresh_state["phase"] = None
        refresh_state["current"] = None
        refresh_state["total"] = None
        refresh_lock.release()


def start_refresh_async():
    if refresh_state["in_progress"]:
        return False
    refresh_state["in_progress"] = True  # set immediately so a fast poll never misses it
    threading.Thread(target=run_refresh, daemon=True).start()
    return True


def fts_query(raw_query):
    terms = re.findall(r"\w+", raw_query)
    if not terms:
        return ""
    return " ".join(f'"{t}"*' for t in terms)


def search(q, limit=DEFAULT_PAGE_SIZE, offset=0):
    if not q.strip():
        return [], False
    conn = sqlite3.connect(DB_PATH)
    conn.row_factory = sqlite3.Row
    match = fts_query(q)
    try:
        # Fetch one extra row so we can tell whether there's a next page
        # without a separate (and, for a broad match, potentially slow) COUNT(*).
        rows = conn.execute(
            """
            SELECT items.id, items.catalog, items.accessno, items.title, items.body,
                   items.image_path, items.thumb_path,
                   bm25(items_fts) AS rank
            FROM items_fts
            JOIN items ON items.id = items_fts.rowid
            WHERE items_fts MATCH ?
            ORDER BY rank
            LIMIT ? OFFSET ?
            """,
            (match, limit + 1, offset),
        ).fetchall()
    except sqlite3.OperationalError:
        rows = []
    conn.close()
    has_more = len(rows) > limit
    rows = rows[:limit]
    results = []
    for r in rows:
        snippet = (r["body"] or "")[:220]
        results.append({
            "id": r["id"],
            "catalog": r["catalog"],
            "accessno": r["accessno"],
            "title": r["title"],
            "snippet": snippet,
            "has_image": bool(r["thumb_path"] or r["image_path"]),
        })
    return results, has_more


def get_item(item_id):
    conn = sqlite3.connect(DB_PATH)
    conn.row_factory = sqlite3.Row
    row = conn.execute(
        "SELECT id, catalog, accessno, title, image_path, thumb_path, detail_json FROM items WHERE id = ?",
        (item_id,),
    ).fetchone()
    conn.close()
    if not row:
        return None
    detail = json.loads(row["detail_json"] or "{}")
    fields = [
        {"label": FIELD_LABELS.get(k, k), "value": str(v)}
        for k, v in detail.items()
        if str(v).strip()
    ]
    return {
        "id": row["id"],
        "catalog": row["catalog"],
        "accessno": row["accessno"],
        "title": row["title"],
        "has_image": bool(row["thumb_path"]),
        "has_original": bool(row["image_path"]),
        "fields": fields,
    }


def get_paths(item_id):
    conn = sqlite3.connect(DB_PATH)
    row = conn.execute(
        "SELECT image_path, thumb_path, title, accessno FROM items WHERE id = ?", (item_id,)
    ).fetchone()
    conn.close()
    if not row:
        return None
    return {"image_path": row[0], "thumb_path": row[1], "title": row[2], "accessno": row[3]}


def safe_download_filename(item_info, original_path):
    ext = os.path.splitext(original_path)[1]
    base = item_info["accessno"] or item_info["title"] or "image"
    safe = re.sub(r"[^\w\s.,()-]", "", base).strip() or "image"
    return f"{safe}{ext}"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def address_string(self):
        # Avoid a reverse-DNS lookup (slow, and pointless) on every request.
        return self.client_address[0]

    def _json(self, obj, status=200):
        body = json.dumps(obj).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _read_json_body(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length) if length else b"{}"
        return json.loads(raw or b"{}")

    def do_POST(self):
        parsed = urlparse(self.path)
        if parsed.path == "/api/refresh":
            started = start_refresh_async()
            status = get_status()
            status["started"] = started
            self._json(status)
            return
        if parsed.path == "/api/config":
            try:
                new_config = self._read_json_body()
            except (json.JSONDecodeError, ValueError):
                self._json({"error": "invalid JSON"}, 400)
                return
            save_config(new_config)
            self._json({"saved": True})
            return
        self.send_response(404)
        self.end_headers()

    def do_GET(self):
        parsed = urlparse(self.path)

        if parsed.path == "/api/status":
            self._json(get_status())
            return

        if parsed.path == "/api/config":
            self._json(CONFIG)
            return

        if parsed.path == "/api/validate":
            qs = parse_qs(parsed.query)
            kind = qs.get("kind", [""])[0]
            path = qs.get("path", [""])[0]
            self._json(validate_path(kind, path))
            return

        if parsed.path == "/api/search":
            qs = parse_qs(parsed.query)
            q = qs.get("q", [""])[0]
            offset = int(qs.get("offset", ["0"])[0])
            show_all = qs.get("all", ["0"])[0] == "1"
            limit = MAX_ALL_RESULTS if show_all else DEFAULT_PAGE_SIZE
            results, has_more = search(q, limit=limit, offset=offset)
            self._json({"results": results, "has_more": has_more})
            return

        m = re.match(r"^/api/item/(\d+)$", parsed.path)
        if m:
            item = get_item(int(m.group(1)))
            if item is None:
                self._json({"error": "not found"}, 404)
            else:
                self._json(item)
            return

        m = re.match(r"^/thumb/(\d+)$", parsed.path)
        if m:
            paths = get_paths(int(m.group(1)))
            thumb = paths and paths["thumb_path"]
            if not thumb or not os.path.isfile(thumb):
                self.send_response(404)
                self.end_headers()
                return
            with open(thumb, "rb") as f:
                data = f.read()
            self.send_response(200)
            self.send_header("Content-Type", "image/jpeg")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "public, max-age=86400")
            self.end_headers()
            self.wfile.write(data)
            return

        m = re.match(r"^/original/(\d+)$", parsed.path)
        if m:
            paths = get_paths(int(m.group(1)))
            original = paths and paths["image_path"]
            if not original or not os.path.isfile(original):
                self.send_response(404)
                self.end_headers()
                return
            qs = parse_qs(parsed.query)
            download = qs.get("download", ["0"])[0] == "1"
            ctype = mimetypes.guess_type(original)[0] or "application/octet-stream"
            with open(original, "rb") as f:
                data = f.read()
            self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(data)))
            if download:
                filename = safe_download_filename(paths, original)
                self.send_header("Content-Disposition", f'attachment; filename="{filename}"')
            else:
                self.send_header("Cache-Control", "public, max-age=86400")
            self.end_headers()
            self.wfile.write(data)
            return

        path = parsed.path
        if path == "/":
            path = "/index.html"
        file_path = os.path.join(STATIC_DIR, path.lstrip("/"))
        if os.path.commonpath([os.path.abspath(file_path), STATIC_DIR]) != STATIC_DIR:
            self.send_response(403)
            self.end_headers()
            return
        if not os.path.isfile(file_path):
            self.send_response(404)
            self.end_headers()
            return
        ctype = "text/html" if file_path.endswith(".html") else \
                "text/css" if file_path.endswith(".css") else \
                "application/javascript" if file_path.endswith(".js") else \
                "application/octet-stream"
        with open(file_path, "rb") as f:
            data = f.read()
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class Server(ThreadingHTTPServer):
    def server_bind(self):
        # HTTPServer.server_bind() calls socket.getfqdn(), which does a DNS
        # lookup and can hang for a long time (or forever) on machines with
        # no/blocked DNS. We don't need the fully-qualified name, so skip it.
        socketserver.TCPServer.server_bind(self)
        host, port = self.server_address[:2]
        self.server_name = host
        self.server_port = port


if __name__ == "__main__":
    port = 8765
    print(f"Serving http://127.0.0.1:{port}")
    Server(("127.0.0.1", port), Handler).serve_forever()
