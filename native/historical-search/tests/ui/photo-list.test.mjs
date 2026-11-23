// The photo list: what lets a dropped image be checked against the collection.
// It has no controls of its own. The one refresh brings it up to date along
// with the search index, the line about the index says how many photos it
// holds, and a dropped image finding it out of date brings it up to date first.
// What is checked here is what a person sees: that there is nothing extra to
// press, that the refresh covers it, and that nothing in its notes can be
// taken for markup.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, click } from './harness.mjs';

const text = (window, selector) => window.document.querySelector(selector).textContent;
const drop = (backend, paths) => backend.listeners['tauri://drag-drop']({ payload: { paths } });
const calls = (backend, cmd) => backend.calls.filter(c => c.cmd === cmd).length;

test('there is no button for the photo list, and the drop box sits right under the search box', async () => {
    const backend = makeBackend({ total: 3 });
    const { window } = await loadPage(backend);
    const doc = window.document;
    assert.equal(doc.getElementById('photo-list-btn'), null);
    assert.equal(doc.getElementById('photo-list-row'), null);
    assert.equal(doc.getElementById('photo-list-status'), null);
    assert.equal(doc.getElementById('photo-list-notes'), null);
    assert.doesNotMatch(doc.body.textContent, /Update photo list/);
    const box = doc.getElementById('scan-drop');
    assert.match(box.textContent, /Search by picture/);
    assert.match(box.textContent, /drop an image here/i);
    // Above the line about the index, not below it.
    assert.ok(box.compareDocumentPosition(doc.getElementById('last-updated')) & window.Node.DOCUMENT_POSITION_FOLLOWING);
    assert.ok(doc.querySelector('.search-box').compareDocumentPosition(box) & window.Node.DOCUMENT_POSITION_FOLLOWING);
});

test('the refresh also updates the photo list, and the one line says how many photos it holds', async () => {
    const backend = makeBackend({ total: 3, photoListFiles: 12345 });
    const { window } = await loadPage(backend);
    await settle(window, 50);
    assert.doesNotMatch(text(window, '#last-updated'), /photos/, 'nothing about photos before there is a list');
    click(window, '#refresh-btn');
    await settle(window, 200);
    assert.equal(calls(backend, 'start_refresh'), 1);
    assert.equal(calls(backend, 'photo_check_update'), 1, 'the same click brought the photo list up to date');
    assert.match(text(window, '#last-updated'), /Index updated .* · 3 records · 12,345 photos/);
    assert.equal(window.document.getElementById('refresh-btn').disabled, false, 'ready to be used again');
    assert.equal(window.document.getElementById('refresh-btn').textContent, 'Refresh Search Index');
});

test('a list made earlier is counted in the line when the window opens', async () => {
    const backend = makeBackend({ total: 3 });
    backend.photoList = { ...backend.photoList, files: 22898, last_scanned: 1700000100 };
    const { window } = await loadPage(backend);
    await settle(window, 50);
    assert.match(text(window, '#last-updated'), /3 records · 22,898 photos/);
    assert.equal(calls(backend, 'photo_check_update'), 0, 'opening the window does not start any work');
});

test('while the pictures are being learned, the refresh says how far along it is, and stays busy', async () => {
    const backend = makeBackend({ total: 3 });
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        if (cmd === 'photo_check_update') {
            backend.photoList = { ...backend.photoList, in_progress: true, phase: 'Learning what the pictures look like',
                                  phase_current: 4200, phase_total: 22898 };
            return true;
        }
        return original(cmd, args);
    };
    const { window } = await loadPage(backend);
    click(window, '#refresh-btn');
    await settle(window, 100);
    assert.match(text(window, '#last-updated'), /Learning what the pictures look like… 4,200 of 22,898/);
    assert.equal(window.document.getElementById('refresh-btn').disabled, true);
    // Let it finish, so the window stops asking and the test can end.
    backend.photoList = { ...backend.photoList, in_progress: false, phase: null, files: 22898, last_scanned: 1700000100 };
    await settle(window, 900);
    assert.match(text(window, '#last-updated'), /22,898 photos/);
    assert.equal(window.document.getElementById('refresh-btn').disabled, false);
});

test('while the folders are being read there is a running count', async () => {
    const backend = makeBackend({ total: 3 });
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        if (cmd === 'photo_check_update') {
            backend.photoList = { ...backend.photoList, in_progress: true, looked_at: 4200 };
            return true;
        }
        return original(cmd, args);
    };
    const { window } = await loadPage(backend);
    click(window, '#refresh-btn');
    await settle(window, 100);
    assert.match(text(window, '#last-updated'), /Reading the photo folders… 4,200 photos so far/);
    backend.photoList = { ...backend.photoList, in_progress: false, files: 9, last_scanned: 1700000100 };
    await settle(window, 900);
    assert.match(text(window, '#last-updated'), /9 photos/);
});

test('a folder that could not be read is explained among the warnings, in words and not as markup', async () => {
    const backend = makeBackend({ total: 3 });
    backend.photoList = {
        ...backend.photoList, files: 5, last_scanned: 1700000100,
        notes: ['The folder "<b>Folder A</b>" could not be opened, so what the list already had for it was kept.'],
    };
    const { window } = await loadPage(backend);
    await settle(window, 50);
    const warnings = window.document.getElementById('warnings');
    assert.equal(warnings.hidden, false);
    assert.match(warnings.textContent, /<b>Folder A<\/b>/, 'shown as typed');
    assert.equal(warnings.querySelector('b'), null, 'and not turned into an element');
});

test('with nothing to report, the warnings stay out of the way', async () => {
    const backend = makeBackend({ total: 3 });
    const { window } = await loadPage(backend);
    await settle(window, 50);
    assert.equal(window.document.getElementById('warnings').hidden, true);
});

test('a failed update says why instead of looking finished', async () => {
    const backend = makeBackend({ total: 3 });
    backend.photoList = { ...backend.photoList, last_error: 'Something went wrong.' };
    const { window } = await loadPage(backend);
    await settle(window, 50);
    assert.match(text(window, '#warnings'), /photo list could not be updated.*Something went wrong/);
});

test('Settings can start the list over, and the line stops counting its photos', async () => {
    const backend = makeBackend({ total: 3, photoListFiles: 77 });
    const { window } = await loadPage(backend);
    click(window, '#refresh-btn');
    await settle(window, 200);
    assert.match(text(window, '#last-updated'), /77 photos/);

    click(window, '#settings-btn');
    await settle(window, 200);
    click(window, '#photo-list-reset-btn');
    await settle(window, 200);
    assert.ok(backend.calls.some(c => c.cmd === 'photo_check_clear'));
    assert.match(text(window, '#photo-list-reset-msg'), /cleared.*next refresh/i);
    assert.doesNotMatch(text(window, '#last-updated'), /photos/);
});

test('clearing while an update is running is refused in words', async () => {
    const backend = makeBackend({ total: 3 });
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        if (cmd === 'photo_check_clear') throw 'The photo list is being updated; try again when it has finished.';
        return original(cmd, args);
    };
    const { window } = await loadPage(backend);
    click(window, '#settings-btn');
    await settle(window, 200);
    click(window, '#photo-list-reset-btn');
    await settle(window, 100);
    assert.match(text(window, '#photo-list-reset-msg'), /being updated/);
});

test('a dropped image finding no list makes it first, then checks', async () => {
    const backend = makeBackend({ total: 3, photoListFiles: 50 });
    const { window } = await loadPage(backend);
    await settle(window, 50);
    drop(backend, ['/scans/new.tif']);
    await settle(window, 300);
    const order = backend.calls.map(c => c.cmd).filter(c => c === 'photo_check_update' || c === 'photo_check_scan');
    assert.deepEqual(order, ['photo_check_update', 'photo_check_scan'], 'the list was made before the check');
    assert.match(text(window, '#last-updated'), /50 photos/, 'and the line about the index now counts it');
});

test('a dropped image finding the list complete does not redo it', async () => {
    const backend = makeBackend({ total: 3 });
    backend.photoList = { ...backend.photoList, files: 50, last_scanned: 1700000100, not_yet_learned: 0 };
    const { window } = await loadPage(backend);
    await settle(window, 50);
    drop(backend, ['/scans/new.tif']);
    await settle(window, 300);
    assert.equal(calls(backend, 'photo_check_update'), 0);
    assert.equal(calls(backend, 'photo_check_scan'), 1);
});

test('a dropped image finding pictures not yet learned brings the list up to date first', async () => {
    const backend = makeBackend({ total: 3, photoListFiles: 50 });
    backend.photoList = { ...backend.photoList, files: 40, last_scanned: 1700000100, not_yet_learned: 10 };
    const { window } = await loadPage(backend);
    await settle(window, 50);
    drop(backend, ['/scans/new.tif']);
    await settle(window, 300);
    assert.equal(calls(backend, 'photo_check_update'), 1);
    assert.equal(calls(backend, 'photo_check_scan'), 1);
});

test('while the list is made for a dropped image, the strip says the collection is being got ready', async () => {
    const backend = makeBackend({ total: 3 });
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        if (cmd === 'photo_check_update') {
            backend.photoList = { ...backend.photoList, in_progress: true, phase: 'Learning what the pictures look like',
                                  phase_current: 100, phase_total: 22898 };
            return true;
        }
        return original(cmd, args);
    };
    const { window } = await loadPage(backend);
    await settle(window, 50);
    drop(backend, ['/scans/new.tif']);
    await settle(window, 100);
    assert.match(text(window, '#scan-result'), /Getting the collection ready: Learning what the pictures look like… 100 of 22,898/);
    assert.equal(calls(backend, 'photo_check_scan'), 0, 'not checked until it is ready');
    backend.photoList = { ...backend.photoList, in_progress: false, phase: null, files: 22898, last_scanned: 1700000100 };
    await settle(window, 1000);
    assert.equal(calls(backend, 'photo_check_scan'), 1);
});
