"""Shared, cached JPEG-preview generation for any source image (JPEG, TIFF,
BMP, PNG, ...). Browsers can't render TIFF/BMP inline, and serving multi-
megabyte originals just to show a 72px list thumbnail is wasteful, so every
image gets a small cached JPEG stand-in the UI can always display."""
import os
import hashlib
from PIL import Image, UnidentifiedImageError

# These are large but legitimate historical scans, not decompression bombs.
Image.MAX_IMAGE_PIXELS = None


def cache_path_for(original_path, cache_dir):
    # Hash the canonical (absolute, symlink- and ".."-resolved) path, not the
    # raw string -- otherwise two different-but-equivalent ways of writing
    # the same path (e.g. after moving the code and changing how many "../"
    # segments reach the data folder) hash differently and silently miss the
    # cache, forcing every image to be reprocessed for no real reason.
    canonical = os.path.realpath(original_path)
    h = hashlib.sha1(canonical.encode("utf-8")).hexdigest()
    return os.path.join(cache_dir, h[:2], h + ".jpg")


def get_preview_jpeg(original_path, cache_dir, max_dim=1600):
    """Return a cached, browser-safe JPEG preview path for original_path,
    generating it if missing or stale. Returns None if unreadable as an image."""
    out_path = cache_path_for(original_path, cache_dir)
    try:
        src_mtime = os.path.getmtime(original_path)
    except OSError:
        return None
    if os.path.exists(out_path) and os.path.getmtime(out_path) >= src_mtime:
        return out_path
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    try:
        with Image.open(original_path) as im:
            im = im.convert("RGB")
            im.thumbnail((max_dim, max_dim))
            im.save(out_path, "JPEG", quality=85)
        return out_path
    except (UnidentifiedImageError, OSError, ValueError):
        return None
