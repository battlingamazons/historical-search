// The PDF viewer in a real browser.
//
// The window's own logic is tested against a stand-in browser (jsdom) that
// cannot draw, so none of the drawing is exercised there -- and drawing is
// where the viewer went wrong: a fax-compressed scan showed as a blank white
// page, with nothing in any log, because pdf.js decodes those with WebAssembly
// files it must be told where to find. A blank page is exactly what a test
// that only checks "no error" would pass.
//
// This serves the real src/ folder and the viewer's real code to headless
// Chrome, hands it the files a piece at a time exactly as the application
// does, and looks at the pixels.

import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { readFileSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join, extname } from 'node:path';
import puppeteer from 'puppeteer-core';

const here = dirname(fileURLToPath(import.meta.url));
const src = join(here, '..', '..', 'src');
const support = join(here, '..', '..', 'src-tauri', 'tests', 'support');

/// A small text PDF, one line per page, with a correct cross-reference table.
function textPdf(lines) {
  const objects = [];
  const kids = lines.map((_, i) => `${4 + i * 2} 0 R`).join(' ');
  objects.push('<< /Type /Catalog /Pages 2 0 R >>');
  objects.push(`<< /Type /Pages /Kids [${kids}] /Count ${lines.length} >>`);
  objects.push('<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>');
  lines.forEach((line, i) => {
    objects.push(`<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents ${5 + i * 2} 0 R >>`);
    const stream = `BT /F1 24 Tf 72 700 Td (${line}) Tj ET`;
    objects.push(`<< /Length ${stream.length} >>\nstream\n${stream}\nendstream`);
  });
  let out = '%PDF-1.4\n';
  const offsets = [];
  objects.forEach((body, i) => { offsets.push(out.length); out += `${i + 1} 0 obj\n${body}\nendobj\n`; });
  const xref = out.length;
  out += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
  for (const o of offsets) out += `${String(o).padStart(10, '0')} 00000 n \n`;
  out += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return Buffer.from(out, 'latin1');
}

const FILES = {
  'text.pdf': textPdf(['Minutes of the annual meeting', 'The Hartwell family donated the loom', 'Report on the schoolhouse bell tower']),
  'jpeg-scan.pdf': readFileSync(join(support, 'scan_jpeg.pdf')),
  'fax-scan.pdf': readFileSync(join(support, 'scan_g4.pdf')),
};

const TYPES = { '.mjs': 'text/javascript', '.js': 'text/javascript', '.html': 'text/html', '.wasm': 'application/wasm', '.css': 'text/css' };

let server;
let browser;
let base;

before(async () => {
  server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://x');
    if (url.pathname === '/harness.html') {
      res.setHeader('content-type', 'text/html');
      res.end('<!doctype html><meta charset="utf-8"><div id="host"></div>');
      return;
    }
    // A piece of a fixture, the way the application hands a file over.
    if (url.pathname.startsWith('/fx/')) {
      const body = FILES[url.pathname.slice(4)];
      if (!body) { res.statusCode = 404; res.end(); return; }
      const [from, to] = [Number(url.searchParams.get('start') ?? 0), Number(url.searchParams.get('end') ?? body.length)];
      res.setHeader('content-type', 'application/octet-stream');
      res.setHeader('x-size', String(body.length));
      res.end(body.subarray(from, Math.min(to, body.length)));
      return;
    }
    const file = join(src, url.pathname.replace(/^\/+/, ''));
    if (!file.startsWith(src) || !existsSync(file)) { res.statusCode = 404; res.end(); return; }
    res.setHeader('content-type', TYPES[extname(file)] || 'application/octet-stream');
    res.end(readFileSync(file));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  base = `http://127.0.0.1:${server.address().port}`;

  const candidates = [
    process.env.CHROME_PATH,
    '/usr/bin/google-chrome', '/usr/bin/google-chrome-stable', '/usr/bin/chromium', '/usr/bin/chromium-browser',
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
  ].filter(Boolean);
  const executablePath = candidates.find(p => existsSync(p));
  assert.ok(executablePath, 'these tests need Chrome or Chromium; set CHROME_PATH to its location');
  browser = await puppeteer.launch({ executablePath, headless: true, args: ['--no-sandbox'] });
});

after(async () => {
  await browser?.close();
  server?.close();
});

/// Mounts the viewer on one fixture in a fresh page and returns what it shows.
async function show(name, { startPage = 1, terms = [] } = {}) {
  const page = await browser.newPage();
  const problems = [];
  page.on('pageerror', e => problems.push(e.message));
  try {
    await page.goto(`${base}/harness.html`);
    await page.setViewport({ width: 1000, height: 900 });
    const result = await page.evaluate(async (name, startPage, terms) => {
      const { mountViewer } = await import('/viewer.mjs');
      const read = async (start, end) => (await fetch(`/fx/${name}?start=${start}&end=${end}`)).arrayBuffer();
      const size = Number((await fetch(`/fx/${name}?start=0&end=1`)).headers.get('x-size'));
      const host = document.getElementById('host');
      const viewer = await mountViewer(host, { read, size, startPage, terms });
      const dark = () => {
        const c = host.querySelector('canvas');
        const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
        let n = 0;
        for (let i = 0; i < d.length; i += 4) if (d[i] < 128) n += 1;
        return n;
      };
      const state = () => ({
        dark: dark(),
        hits: host.querySelectorAll('.pdf-hit').length,
        hitText: [...host.querySelectorAll('.pdf-hit')].map(s => s.textContent),
        where: host.querySelector('input').value,
        count: host.querySelector('[data-role="count"]').textContent,
        note: host.querySelector('.pdf-note').hidden ? '' : host.querySelector('.pdf-note').textContent,
      });
      window.__viewer = viewer;
      window.__state = state;
      return state();
    }, name, startPage, terms);
    return { page, result, problems };
  } catch (e) {
    await page.close();
    throw e;
  }
}

test('a page of a text PDF is drawn, and the words that matched are marked', async () => {
  const { page, result, problems } = await show('text.pdf', { startPage: 2, terms: ['hartwell'] });
  try {
    assert.equal(result.where, '2');
    assert.equal(result.count, '3');
    assert.ok(result.dark > 500, `the page should have ink on it, got ${result.dark} dark pixels`);
    assert.ok(result.hits >= 1 && result.hitText.some(t => /hartwell/i.test(t)), `got ${JSON.stringify(result.hitText)}`);
    assert.deepEqual(problems, []);
  } finally { await page.close(); }
});

test('stepping to another page draws it, and its marks follow', async () => {
  const { page } = await show('text.pdf', { startPage: 1, terms: ['schoolhouse'] });
  try {
    const first = await page.evaluate(() => window.__state());
    assert.equal(first.hits, 0, 'nothing on page 1 matches');
    await page.evaluate(() => window.__viewer.goTo(3));
    await page.waitForFunction(() => window.__state().where === '3' && window.__state().hits > 0, { timeout: 15000 });
    const third = await page.evaluate(() => window.__state());
    assert.ok(third.dark > 500);
    assert.ok(third.hitText.some(t => /schoolhouse/i.test(t)));
  } finally { await page.close(); }
});

test('a scan made of a JPEG is drawn, and says its words cannot be marked', async () => {
  const { page, result } = await show('jpeg-scan.pdf', { terms: ['hartwell'] });
  try {
    assert.ok(result.dark > 500, `the scanned page should show, got ${result.dark} dark pixels`);
    assert.equal(result.hits, 0);
    assert.match(result.note, /picture of a printed page/);
  } finally { await page.close(); }
});

test('a fax-compressed scan is drawn rather than left a blank sheet', async () => {
  // The regression this exists for.
  const { page, result } = await show('fax-scan.pdf');
  try {
    assert.ok(result.dark > 500, `a blank page is the failure; got ${result.dark} dark pixels`);
  } finally { await page.close(); }
});

test('asking for a page that does not exist shows the nearest one', async () => {
  const { page } = await show('text.pdf', { startPage: 99 });
  try {
    const state = await page.evaluate(() => window.__state());
    assert.equal(state.where, '3');
  } finally { await page.close(); }
});
