# Historical Society Search

A dead-simple search tool over a PastPerfect museum catalogue. Type what you are
looking for and it finds it, shows the picture, and gives you the full object ID
so the item can actually be found on a shelf.

It is a native desktop application. There is no web server, no open port, and
nothing is reachable from another computer — it reads the collection from local
folders and keeps its own search index.

## Searching

Type words: every word has to be in the record, and a word matches from its
beginning (*stew* finds *Stewart*). The same box also takes `OR`, `NOT` and
`AND` in capitals, a minus (`school -high`), quotation marks for an exact
phrase (`"town hall"`) and brackets (`mill AND (pond OR creek)`). The
**Search tips** button explains them with examples to click. The operators are
capitals-only so that an ordinary *and* in a name still works as a word, and a
half-typed query is read as far as it goes rather than refused.

## PDFs

Folder Settings also takes **PDF folders**. Every PDF under one (subfolders
included) is listed, and searched by the words inside it as well as by its name
and folder. A result shows the passage that matched, and *Open this PDF* opens
the file in whatever the computer uses for PDFs. The files themselves are only
ever read.

- **Scanned PDFs** — pictures of pages with no text of their own, which is
  most of what a society has — are read with text recognition (OCR) using
  Tesseract. It takes about a second a page, once per file: the recognised text
  is kept (in `ocr_cache` beside the settings, keyed to the file's size, date
  and ends so a renamed or moved file is not read again), and an interrupted
  run picks up from the pages already done. The record says its text was
  recognised and may contain mistakes. Folder Settings has a switch to turn it
  off. Windows builds carry their own Tesseract; on macOS install it with
  `brew install tesseract`, and on Linux the `.deb` depends on `tesseract-ocr`
  (the AppImage needs it installed). Without it, scans are still listed and
  found by name, and the status line says why.
- Scans are read by taking the page picture straight out of the PDF. JPEG
  pictures and Group 4 fax pictures (the usual forms) and uncompressed or
  Flate-compressed grey and colour ones are understood. **JBIG2 and JPEG 2000
  pictures are not yet** — a PDF made only of those is listed with a note
  saying so.
- **Each PDF is read by a second copy of the program** with a time limit, not
  inside the window. PDFs come from every kind of software, and one that makes
  the reader crash or hang then costs that one file its text and nothing more.
- Text extraction uses a patched copy of the `pdf-extract` crate in
  `native/historical-search/vendor/`. The upstream release silently dropped
  any line written with the `'` or `"` operators; see the notice in that
  folder.

## Excel spreadsheets

Folder Settings also takes **Excel spreadsheets** (`.xlsx`, `.xlsm` and the
older `.xls`), either one workbook or a folder of them. **Each row becomes a
record**: it is found by the words in any of its cells, and its card shows the
row under its column headings, with the file, sheet and row number it came from.

- The headings are the first row with two or more cells filled in, so a title
  line above them is kept and searched but not mistaken for headings. A sheet
  with no text headings gets *Column A*, *Column B* and so on.
- A column called *Accession*, *Catalog Number* or *Object ID* is used as the
  record's number. The record's title is its *Title* / *Name* / *Object* /
  *Description* column, or else its first filled cell.
- Hidden sheets are left out (they are usually lookup lists). Formulas are
  shown as the value Excel last saved; dates as `1921-03-04`.
- Like PDFs, a workbook is read by a second copy of the program with a time
  limit, so a damaged one costs only itself. Workbooks over 200 MB, and rows
  past the first 500,000, are left out, and the refresh says so.

## Getting a build

Every push builds installers for Windows, Linux and macOS on GitHub. To get one:

1. Open the **Actions** tab, choose the most recent **Build and test** run.
2. Download the artifact — `historical-search-Windows` (an `.msi` and an
   `.exe` installer) `historical-search-Linux` (a `.deb` and an `.AppImage`) or
   `historical-search-macOS` (a `.tar.gz` of the app).

A build can also be started by hand from the Actions tab with **Run workflow**,
without pushing anything. Tagging a commit `v1.2.3` additionally publishes the
installers as a GitHub release.

On Linux, install the `.deb` with `sudo apt install ./<file>.deb`, or make the
`.AppImage` executable and run it. The `.deb` needs `libwebkit2gtk-4.1`, which
its package manager pulls in. Settings and the index live under
`~/.local/share/Historical Society Search`.

Windows needs the Microsoft **WebView2 runtime**. Windows 11 and up-to-date
Windows 10 have it already; if the app will not start, that is the first thing
to check.

## Layout

    native/historical-search/     the application
      src/                        the interface (plain HTML, no build step)
      src-tauri/                  the Rust behind it
    prototype/                    the original Python version, kept as a
                                  fallback and as the reference the Rust is
                                  tested against

The museum's data lives **outside this repository** and must stay there — the
PastPerfect folder and the photo folders are pointed at from within the app,
under *Folder Settings*. Nothing is ever copied into the source tree, and
`.gitignore` refuses the data file types as a backstop.

The app's own runtime state — settings, the search index, cached previews and
the diagnostics log — lives in the operating system's application-data folders,
never beside the source.

## Working on it

    cd native/historical-search
    npm install
    npx tauri dev             # run it
    npx tauri build           # build it

    cd src-tauri
    cargo test --lib --tests --no-default-features   # only what runs anywhere

    cp test-env.sh.example test-env.sh       # once: point at your collection
    source test-env.sh
    cd src-tauri && cargo test --lib         # everything, including the data

The first form is what CI runs. The `live-data` feature is **on by default**,
so a plain `cargo test` runs the tests that read the real catalogue and fails
loudly, naming the variable to set, if it cannot find it. Turning the feature
off does not make those tests pass without checking anything — it stops them
being compiled at all, because that data is not in this repository.

Nothing in this repository names a particular machine, person or society. The
tests take the location of the collection from `HS_TEST_PP5_DIR` and
`HS_TEST_PHOTOS_DIR`, discover the photo folders rather than naming them, and
search for a sample file to work on rather than hardcoding one. `test-env.sh`,
where those paths live, is gitignored.

## When something goes wrong

The app has a **Diagnostics** button next to Folder Settings. It reports the
platform, the resolved folders, whether each catalogue table opens, the shape of
the index, and a set of checks run on the spot, and it can copy that to the
clipboard or save it to a file. There is also a rolling log beside the settings
file, which records failures the app would otherwise pass over in silence, and
captures crashes.

That report is the right thing to send when reporting a problem, particularly on
Windows, which is the platform least likely to have been exercised directly.

## License

MIT -- see [LICENSE](LICENSE).

The application is built from, and its installers carry, other people's
software under their own licences -- Rust libraries, and on Windows a copy of
the Tesseract text recognition program with its libraries. Their licences and
notices are in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md), which the
installers include. It is generated, not written by hand:
`python3 scripts/generate-notices.py` rebuilds it after a dependency changes,
and CI fails if it is out of date.
