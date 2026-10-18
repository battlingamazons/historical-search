# Puts a copy of Tesseract (the text recognition program) into
# native/historical-search/src-tauri/ocr, which the Windows installer then
# carries. Run on a Windows machine, from the repository root, before building.
#
# Installs the UB Mannheim build through Chocolatey, which is what Windows
# runners on GitHub have, and copies the program, its libraries and the
# English language data. Nothing else is needed at runtime.

$ErrorActionPreference = 'Stop'

$source = 'C:\Program Files\Tesseract-OCR'
if (-not (Test-Path "$source\tesseract.exe")) {
    choco install tesseract -y --no-progress
    if ($LASTEXITCODE -ne 0) { throw "Installing Tesseract failed ($LASTEXITCODE)" }
}
if (-not (Test-Path "$source\tesseract.exe")) { throw "tesseract.exe is not in $source" }

$target = 'native\historical-search\src-tauri\ocr'
New-Item -ItemType Directory -Force -Path "$target\tessdata" | Out-Null
Copy-Item "$source\tesseract.exe" $target
Copy-Item "$source\*.dll" $target
Copy-Item "$source\tessdata\eng.traineddata" "$target\tessdata"
if (Test-Path "$source\tessdata\configs") {
    Copy-Item "$source\tessdata\configs" "$target\tessdata" -Recurse
}

# Prove the copy works on its own, from where it will live, before it is
# packed into an installer that would fail on the society's machine instead.
$env:TESSDATA_PREFIX = (Resolve-Path "$target\tessdata").Path
& "$target\tesseract.exe" --version
if ($LASTEXITCODE -ne 0) { throw "The copied Tesseract does not run" }
$languages = & "$target\tesseract.exe" --list-langs
if ($languages -notcontains 'eng') { throw "The copied Tesseract has no English data" }
# What is being shipped, so the third-party notices can be checked against it.
Write-Host "Files in the bundled Tesseract:"
Get-ChildItem $target -Recurse -File | ForEach-Object { Write-Host ("  {0,12:N0}  {1}" -f $_.Length, $_.FullName.Substring((Resolve-Path $target).Path.Length + 1)) }
Write-Host "Bundled Tesseract:" (Get-ChildItem $target -Recurse | Measure-Object Length -Sum).Sum "bytes"
