// The photo list: the first part of checking whether a scan is already in the
// collection. The window shows how big the list is and how old, can bring it
// up to date, and can throw it away. What is checked here is what a person
// sees, and that nothing in the list's notes can be taken for markup.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, click } from './harness.mjs';

const text = (window, selector) => window.document.querySelector(selector).textContent;

test('before the list exists, the window says so and offers to make it', async () => {
    const backend = makeBackend({ total: 3, photoListFiles: 12345 });
    const { window } = await loadPage(backend);
    await settle(window, 50);
    assert.match(text(window, '#photo-list-status'), /not made yet/i);
    const button = window.document.getElementById('photo-list-btn');
    assert.equal(button.textContent, 'Update photo list');
    assert.equal(button.disabled, false);
});

test('updating the list shows how many photos it now holds', async () => {
    const backend = makeBackend({ total: 3, photoListFiles: 12345 });
    const { window } = await loadPage(backend);
    click(window, '#photo-list-btn');
    await settle(window, 100);
    assert.ok(backend.calls.some(c => c.cmd === 'photo_check_update'), 'the update was asked for');
    assert.match(text(window, '#photo-list-status'), /12,345 photos/, 'with a thousands separator');
    assert.match(text(window, '#photo-list-status'), /updated/i);
    assert.equal(window.document.getElementById('photo-list-btn').disabled, false, 'ready to be used again');
});

test('while the list is being read there is a running count, and nothing can be started twice', async () => {
    const backend = makeBackend({ total: 3 });
    backend.photoList = { ...backend.photoList, in_progress: true, looked_at: 4200 };
    const { window } = await loadPage(backend);
    await settle(window, 50);
    assert.match(text(window, '#photo-list-status'), /4,200 photos so far/);
    assert.equal(window.document.getElementById('photo-list-btn').disabled, true);
    // And when the update ends the window notices on its own.
    backend.photoList = { ...backend.photoList, in_progress: false, files: 9, last_scanned: 1700000100 };
    await settle(window, 900);
    assert.match(text(window, '#photo-list-status'), /9 photos/);
    assert.equal(window.document.getElementById('photo-list-btn').disabled, false);
});

test('while the pictures are being learned there is a count of how far along it is', async () => {
    const backend = makeBackend({ total: 3 });
    backend.photoList = { ...backend.photoList, in_progress: true, phase: 'Learning what the pictures look like',
                          phase_current: 4200, phase_total: 22898 };
    const { window } = await loadPage(backend);
    await settle(window, 50);
    assert.match(text(window, '#photo-list-status'), /Learning what the pictures look like… 4,200 of 22,898/);
    assert.equal(window.document.getElementById('photo-list-btn').disabled, true);
    // Let it finish, so the window stops asking and the test can end.
    backend.photoList = { ...backend.photoList, in_progress: false, phase: null, files: 22898, last_scanned: 1700000100 };
    await settle(window, 900);
    assert.equal(window.document.getElementById('photo-list-btn').disabled, false);
});

test('pictures not yet learned are mentioned beside the count', async () => {
    const backend = makeBackend({ total: 3 });
    backend.photoList = { ...backend.photoList, files: 22898, last_scanned: 1700000100, not_yet_learned: 1200 };
    const { window } = await loadPage(backend);
    await settle(window, 50);
    assert.match(text(window, '#photo-list-status'), /22,898 photos · 1,200 not yet learned/);
});

test('a folder that could not be read is explained, in words and not as markup', async () => {
    const backend = makeBackend({ total: 3 });
    backend.photoList = {
        ...backend.photoList, files: 5, last_scanned: 1700000100,
        notes: ['The folder "<b>Folder A</b>" could not be opened, so what the list already had for it was kept.'],
    };
    const { window } = await loadPage(backend);
    await settle(window, 50);
    const notes = window.document.getElementById('photo-list-notes');
    assert.equal(notes.hidden, false);
    assert.match(notes.textContent, /<b>Folder A<\/b>/, 'shown as typed');
    assert.equal(notes.querySelector('b'), null, 'and not turned into an element');
});

test('with no notes, the notes area stays out of the way', async () => {
    const backend = makeBackend({ total: 3 });
    const { window } = await loadPage(backend);
    await settle(window, 50);
    assert.equal(window.document.getElementById('photo-list-notes').hidden, true);
});

test('a failed update says why instead of looking finished', async () => {
    const backend = makeBackend({ total: 3 });
    backend.photoList = { ...backend.photoList, last_error: 'No photo folders are set up yet.' };
    const { window } = await loadPage(backend);
    await settle(window, 50);
    assert.match(text(window, '#photo-list-status'), /could not be updated.*No photo folders are set up yet/);
});

test('there is no way to clear the list on the main screen, and Folder Settings has one', async () => {
    const backend = makeBackend({ total: 3, photoListFiles: 77 });
    const { window } = await loadPage(backend);
    assert.equal(window.document.getElementById('photo-list-row').textContent.match(/start/i), null,
        'the main screen only offers to update');
    click(window, '#photo-list-btn');
    await settle(window, 100);
    assert.match(text(window, '#photo-list-status'), /77 photos/);

    click(window, '#settings-btn');
    await settle(window, 200);
    click(window, '#photo-list-reset-btn');
    await settle(window, 200);
    assert.ok(backend.calls.some(c => c.cmd === 'photo_check_clear'));
    assert.match(text(window, '#photo-list-reset-msg'), /cleared/i);
    assert.match(text(window, '#photo-list-status'), /not made yet/i, 'and the main row follows');
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

test('updating the photo list does not touch the catalogue index', async () => {
    const backend = makeBackend({ total: 3, photoListFiles: 5 });
    const { window } = await loadPage(backend);
    const before = backend.calls.filter(c => c.cmd === 'start_refresh').length;
    click(window, '#photo-list-btn');
    await settle(window, 100);
    assert.equal(backend.calls.filter(c => c.cmd === 'start_refresh').length, before);
    assert.equal(window.document.getElementById('refresh-btn').disabled, false);
});
