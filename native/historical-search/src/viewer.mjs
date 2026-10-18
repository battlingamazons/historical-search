// Shows one page of a PDF inside the item card.
//
// The file is never handed to the window whole. A scanned volume can be over a
// gigabyte, and what is wanted is one page of it, so pdf.js is given a way to
// ask for the pieces it needs, and the application reads just those bytes
// from the file (`read_document_range`, by the record's id -- the window never
// holds a path).
//
// Loaded only when a PDF is opened, so the many people who never open one do
// not pay for 1.7 MB of script.

let pdfjs = null;

async function loadPdfjs() {
  if (!pdfjs) {
    pdfjs = await import('./pdfjs/pdf.min.mjs');
    pdfjs.GlobalWorkerOptions.workerSrc = new URL('./pdfjs/pdf.worker.min.mjs', import.meta.url).href;
  }
  return pdfjs;
}

/// Whether a piece of text starts a word the person typed. The same rule the
/// search uses -- words match from their beginning -- so a highlight appears
/// where the search said something would be.
function startsAWord(text, terms) {
  const words = text.toLowerCase().split(/[^\p{L}\p{N}]+/u).filter(Boolean);
  return words.some(word => terms.some(term => word.startsWith(term)));
}

const MAX_CANVAS_PIXELS = 24 * 1024 * 1024;
const ZOOM_STEPS = [0.5, 0.75, 1, 1.25, 1.5, 2, 3, 4];

/// Puts a viewer in `host` and shows `startPage`. Returns { goTo, destroy }.
///
/// `read(start, end)` resolves to the bytes of the file between those
/// offsets; `size` is the file's length; `terms` are the lower-cased words to
/// highlight.
export async function mountViewer(host, { read, size, startPage, terms, onPage }) {
  const lib = await loadPdfjs();

  host.innerHTML = `
    <div class="pdf-bar">
      <button type="button" data-act="prev" title="Previous page">&larr;</button>
      <span class="pdf-where">Page <input type="number" min="1" value="1" aria-label="Page number"> of <span data-role="count">…</span></span>
      <button type="button" data-act="next" title="Next page">&rarr;</button>
      <span class="pdf-spacer"></span>
      <button type="button" data-act="out" title="Smaller">&minus;</button>
      <button type="button" data-act="fit" title="Fit to width">Fit</button>
      <button type="button" data-act="in" title="Larger">+</button>
    </div>
    <div class="pdf-stage"><div class="pdf-page"><canvas></canvas><div class="pdf-text"></div></div></div>
    <div class="pdf-note" hidden></div>`;
  const stage = host.querySelector('.pdf-stage');
  const pageBox = host.querySelector('.pdf-page');
  const canvas = host.querySelector('canvas');
  const textLayer = host.querySelector('.pdf-text');
  const note = host.querySelector('.pdf-note');
  const pageInput = host.querySelector('input');
  const countEl = host.querySelector('[data-role="count"]');

  const say = (message) => {
    note.hidden = !message;
    note.textContent = message || '';
  };

  // pdf.js asks for ranges; the application answers from the file.
  class FileRanges extends lib.PDFDataRangeTransport {
    requestDataRange(begin, end) {
      read(begin, Math.min(end, size))
        .then(bytes => this.onDataRange(begin, new Uint8Array(bytes)))
        .catch(err => say('Could not read this PDF: ' + err));
    }
  }
  const first = new Uint8Array(await read(0, Math.min(size, 65536)));
  const doc = await lib.getDocument({
    range: new FileRanges(size, first),
    length: size,
    disableAutoFetch: true,
    disableStream: true,
    // The decoders for fax-compressed (CCITT), JBIG2 and JPEG 2000 pictures
    // are WebAssembly files in pdfjs/wasm. Without this address a page made
    // of one of those draws as a blank sheet and logs a warning nobody sees.
    wasmUrl: new URL('./pdfjs/wasm/', import.meta.url).href,
    // The fonts and character maps pdf.js can load from its package are not
    // shipped; pages fall back to the computer's own fonts.
    useSystemFonts: true,
    isEvalSupported: false,
  }).promise;

  countEl.textContent = String(doc.numPages);
  pageInput.max = String(doc.numPages);

  let page = 1;
  let zoom = 1;
  let token = 0;
  let task = null;
  let destroyed = false;

  async function show(number) {
    number = Math.min(Math.max(1, Math.round(number) || 1), doc.numPages);
    const mine = ++token;
    page = number;
    pageInput.value = String(number);
    say('');
    if (task) { try { task.cancel(); } catch (e) { /* already finished */ } }
    try {
      const pdfPage = await doc.getPage(number);
      if (mine !== token || destroyed) return;
      const natural = pdfPage.getViewport({ scale: 1 });
      // Fit to the width of the stage, then the chosen zoom on top.
      const fit = Math.max(0.1, (stage.clientWidth - 24) / natural.width);
      let scale = fit * zoom;
      const dpr = window.devicePixelRatio || 1;
      // A very large page at high zoom must not ask for a canvas the browser
      // will refuse -- which would show as a blank page with no explanation.
      const pixels = natural.width * natural.height * scale * scale * dpr * dpr;
      if (pixels > MAX_CANVAS_PIXELS) scale *= Math.sqrt(MAX_CANVAS_PIXELS / pixels);
      const viewport = pdfPage.getViewport({ scale });

      canvas.width = Math.floor(viewport.width * dpr);
      canvas.height = Math.floor(viewport.height * dpr);
      canvas.style.width = `${Math.floor(viewport.width)}px`;
      canvas.style.height = `${Math.floor(viewport.height)}px`;
      pageBox.style.width = canvas.style.width;
      pageBox.style.height = canvas.style.height;
      textLayer.replaceChildren();
      textLayer.style.setProperty('--total-scale-factor', String(viewport.scale));
      textLayer.style.setProperty('--scale-factor', String(viewport.scale));

      task = pdfPage.render({
        canvasContext: canvas.getContext('2d'),
        viewport,
        transform: dpr === 1 ? null : [dpr, 0, 0, dpr, 0, 0],
      });
      await task.promise;
      if (mine !== token || destroyed) return;

      // The words on the page, laid over the picture so they can be found
      // and selected -- and, here, marked where they match. A scan has none
      // of its own, so there is nothing to mark on it.
      const layer = new lib.TextLayer({
        textContentSource: pdfPage.streamTextContent(),
        container: textLayer,
        viewport,
      });
      await layer.render();
      if (mine !== token || destroyed) return;
      let marked = 0;
      if (terms.length) {
        for (const span of textLayer.querySelectorAll('span')) {
          if (span.textContent && startsAWord(span.textContent, terms)) {
            span.classList.add('pdf-hit');
            marked += 1;
          }
        }
      }
      if (terms.length && marked === 0 && textLayer.querySelector('span') === null) {
        say('This page is a picture of a printed page, so the words that matched cannot be marked on it.');
      }
      if (onPage) onPage(number);
    } catch (err) {
      if (err && err.name === 'RenderingCancelledException') return;
      if (mine === token && !destroyed) say('Could not show this page: ' + (err && err.message ? err.message : err));
    }
  }

  const stepZoom = (direction) => {
    const at = ZOOM_STEPS.findIndex(z => z >= zoom - 1e-6);
    const next = ZOOM_STEPS[Math.min(ZOOM_STEPS.length - 1, Math.max(0, (at < 0 ? ZOOM_STEPS.length - 1 : at) + direction))];
    zoom = next;
    show(page);
  };
  host.addEventListener('click', (event) => {
    const act = event.target.closest('[data-act]')?.dataset.act;
    if (act === 'prev') show(page - 1);
    else if (act === 'next') show(page + 1);
    else if (act === 'in') stepZoom(1);
    else if (act === 'out') stepZoom(-1);
    else if (act === 'fit') { zoom = 1; show(page); }
  });
  pageInput.addEventListener('change', () => show(Number(pageInput.value)));

  await show(startPage || 1);
  return {
    goTo: (number) => show(number),
    destroy: () => {
      destroyed = true;
      try { task && task.cancel(); } catch (e) { /* ignore */ }
      doc.destroy();
    },
  };
}
