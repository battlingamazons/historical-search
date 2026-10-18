#!/usr/bin/env python3
"""Writes THIRD-PARTY-NOTICES.md: the licences of everything this application
carries or is built from, as those licences require them to be reproduced.

Two parts. The Rust libraries come from `cargo metadata`, with each crate's own
licence files read from its source (so the text and copyright line are the
authors', not ours); where a crate ships none, the standard text of its
licence is used with its listed authors. Everything else -- Tesseract and the
libraries it carries on Windows -- is described in `third-party/`.

Run from the repository root:  python3 scripts/generate-notices.py
CI runs it and fails if the committed file is out of date, so it cannot
quietly fall behind the dependencies.
"""
import hashlib
import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MANIFEST = os.path.join(ROOT, "native", "historical-search", "src-tauri", "Cargo.toml")
OUT = os.path.join(ROOT, "THIRD-PARTY-NOTICES.md")

# Standard texts, for crates that carry no licence file of their own.
STANDARD = {
    "MIT": """Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.""",
    "BSD-3-Clause": """Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its contributors
   may be used to endorse or promote products derived from this software
   without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.""",
    "Zlib": """This software is provided 'as-is', without any express or implied warranty. In
no event will the authors be held liable for any damages arising from the use
of this software.

Permission is granted to anyone to use this software for any purpose,
including commercial applications, and to alter it and redistribute it freely,
subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not claim
   that you wrote the original software. If you use this software in a
   product, an acknowledgment in the product documentation would be
   appreciated but is not required.
2. Altered source versions must be plainly marked as such, and must not be
   misrepresented as being the original software.
3. This notice may not be removed or altered from any source distribution.""",
}


# The libraries the Windows copy of Tesseract reports being built with
# (`tesseract --version`), and the MinGW-w64 runtime every such build needs.
# (component, licence as applied here, note, licence files in third-party/windows)
WINDOWS = [
    ("giflib", "MIT", "", ["giflib-COPYING.txt"]),
    ("libjpeg-turbo", "IJG licence, BSD-3-Clause and zlib licence", "", ["libjpeg-turbo-LICENSE.md.txt", "libjpeg-turbo-README.ijg.txt"]),
    ("libpng", "libpng licence", "", ["libpng-LICENSE.md.txt"]),
    ("libtiff", "libtiff licence (BSD-style)", "", ["libtiff-LICENSE.md.txt"]),
    ("zlib", "zlib licence", "", ["zlib-LICENSE.txt"]),
    ("libwebp", "BSD-3-Clause", "", ["libwebp-COPYING.txt"]),
    ("OpenJPEG (libopenjp2)", "BSD-2-Clause", "", ["openjpeg-LICENSE.txt"]),
    ("libarchive", "BSD-2-Clause", "", ["libarchive-COPYING.txt"]),
    ("xz (liblzma)", "BSD Zero Clause / public domain", "", ["xz-COPYING.txt"]),
    ("bzip2", "bzip2 licence (BSD-style)", "", ["bzip2-COPYING.txt"]),
    ("LZ4", "BSD-2-Clause", "library licence", ["lz4-LICENSE.txt"]),
    ("Zstandard", "BSD-3-Clause", "offered as BSD-3-Clause or GPL-2.0; used under BSD-3-Clause", ["zstd-LICENSE.txt"]),
    ("Expat", "MIT", "", ["expat-COPYING.txt"]),
    ("libb2 (BLAKE2)", "CC0-1.0", "offered as CC0-1.0, Apache-2.0 or OpenSSL; used under CC0-1.0", ["libb2-COPYING.txt"]),
    ("curl (libcurl)", "curl licence (MIT-style)", "uses Windows' own TLS (Schannel); no TLS library is bundled", ["curl-COPYING.txt"]),
    ("Brotli", "MIT", "", ["brotli-LICENSE.txt"]),
    ("libidn2", "LGPL-3.0-or-later", "see the note on replacement below; offered as GPL-2.0-or-later or LGPL-3.0-or-later", ["libidn2-COPYING.LESSERv3.txt", "gcc-COPYING3.txt"]),
    ("libpsl", "MIT", "", ["libpsl-COPYING.txt"]),
    ("libssh2", "BSD-3-Clause", "", ["libssh2-COPYING.txt"]),
    ("MinGW-w64 runtime (libgcc, libstdc++)", "GPL-3.0 with the GCC Runtime Library Exception", "the exception permits distributing programs built with it under their own licence", ["gcc-COPYING.RUNTIME.txt", "gcc-COPYING3.txt"]),
    ("winpthreads", "MIT and BSD-style", "part of MinGW-w64", ["winpthreads-COPYING.txt"]),
]


def read(path):
    with open(path, "rb") as f:
        return f.read().decode("utf-8", errors="replace").replace("\r\n", "\n").strip()


def license_files(pkg_dir):
    found = []
    for name in sorted(os.listdir(pkg_dir)):
        if re.match(r"(?i)^(licen[cs]e|copying|notice|unlicense)", name) and os.path.isfile(os.path.join(pkg_dir, name)):
            found.append(os.path.join(pkg_dir, name))
    return found


def required_licenses(expression):
    """The licences whose text must accompany a crate: every term joined by AND,
    and one choice from each OR (MIT where offered, else Apache-2.0, else the
    first)."""
    terms = []
    for part in re.split(r"\s+AND\s+", expression.replace("/", " OR ")):
        part = part.strip("() ")
        options = [o.strip("() ") for o in re.split(r"\s+OR\s+", part)]
        options = [o.split(" WITH ")[0] for o in options]
        for preferred in ("MIT", "Apache-2.0"):
            if preferred in options:
                terms.append(preferred)
                break
        else:
            terms.append(options[0])
    return terms


def main():
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--manifest-path", MANIFEST],
        check=True, capture_output=True, text=True).stdout)
    workspace = set(meta["workspace_members"])
    packages = sorted(
        (p for p in meta["packages"] if p["id"] not in workspace),
        key=lambda p: (p["name"].lower(), p["version"]))

    texts = {}      # hash -> [text, [crates]]
    listing = []
    for p in packages:
        label = f'{p["name"]} {p["version"]}'
        expression = p.get("license") or "unknown"
        listing.append((label, expression, p.get("repository") or p.get("homepage") or ""))
        pkg_dir = os.path.dirname(p["manifest_path"])
        files = license_files(pkg_dir)
        bodies = [read(f) for f in files]
        # A notice file alone (the patched copy of pdf-extract has one) is not
        # the licence, so the standard text is still added below.
        has_licence = any(
            re.match(r"(?i)^(licen[cs]e|copying|unlicense)", os.path.basename(f)) for f in files)
        if not has_licence:
            holders = ", ".join(a.split("<")[0].strip() for a in p.get("authors", []) if a.strip())
            holders = holders or f'the {p["name"]} authors'
            for term in required_licenses(expression):
                if term in STANDARD:
                    bodies.append(f"Copyright (c) {holders}\n\n{STANDARD[term]}")
                else:
                    bodies.append(f"Licensed under {term}. See {p.get('repository') or 'the crate'} for the text.")
        for body in bodies:
            key = hashlib.sha1(body.encode()).hexdigest()
            texts.setdefault(key, [body, []])[1].append(label)

    out = []
    out.append(open(os.path.join(ROOT, "third-party", "HEADER.md")).read().rstrip())
    out.append("\n\n## Text recognition: Tesseract\n")
    out.append(open(os.path.join(ROOT, "third-party", "TESSERACT.md")).read().rstrip())
    for title, name in (("Tesseract and the English language data (Apache License 2.0)", "tesseract-LICENSE.txt"),
                        ("Leptonica", "leptonica-LICENSE.txt")):
        out.append(f"\n\n### {title}\n\n```\n{read(os.path.join(ROOT, 'third-party', name))}\n```")

    out.append("\n\n## Windows bundle: libraries inside the copy of Tesseract\n")
    out.append(open(os.path.join(ROOT, "third-party", "WINDOWS.md")).read().rstrip())
    out.append("\n\n| Component | Licence | Notes |\n|---|---|---|")
    for name, licence, note, _files in WINDOWS:
        out.append(f"| {name} | {licence} | {note} |")
    shown = set()
    for name, licence, note, files in WINDOWS:
        for fname in files:
            if fname in shown:
                continue
            shown.add(fname)
            out.append(f"\n\n#### {name} -- `{fname}`\n\n```\n{read(os.path.join(ROOT, 'third-party', 'windows', fname))}\n```")

    out.append("\n\n## Rust libraries\n")
    out.append(f"This application is built from {len(packages)} Rust libraries "
               "(crates), all obtained from crates.io, except `pdf-extract`, a patched copy "
               "kept in `native/historical-search/vendor/`. Each is listed with its licence; "
               "the licence texts follow, each with the libraries it applies to.\n")
    out.append("| Library | Licence | Source |\n|---|---|---|")
    for label, expression, repo in listing:
        out.append(f"| {label} | {expression} | {repo} |")

    out.append("\n\n### Licence texts\n")
    ordered = sorted(texts.values(), key=lambda t: t[1][0].lower())
    for body, crates in ordered:
        out.append(f"\n#### Applies to: {', '.join(crates)}\n\n```\n{body}\n```")
    out.append("")
    with open(OUT, "w", encoding="utf-8") as f:
        f.write("\n".join(out))
    print(f"wrote {OUT}: {len(packages)} libraries, {len(texts)} distinct licence texts")


if __name__ == "__main__":
    sys.exit(main())
