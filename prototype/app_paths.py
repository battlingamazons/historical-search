"""Where this app's runtime state lives -- deliberately NOT inside the source
tree. config.json (your local folder settings), search_index.db (generated
from your data), and thumb_cache/ (generated preview images) are all produced
by *running* the app, not by building it, so none of them belong alongside
the source code: they'd otherwise end up looking like tracked project files
in git, and in config.json's case would literally bake one person's local
filesystem paths into what's supposed to be shareable source.

Instead they live in the same per-user, OS-standard locations any well-
behaved desktop app uses -- Application Support / Caches on macOS,
%LOCALAPPDATA% on Windows, XDG dirs on Linux.
"""
import os
import sys

APP_NAME = "Historical Society Search"


def _make(path):
    os.makedirs(path, exist_ok=True)
    return path


def data_dir():
    """Persistent app state: config.json, search_index.db."""
    if sys.platform == "darwin":
        base = os.path.expanduser("~/Library/Application Support")
    elif sys.platform == "win32":
        base = os.environ.get("LOCALAPPDATA", os.path.expanduser("~"))
    else:
        base = os.environ.get("XDG_DATA_HOME", os.path.expanduser("~/.local/share"))
    return _make(os.path.join(base, APP_NAME))


def cache_dir():
    """Disposable, regenerable state: thumb_cache/. Safe to delete anytime --
    the app will just rebuild whatever it needs on the next refresh."""
    if sys.platform == "darwin":
        base = os.path.expanduser("~/Library/Caches")
    elif sys.platform == "win32":
        base = os.environ.get("LOCALAPPDATA", os.path.expanduser("~"))
    else:
        base = os.environ.get("XDG_CACHE_HOME", os.path.expanduser("~/.cache"))
    return _make(os.path.join(base, APP_NAME))
