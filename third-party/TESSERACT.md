Scanned pages are read with [Tesseract](https://github.com/tesseract-ocr/tesseract),
an open-source text recognition engine, licensed under the Apache License,
Version 2.0, with its English language data from the
[tessdata](https://github.com/tesseract-ocr/tessdata) project under the same
licence. Tesseract uses the
[Leptonica](https://github.com/DanBloomberg/leptonica) image library, licensed
as below.

Tesseract is run as a separate program; it is not linked into this
application. The Windows installer carries a copy of Tesseract, built from its
unmodified source by `scripts/bundle-tesseract.ps1` (with vcpkg), and the
English language data -- so that nothing has to be installed separately. On
macOS and Linux the copy already installed on the computer is used and is not
distributed with this application. The licences of the libraries built into
the Windows copy are listed under "Windows bundle" below.
