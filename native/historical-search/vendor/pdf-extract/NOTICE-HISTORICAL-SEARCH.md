# Vendored copy of `pdf-extract`

This folder is `pdf-extract` 0.9.0 from crates.io
(<https://github.com/jrmuizel/pdf-extract>), licensed MIT as its `Cargo.toml`
states, with the one change below. It is kept here, rather than fetched, so the
fix does not depend on upstream releasing it.

## The change

`src/lib.rs` now handles the `'` and `"` text operators. Upstream ignores them,
so any line of a PDF drawn with one -- the usual way text-to-PDF converters,
Ghostscript and some older word processors write a block of lines -- was left
out of the extracted text, silently. Look for "Historical Society Search patch".

If upstream fixes this, delete this folder and depend on the crates.io release.
