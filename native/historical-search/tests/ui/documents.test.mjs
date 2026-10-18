// PDFs in the window.
//
// A PDF is a different kind of result from everything else the application
// shows: it has no catalog number and no picture, so the usual "No ID on file"
// would read as a fault, and an empty image area as a missing thumbnail. These
// check that it is presented as what it is, that opening it asks the
// application by id (never by a path the window could have made up), and that
// a file which has gone missing is explained next to the button pressed.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, type, click } from './harness.mjs';

// Values the page built live in its own realm, which deepEqual treats as
// different from this one's even when identical. Compared as plain data.
const plain = value => JSON.parse(JSON.stringify(value));

async function search(backend) {
    const { window } = await loadPage(backend);
    type(window, 'ledger');
    await settle(window);
    return window;
}

test('a PDF result is labelled PDF, not given a missing catalog number', async () => {
    const window = await search(makeBackend({ total: 3, documentIds: [2] }));
    const cards = [...window.document.querySelectorAll('.result')];
    assert.equal(cards.length, 3);

    const pdf = cards[1];
    assert.equal(pdf.querySelector('.accessno-badge').textContent, 'PDF');
    assert.ok(pdf.querySelector('.accessno-badge.doc'));
    assert.equal(pdf.querySelector('.accessno-badge.missing'), null, 'no "No ID on file" on a PDF');

    // The others are untouched.
    assert.match(cards[0].querySelector('.accessno-badge').textContent, /2010\.1\.1/);
    assert.equal(cards[0].querySelector('.accessno-badge.doc'), null);
});

test('opening a PDF card shows its page count and an Open button', async () => {
    const window = await search(makeBackend({ total: 3, documentIds: [2] }));
    click(window, '.result[data-id="2"]');
    await settle(window, 100);

    const card = window.document.querySelector('.card');
    assert.match(card.querySelector('.accessno-badge.doc').textContent, /PDF · 7 pages/);
    assert.ok(card.querySelector('#open-pdf-btn'), 'there should be a way to open it');
    assert.equal(card.querySelector('#main-image'), null, 'a PDF has no picture to show');
});

test('an ordinary record has no Open PDF button', async () => {
    const window = await search(makeBackend({ total: 3, documentIds: [2] }));
    click(window, '.result[data-id="1"]');
    await settle(window, 100);
    assert.equal(window.document.querySelector('#open-pdf-btn'), null);
});

test('Open asks the application by id', async () => {
    const backend = makeBackend({ total: 3, documentIds: [2] });
    const window = await search(backend);
    click(window, '.result[data-id="2"]');
    await settle(window, 100);
    click(window, '#open-pdf-btn');
    await settle(window, 50);

    const call = backend.calls.find(c => c.cmd === 'open_document');
    assert.deepEqual(plain(call.args), { id: 2 });
});

test('a PDF that has gone is explained on the card, not silently ignored', async () => {
    const backend = makeBackend({
        total: 3, documentIds: [2],
        openDocumentError: 'the PDF is no longer where the index recorded it',
    });
    const window = await search(backend);
    click(window, '.result[data-id="2"]');
    await settle(window, 100);
    const msg = window.document.querySelector('#open-pdf-msg');
    assert.equal(msg.hidden, true);

    click(window, '#open-pdf-btn');
    await settle(window, 50);
    assert.equal(msg.hidden, false);
    assert.match(msg.textContent, /no longer where the index recorded it/);
});

test('PDF folders are saved separately from photo folders', async () => {
    const backend = makeBackend({ total: 1 });
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        if (cmd === 'get_config') {
            return {
                data_dir: '', images_dir: '', multimedia_dir: '',
                photo_archive_roots: [{ label: 'Scans', path: '/photos/Scans' }],
                document_roots: [{ label: 'Newsletters', path: '/docs/News' }],
            };
        }
        if (cmd === 'save_config') { backend.saved = args.config; return null; }
        return original(cmd, args);
    };
    const { window } = await loadPage(backend);
    click(window, '#settings-btn');
    await settle(window, 150);

    const pdfRows = window.document.querySelectorAll('#pdf-roots-list .photo-root-row');
    assert.equal(pdfRows.length, 1);
    assert.equal(pdfRows[0].querySelector('.root-path').value, '/docs/News');
    assert.equal(window.document.querySelectorAll('#photo-roots-list .photo-root-row').length, 1);

    // Add a second PDF folder and save.
    click(window, '#add-pdf-root-btn');
    const added = window.document.querySelectorAll('#pdf-roots-list .photo-root-row')[1];
    added.querySelector('.root-label').value = 'Minutes';
    added.querySelector('.root-path').value = '/docs/Minutes';
    click(window, '#settings-save');
    await settle(window, 100);

    assert.deepEqual(plain(backend.saved.document_roots), [
        { label: 'Newsletters', path: '/docs/News' },
        { label: 'Minutes', path: '/docs/Minutes' },
    ]);
    assert.deepEqual(plain(backend.saved.photo_archive_roots), [{ label: 'Scans', path: '/photos/Scans' }],
        'a PDF folder must not leak into the photo folders');
});

test('a PDF folder is checked as a PDF folder', async () => {
    const backend = makeBackend({ total: 1 });
    const original = backend.invoke;
    backend.kinds = [];
    backend.invoke = async (cmd, args) => {
        if (cmd === 'get_config') {
            return { data_dir: '', images_dir: '', multimedia_dir: '', photo_archive_roots: [],
                     document_roots: [{ label: 'News', path: '/docs/News' }] };
        }
        if (cmd === 'validate_path') backend.kinds.push(args.kind);
        return original(cmd, args);
    };
    const { window } = await loadPage(backend);
    click(window, '#settings-btn');
    await settle(window, 700);
    assert.ok(backend.kinds.includes('pdf_folder'), `checked as ${backend.kinds}`);
});
