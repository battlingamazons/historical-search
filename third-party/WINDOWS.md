The Windows installer carries Tesseract's own libraries, listed below, as
files in the installation folder next to the program. The list is what
Tesseract reports being built with plus the standard MinGW-w64 runtime; the
licence texts below are the projects' own.

**On libidn2 (LGPL-3.0-or-later):** it ships as a separate DLL in the
installation folder, not built into any other file, so it can be replaced with
another build of the same library. Its source is available from the GNU Libidn
project (<https://gitlab.com/libidn/libidn2>). The GNU General Public License
version 3 is reproduced because the LGPL is defined as an addition to it.

**On the GCC runtime:** libgcc and libstdc++ are distributed under the GPL
version 3 *with the GCC Runtime Library Exception* (reproduced below), which
allows them to accompany a program under a different licence. Their source is
part of GCC (<https://gcc.gnu.org/>).
