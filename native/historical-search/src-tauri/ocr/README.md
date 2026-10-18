# Text recognition (OCR) files

The Windows build ships its own copy of Tesseract here, so the society's
computer needs to install nothing. CI fills this folder with `tesseract.exe`,
its libraries and the English language data (see `scripts/bundle-tesseract.ps1`)
before building; only this note is kept in the repository.

Other platforms use the Tesseract installed on the computer: `brew install
tesseract` on macOS, `apt install tesseract-ocr` on Linux (the `.deb` pulls it
in by itself).
