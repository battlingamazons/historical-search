// The enlarged photograph viewer in a real browser.
//
// The jsdom tests of the viewer give it a stage of a made-up size and a
// picture that "loads" on cue; nothing is laid out and nothing is drawn.
// This serves the real src/ folder to headless Chrome and checks sizes and
// positions as the browser works them out.

import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { readFileSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join, extname } from 'node:path';
import puppeteer from 'puppeteer-core';

const here = dirname(fileURLToPath(import.meta.url));
const src = join(here, '..', '..', 'src');
const TYPES = { '.mjs': 'text/javascript', '.js': 'text/javascript', '.html': 'text/html', '.css': 'text/css' };

let server;
let browser;
let base;

before(async () => {
  server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://x');
    if (url.pathname === '/harness.html') {
      res.setHeader('content-type', 'text/html');
      res.end('<!doctype html><meta charset="utf-8"><body style="margin:0"><style>.lb{position:fixed;inset:0;display:flex;flex-direction:column}.lb-stage{flex:1;position:relative;overflow:hidden}.lb-img{position:absolute;left:0;top:0;transform-origin:0 0;max-width:none}.lb-msg[hidden],.lb-img[hidden]{display:none}</style><button id="opener">open</button>');
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

/// A page with the viewer open on a 3000x1500 picture, in a 1000x700 window.
async function open() {
  const page = await browser.newPage();
  const problems = [];
  page.on('pageerror', e => problems.push(e.message));
  await page.setViewport({ width: 1000, height: 700 });
  await page.goto(`${base}/harness.html`);
  await page.evaluate(async () => {
    const { openLightbox } = await import('/lightbox.mjs');
    const canvas = document.createElement('canvas');
    canvas.width = 3000; canvas.height = 1500;
    const g = canvas.getContext('2d');
    g.fillStyle = '#c9a35c'; g.fillRect(0, 0, 3000, 1500);
    g.fillStyle = '#000'; g.font = '60px sans-serif'; g.fillText('Hartwell Mill, 1887', 100, 200);
    const url = canvas.toDataURL('image/png');
    window.__closed = 0;
    window.__box = openLightbox({
      title: 'Hartwell Mill', count: 1, index: 0, doc: document,
      load: async () => ({ url, caption: 'The mill' }),
      openExternal: () => { window.__external = (window.__external || 0) + 1; },
      onClose: () => { window.__closed += 1; },
    });
  });
  await page.waitForFunction(() => !document.querySelector('.lb-img').hidden && document.querySelector('.lb-img').naturalWidth > 0);
  return { page, problems };
}

const rect = page => page.evaluate(() => {
  const r = document.querySelector('.lb-img').getBoundingClientRect();
  const s = document.querySelector('.lb-stage').getBoundingClientRect();
  return { x: r.x, y: r.y, w: r.width, h: r.height, stage: { x: s.x, y: s.y, w: s.width, h: s.height } };
});

test('it opens with the whole picture in view, centred', async () => {
  const { page, problems } = await open();
  const r = await rect(page);
  assert.ok(Math.abs(r.w - r.stage.w) < 1.5, `fits the width: ${r.w} in ${r.stage.w}`);
  assert.ok(r.h <= r.stage.h + 1, 'and is not taller than the stage');
  assert.ok(r.y >= r.stage.y - 1 && r.y + r.h <= r.stage.y + r.stage.h + 1, 'wholly inside it');
  assert.deepEqual(problems, []);
  await page.close();
});

test('100% shows the picture at its own size and the zoom label agrees', async () => {
  const { page } = await open();
  await page.click('[data-act="actual"]');
  const r = await rect(page);
  assert.ok(Math.abs(r.w - 3000) < 1.5 && Math.abs(r.h - 1500) < 1.5, `got ${r.w}x${r.h}`);
  assert.equal(await page.$eval('.lb-zoom', e => e.textContent), '100%');
  await page.close();
});

test('the wheel zooms in about the pointer, and dragging moves the picture', async () => {
  const { page } = await open();
  await page.click('[data-act="actual"]');
  const stage = (await rect(page)).stage;
  const at = { x: stage.x + 300, y: stage.y + 200 };
  // The picture point under the pointer before and after.
  const underPointer = async () => {
    const r = await rect(page);
    return { px: (at.x - r.x) / r.w, py: (at.y - r.y) / r.h, w: r.w };
  };
  const before = await underPointer();
  await page.mouse.move(at.x, at.y);
  await page.mouse.wheel({ deltaY: -300 });
  const after = await underPointer();
  assert.ok(after.w > before.w, 'larger');
  assert.ok(Math.abs(after.px - before.px) < 0.002 && Math.abs(after.py - before.py) < 0.002, 'the spot under the pointer stayed put');

  const r1 = await rect(page);
  await page.mouse.down();
  for (let i = 1; i <= 4; i++) {
    await page.mouse.move(at.x + 30 * i, at.y + 15 * i);
    await page.evaluate(() => new Promise(requestAnimationFrame));
  }
  await page.mouse.up();
  const r2 = await rect(page);
  assert.ok(Math.abs(r2.x - r1.x - 120) < 2 && Math.abs(r2.y - r1.y - 60) < 2, `moved by the drag: ${r2.x - r1.x},${r2.y - r1.y}`);
  await page.close();
});

test('the picture cannot be dragged out of the window', async () => {
  const { page } = await open();
  await page.click('[data-act="actual"]');
  const s = (await rect(page)).stage;
  await page.mouse.move(s.x + 500, s.y + 300);
  await page.mouse.down();
  await page.mouse.move(s.x + 5000, s.y + 5000);
  await page.mouse.up();
  const r = await rect(page);
  assert.ok(r.x <= s.x + 1 && r.y <= s.y + 1, 'its top left edge stops at the stage corner');
  await page.close();
});

test('double-click goes in and out, Escape closes, and the photo viewer button calls out', async () => {
  const { page } = await open();
  const s = (await rect(page)).stage;
  await page.mouse.click(s.x + 500, s.y + 200, { count: 2 });
  assert.ok((await rect(page)).w > s.w * 1.5, 'zoomed in');
  await page.mouse.click(s.x + 500, s.y + 200, { count: 2 });
  assert.ok(Math.abs((await rect(page)).w - s.w) < 1.5, 'and back out to fit');

  await page.click('[data-act="external"]');
  assert.equal(await page.evaluate(() => window.__external), 1);

  await page.keyboard.press('Escape');
  assert.equal(await page.$('.lb'), null);
  assert.equal(await page.evaluate(() => window.__closed), 1);
  await page.close();
});

test('resizing the window while fitted keeps the picture fitted', async () => {
  const { page } = await open();
  await page.setViewport({ width: 600, height: 500 });
  await new Promise(r => setTimeout(r, 100));
  const r = await rect(page);
  assert.ok(Math.abs(r.w - r.stage.w) < 1.5, `refitted: ${r.w} in ${r.stage.w}`);
  await page.close();
});
