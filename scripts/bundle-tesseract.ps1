# Puts a copy of Tesseract (the text recognition program) into
# native/historical-search/src-tauri/ocr, which the Windows installer then
# carries. Run on a Windows machine, from the repository root, before building.
#
# Tesseract is built here, statically, with vcpkg -- rather than taking a
# ready-made Windows build. The ready-made one is a hundred megabytes of
# separate DLLs, a third of it character data, because it also carries the
# programs for *training* Tesseract (which pull in drawing, text-layout and
# internationalisation libraries, several of them under the LGPL and GPL). The
# application only reads pages. Built without those, Tesseract is one
# executable of a few megabytes whose libraries are all permissively licensed,
# and the notices that must accompany it are short.
#
# vcpkg is on GitHub's Windows runners already. Its builds are cached by the
# caller, so the first run takes a quarter of an hour and later ones seconds.

$ErrorActionPreference = 'Stop'

$triplet = 'x64-windows-static'
$vcpkg = if ($env:VCPKG_INSTALLATION_ROOT) { $env:VCPKG_INSTALLATION_ROOT } else { 'C:\vcpkg' }
$cache = if ($env:HS_VCPKG_CACHE) { $env:HS_VCPKG_CACHE } else { Join-Path $env:USERPROFILE 'vcpkg-binary-cache' }
New-Item -ItemType Directory -Force -Path $cache | Out-Null

& "$vcpkg\vcpkg.exe" install "tesseract:$triplet" --binarysource="clear;files,$cache,readwrite" --clean-after-build
if ($LASTEXITCODE -ne 0) { throw "vcpkg could not build Tesseract ($LASTEXITCODE)" }

$installed = "$vcpkg\installed\$triplet"
$exe = "$installed\tools\tesseract\tesseract.exe"
if (-not (Test-Path $exe)) { throw "vcpkg did not produce $exe" }

$target = 'native\historical-search\src-tauri\ocr'
New-Item -ItemType Directory -Force -Path "$target\tessdata", "$target\licenses" | Out-Null
Copy-Item $exe $target

# English language data: the "fast" models, the same size and kind that
# Homebrew and Debian ship. Pinned to a release so the build does not change
# under us.
Invoke-WebRequest -UseBasicParsing `
    -Uri 'https://github.com/tesseract-ocr/tessdata_fast/raw/4.1.0/eng.traineddata' `
    -OutFile "$target\tessdata\eng.traineddata"

# What went into it, as vcpkg built it: its own record of every library, and
# each one's licence text. Shipped with the program, and printed here so the
# third-party notices can be checked against it.
& "$vcpkg\vcpkg.exe" list --x-install-root="$vcpkg\installed" | Where-Object { $_ -match ":$triplet" } | Tee-Object -FilePath "$target\licenses\libraries.txt"
Get-ChildItem "$installed\share" -Directory | ForEach-Object {
    $copyright = Join-Path $_.FullName 'copyright'
    if (Test-Path $copyright) { Copy-Item $copyright (Join-Path "$target\licenses" ($_.Name + '.txt')) }
}

# Prove the copy works on its own, from where it will live, before it is
# packed into an installer that would fail on the society's machine instead.
$env:TESSDATA_PREFIX = (Resolve-Path "$target\tessdata").Path
& "$target\tesseract.exe" --version
if ($LASTEXITCODE -ne 0) { throw "The built Tesseract does not run" }
$languages = & "$target\tesseract.exe" --list-langs
if ($languages -notcontains 'eng') { throw "The built Tesseract has no English data" }

Write-Host "Files in the bundled Tesseract:"
Get-ChildItem $target -Recurse -File | ForEach-Object {
    Write-Host ("  {0,12:N0}  {1}" -f $_.Length, $_.FullName.Substring((Resolve-Path $target).Path.Length + 1))
}
