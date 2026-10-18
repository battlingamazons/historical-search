# Historical Society Search

A dead-simple search tool over a PastPerfect museum catalogue. Type what you are
looking for and it finds it, shows the picture, and gives you the full object ID
so the item can actually be found on a shelf.

It is a native desktop application. There is no web server, no open port, and
nothing is reachable from another computer — it reads the collection from local
folders and keeps its own search index.

## PDFs

Folder Settings also takes **PDF folders**. Every PDF under one (subfolders
included) is listed, and searched by the words inside it as well as by its name
and folder. A result shows the passage that matched, and *Open this PDF* opens
the file in whatever the computer uses for PDFs. The files themselves are only
ever read.

- **Scanned PDFs** — ones that are only pictures of pages — have no text to
  read. They are listed and found by name, and the status line says how many
  there are. Reading them would need text recognition (OCR), which the app does
  not do yet.
- **Each PDF is read by a second copy of the program** with a time limit, not
  inside the window. PDFs come from every kind of software, and one that makes
  the reader crash or hang then costs that one file its text and nothing more.
- Text extraction uses a patched copy of the `pdf-extract` crate in
  `native/historical-search/vendor/`. The upstream release silently dropped
  any line written with the `'` or `"` operators; see the notice in that
  folder.

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
