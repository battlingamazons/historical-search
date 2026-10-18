# pdf.js

[PDF.js](https://github.com/mozilla/pdf.js) 5.7.284 (legacy build), Apache
License 2.0 -- see `LICENSE`. Unmodified `pdf.min.mjs` and
`pdf.worker.min.mjs` from the `pdfjs-dist` package on npm, kept here so the
window can show a PDF page without any network access.

To update: `npm pack pdfjs-dist@<version>`, copy `legacy/build/pdf.min.mjs`,
`legacy/build/pdf.worker.min.mjs` and `LICENSE` over these, change the version
above and in `scripts/generate-notices.py`, and rerun that script.
