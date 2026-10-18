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

test('text recognition is on by default and the choice is saved', async () => {
    const backend = makeBackend({ total: 1 });
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        if (cmd === 'save_config') { backend.saved = args.config; return null; }
        return original(cmd, args);
    };
    const { window } = await loadPage(backend);
    click(window, '#settings-btn');
    await settle(window, 150);

    const box = window.document.getElementById('recognize-scans');
    assert.equal(box.checked, true, 'on unless someone turns it off');
    click(window, '#settings-save');
    await settle(window, 100);
    assert.equal(backend.saved.recognize_scanned_pdfs, true);

    // Turned off, and the choice is what gets saved.
    click(window, '#settings-btn');
    await settle(window, 150);
    window.document.getElementById('recognize-scans').checked = false;
    click(window, '#settings-save');
    await settle(window, 100);
    assert.equal(backend.saved.recognize_scanned_pdfs, false);
});

test('settings saved before text recognition existed show it as on', async () => {
    const backend = makeBackend({ total: 1 });
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        if (cmd === 'get_config') {
            // An older settings file: the field is simply absent.
            return { data_dir: '', images_dir: '', multimedia_dir: '', photo_archive_roots: [], document_roots: [] };
        }
        return original(cmd, args);
    };
    const { window } = await loadPage(backend);
    click(window, '#settings-btn');
    await settle(window, 150);
    assert.equal(window.document.getElementById('recognize-scans').checked, true);
});

const HITS = [
    { page: 12, words: 2, snippet: 'The Hartwell family donated the loom to the museum' },
    { page: 3, words: 1, snippet: 'Minutes of the annual meeting, Hartwell reported' },
];

async function openPdf(backend, query = 'hartwell') {
    const { window } = await loadPage(backend);
    type(window, query);
    await settle(window);
    click(window, '.result[data-id="2"]');
    await settle(window, 150);
    return { window, card: window.document.querySelector('.card') };
}

test('the pages a search matched are listed, best first, with the words marked', async () => {
    const backend = makeBackend({ total: 3, documentIds: [2], pageHits: HITS });
    const { window, card } = await openPdf(backend);

    const asked = backend.calls.find(c => c.cmd === 'find_pages');
    assert.deepEqual(plain(asked.args), { id: 2, query: 'hartwell' }, 'asked about this PDF and what was typed');

    const rows = [...card.querySelectorAll('#doc-pages .page-row')];
    assert.deepEqual(rows.map(r => r.querySelector('.page-no').textContent), ['Page 12', 'Page 3']);
    assert.match(rows[0].querySelector('.page-snippet').innerHTML, /<mark>Hartwell<\/mark> family/);
    assert.equal(card.querySelector('#doc-pages').hidden, false);
    assert.ok(card.classList.contains('wide'), 'a PDF gets the wide card');
});

test('a PDF that matched only by name has no page list', async () => {
    const backend = makeBackend({ total: 3, documentIds: [2], pageHits: [] });
    const { card } = await openPdf(backend);
    assert.equal(card.querySelector('#doc-pages').hidden, true);
});

test('when the viewer cannot start, the card says so and the other ways still work', async () => {
    // jsdom cannot load the viewer's script, which is exactly the failure to
    // plan for: a webview too old for it, or a file that failed to load.
    const backend = makeBackend({ total: 3, documentIds: [2], pageHits: HITS });
    const { window, card } = await openPdf(backend);
    await settle(window, 200);
    assert.match(card.querySelector('#pdf-viewer').textContent, /could not be shown here/);
    assert.ok(card.querySelector('#open-pdf-btn'), 'opening it elsewhere is still offered');
    assert.ok(card.querySelector('#reveal-pdf-btn'));
});

test('Show in folder asks by id, and says so when it cannot', async () => {
    const backend = makeBackend({ total: 3, documentIds: [2] });
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        if (cmd === 'reveal_document') { backend.calls.push({ cmd, args }); throw 'the computer could not show this PDF in its folder'; }
        return original(cmd, args);
    };
    const { window, card } = await openPdf(backend);
    click(window, '#reveal-pdf-btn');
    await settle(window, 50);
    assert.deepEqual(plain(backend.calls.find(c => c.cmd === 'reveal_document').args), { id: 2 });
    assert.match(card.querySelector('#open-pdf-msg').textContent, /could not show this PDF/);
});

test('a PDF that has gone says so instead of showing a blank page', async () => {
    const backend = makeBackend({ total: 3, documentIds: [2], documentSize: 0 });
    const { card } = await openPdf(backend);
    assert.match(card.querySelector('#pdf-viewer').textContent, /no longer where the index recorded it/);
    assert.equal(backend.calls.some(c => c.cmd === 'read_document_range'), false);
});
