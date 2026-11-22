// Picture search: finding pictures by what they show. Words typed in the search
// box, or a scan that was dropped, also bring up pictures that fit, in a
// section of their own beneath the ordinary results.
//
// The section must never get in the way of the ordinary search -- it appears
// only when the feature is on, its files are downloaded and some pictures are
// described, and an answer for words that have since been replaced must never
// be shown under the new ones.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, click, type } from './harness.mjs';

const READY = { enabled: true, installed: true, downloading: false, download_done: 0,
                download_total: 300000000, download_error: null, described: 16000, pending: 0 };

const records = ids => ids.map(id => ({
    id, catalog: 'Photo', accessno: `2010.1.${id}`, title: `Picture record ${id}`, snippet: '',
    has_image: true, is_document: false, document_kind: '', is_exact_id: false,
}));
const answerWith = (ids, loose = [], has_more = false) => ({
    hits: [...ids.map(id => ({ kind: 'record', id, index: 0 })), ...loose.map((_, i) => ({ kind: 'loose', id: 0, index: i }))],
    response: { results: records(ids), has_more: false, total: ids.length, terms: [], words: [], datasets: [] },
    loose, has_more,
});
const looseOne = (index, name) => ({ index, name, folder: 'Scans / Barns', kind: 'closest', percent: 0, cropped: false });

async function ready(over = {}) {
    const backend = makeBackend({ total: 3, ...over });
    backend.pictureStatus = { ...READY };
    backend.pictureAnswer = () => answerWith([50, 51]);
    const dom = await loadPage(backend);
    return { backend, window: dom.window };
}
const section = w => w.document.getElementById('picture-section');
const asked = b => b.calls.filter(c => c.cmd === 'picture_search');

test('with picture search off, nothing is asked and nothing is shown', async () => {
    const backend = makeBackend({ total: 3 }); // status: not enabled
    const { window } = await loadPage(backend);
    type(window, 'horse and wagon');
    await settle(window);
    assert.equal(asked(backend).length, 0);
    assert.equal(section(window).hidden, true);
});

test('typed words bring up pictures that fit, under the ordinary results', async () => {
    const { backend, window } = await ready();
    type(window, 'horse and wagon');
    await settle(window, 400);
    assert.equal(asked(backend).length, 1);
    assert.equal(asked(backend)[0].args.phrase, 'horse and wagon');
    assert.equal(asked(backend)[0].args.limit, 24);
    assert.equal(section(window).hidden, false);
    assert.match(section(window).querySelector('h2').textContent, /Pictures that look like this/);
    assert.deepEqual([...section(window).querySelectorAll('.result')].map(e => e.dataset.id), ['50', '51']);
    // Beneath the ordinary results, not among them.
    assert.equal(window.document.getElementById('results').querySelector('[data-id="50"]'), null);
});

test('a record the words already found is not listed a second time', async () => {
    const { backend, window } = await ready();
    backend.pictureAnswer = () => answerWith([1, 2, 77]); // 1 and 2 are the ordinary results too
    type(window, 'barn');
    await settle(window, 400);
    assert.deepEqual([...section(window).querySelectorAll('.result')].map(e => e.dataset.id), ['77']);
});

test('a picture no record uses is listed too, with where it is', async () => {
    const { backend, window } = await ready();
    backend.pictureAnswer = () => answerWith([50], [looseOne(0, 'barn raising.tif')]);
    type(window, 'barn raising');
    await settle(window, 400);
    const cards = [...section(window).querySelectorAll('.result')];
    assert.equal(cards.length, 2);
    assert.match(cards[1].textContent, /barn raising\.tif/);
    assert.match(cards[1].textContent, /No record/);
    assert.match(cards[1].textContent, /Scans \/ Barns/);
});

test('"Show more records" asks for twice as many', async () => {
    const { backend, window } = await ready();
    backend.pictureAnswer = () => answerWith([50, 51], [], true);
    type(window, 'church');
    await settle(window, 400);
    click(window, '#picture-more-btn');
    await settle(window, 100);
    assert.deepEqual(asked(backend).map(c => c.args.limit), [24, 48]);
});

test('very short words are not looked up, and emptying the box clears the section', async () => {
    const { backend, window } = await ready();
    type(window, 'ba');
    await settle(window, 400);
    assert.equal(asked(backend).length, 0);
    type(window, 'barn');
    await settle(window, 400);
    assert.equal(section(window).hidden, false);
    click(window, '#clear-btn');
    await settle(window, 100);
    assert.equal(section(window).hidden, true);
    assert.equal(section(window).innerHTML, '');
});

test('an answer for words that have been replaced is never shown', async () => {
    const { backend, window } = await ready();
    backend.holdPictures = true;
    backend.pictureAnswer = args => answerWith(args.phrase === 'barn' ? [60] : [70]);
    type(window, 'barn');
    await settle(window, 400);
    type(window, 'bridge');
    await settle(window, 400);
    backend.release('lifo'); // the newer answer arrives first, then the old one
    await settle(window, 100);
    assert.deepEqual([...section(window).querySelectorAll('.result')].map(e => e.dataset.id), ['70']);
});

test('a failed picture search says so in its own section and leaves the results alone', async () => {
    const { backend, window } = await ready();
    backend.pictureError = 'Picture search is not set up yet.';
    type(window, 'barn');
    await settle(window, 400);
    assert.match(section(window).textContent, /not set up yet/);
    assert.ok(window.document.querySelectorAll('#results .result').length > 0, 'the ordinary results are still there');
});

test('a dropped scan lists pictures that look like it, with no button to ask', async () => {
    const { backend, window } = await ready();
    backend.pictureAnswer = () => answerWith([80, 81]);
    backend.listeners['tauri://drag-drop']({ payload: { paths: ['/scans/new.tif'] } });
    await settle(window, 200);
    await settle(window, 100);
    assert.equal(window.document.getElementById('scan-pictures-btn'), null);
    assert.deepEqual(asked(backend).map(c => [c.args.path, c.args.phrase]), [['/scans/new.tif', null]]);
    assert.match(section(window).querySelector('h2').textContent, /look like your scan/);
    assert.deepEqual([...section(window).querySelectorAll('.result')].map(e => e.dataset.id), ['80', '81']);
    click(window, '.scan-close');
    assert.equal(section(window).hidden, true, 'closing the scan strip takes its pictures away');
});

test('a dropped scan lists no pictures when picture search is not ready', async () => {
    const backend = makeBackend({ total: 3 });
    const { window } = await loadPage(backend);
    backend.listeners['tauri://drag-drop']({ payload: { paths: ['/scans/new.tif'] } });
    await settle(window, 200);
    assert.equal(section(window).hidden, true);
});

// ---- the settings ----

async function settings(status, config = {}) {
    const backend = makeBackend({ total: 3 });
    backend.pictureStatus = { ...backend.pictureStatus, ...status };
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        if (cmd === 'get_config') return { data_dir: '', images_dir: '', multimedia_dir: '', photo_archive_roots: [],
            document_roots: [], spreadsheet_roots: [], recognize_scanned_pdfs: true, picture_search: false, ...config };
        if (cmd === 'save_config') { backend.saved = args.config; return null; }
        return original(cmd, args);
    };
    const dom = await loadPage(backend);
    click(dom.window, '#settings-btn');
    await settle(dom.window, 200);
    return { backend, window: dom.window };
}
const box = w => w.document.getElementById('picture-search-box');

test('settings: off by default, and nothing about downloads is shown until it is ticked', async () => {
    const { window } = await settings({});
    assert.equal(window.document.getElementById('picture-search-on').checked, false);
    assert.equal(box(window).hidden, true);
});

test('settings: ticking it explains the download and offers the button', async () => {
    const { backend, window } = await settings({});
    const on = window.document.getElementById('picture-search-on');
    on.checked = true;
    on.dispatchEvent(new window.Event('change'));
    await settle(window, 150);
    assert.equal(box(window).hidden, false);
    assert.match(box(window).textContent, /one-time download of about 300 MB/);
    assert.match(box(window).textContent, /only time this program uses the internet/);
    click(window, '#picture-download-btn');
    await settle(window, 100);
    assert.equal(backend.calls.filter(c => c.cmd === 'picture_search_download').length, 1);
});

test('settings: a download in progress shows how far along it is, and can be stopped', async () => {
    const { backend, window } = await settings({ downloading: true, download_done: 120000000, download_total: 300000000 }, { picture_search: true });
    assert.match(box(window).textContent, /120 of 300 MB/);
    assert.equal(box(window).querySelector('progress').value, 120000000);
    click(window, '#picture-stop-btn');
    assert.equal(backend.calls.filter(c => c.cmd === 'picture_search_stop').length, 1);
    window.close(); // the page keeps asking how the download is going
});

test('settings: a download that failed says why and offers to try again', async () => {
    const { window } = await settings({ download_error: 'could not download the picture-search files (timed out).' }, { picture_search: true });
    assert.match(box(window).textContent, /did not finish: could not download/);
    assert.match(box(window).querySelector('#picture-download-btn').textContent, /Try again/);
});

test('settings: once downloaded it says how many pictures are described', async () => {
    const { window } = await settings({ installed: true, enabled: true, described: 12000, pending: 4000 }, { picture_search: true });
    assert.match(box(window).textContent, /files are downloaded/);
    assert.match(box(window).textContent, /12,000 pictures are described so far; 4,000 more/);
    assert.equal(box(window).querySelector('#picture-download-btn'), null);
});

test('settings: saving carries the choice', async () => {
    const { backend, window } = await settings({ installed: true });
    const on = window.document.getElementById('picture-search-on');
    on.checked = true;
    on.dispatchEvent(new window.Event('change'));
    click(window, '#settings-save');
    await settle(window, 150);
    assert.equal(backend.saved.picture_search, true);
});

test('a picture record opened from the pictures can be stepped through like any result', async () => {
    const { backend, window } = await ready();
    backend.pictureAnswer = () => answerWith([1, 2, 3]);
    backend.listeners['tauri://drag-drop']({ payload: { paths: ['/scans/new.tif'] } });
    await settle(window, 300);
    click(window, '#picture-list .result[data-id="2"]');
    await settle(window, 100);
    const pos = () => window.document.querySelector('.nav-position').textContent;
    assert.equal(pos(), '2 of 3');
    click(window, '#next-item-btn');
    await settle(window, 100);
    assert.equal(pos(), '3 of 3');
    assert.equal(window.document.getElementById('next-item-btn').disabled, true);
    click(window, '#prev-item-btn');
    await settle(window, 100);
    click(window, '#prev-item-btn');
    await settle(window, 100);
    assert.equal(pos(), '1 of 3');
});

test('checking only an outlined part of a scan searches pictures for that part', async () => {
    const { backend, window } = await ready();
    backend.pictureAnswer = () => answerWith([80]);
    backend.listeners['tauri://drag-drop']({ payload: { paths: ['/scans/new.tif'] } });
    await settle(window, 300);
    assert.equal(asked(backend).at(-1).args.part, null);
    click(window, '#scan-crop-btn');
    const stage = window.document.querySelector('#scan-cropper');
    const fire = (type, x, y) => stage.dispatchEvent(new window.MouseEvent(type, { clientX: x, clientY: y, bubbles: true }));
    fire('pointerdown', 0.6, 0.5);
    fire('pointermove', 0.2, 0.1);
    fire('pointerup', 0.2, 0.1);
    click(window, '#scan-crop-go');
    await settle(window, 300);
    const part = asked(backend).at(-1).args.part;
    assert.deepEqual(Array.from(part, v => Math.round(v * 10) / 10), [0.2, 0.1, 0.6, 0.5]);
});
