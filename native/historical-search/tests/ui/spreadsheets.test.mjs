// Excel spreadsheets in Folder Settings.
//
// A spreadsheet row can name one workbook or a folder of them, so it offers a
// button for each; it is saved under its own setting and checked as a
// spreadsheet, not mistaken for a photo or PDF folder.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, click } from './harness.mjs';

const plain = value => JSON.parse(JSON.stringify(value));

function withConfig(config) {
    const backend = makeBackend({ total: 1 });
    const original = backend.invoke;
    backend.kinds = [];
    backend.picked = [];
    backend.invoke = async (cmd, args) => {
        if (cmd === 'get_config') {
            return { data_dir: '', images_dir: '', multimedia_dir: '', photo_archive_roots: [],
                     document_roots: [], recognize_scanned_pdfs: true, ...config };
        }
        if (cmd === 'save_config') { backend.saved = args.config; return null; }
        if (cmd === 'validate_path') backend.kinds.push(args.kind);
        if (cmd === 'pick_spreadsheet') { backend.picked.push('file'); return '/chosen/register.xlsx'; }
        if (cmd === 'pick_folder') { backend.picked.push('folder'); return '/chosen/folder'; }
        return original(cmd, args);
    };
    return backend;
}

async function openSettings(backend) {
    const { window } = await loadPage(backend);
    click(window, '#settings-btn');
    await settle(window, 150);
    return window;
}

test('saved spreadsheets are shown, and added ones are saved under their own setting', async () => {
    const backend = withConfig({ spreadsheet_roots: [{ label: 'Cemetery', path: '/sheets/register.xlsx' }] });
    const window = await openSettings(backend);

    const rows = window.document.querySelectorAll('#spreadsheet-roots-list .spreadsheet-row');
    assert.equal(rows.length, 1);
    assert.equal(rows[0].querySelector('.root-path').value, '/sheets/register.xlsx');

    click(window, '#add-spreadsheet-btn');
    const added = window.document.querySelectorAll('#spreadsheet-roots-list .spreadsheet-row')[1];
    added.querySelector('.root-label').value = 'Donors';
    added.querySelector('.root-path').value = '/sheets/donors';
    click(window, '#settings-save');
    await settle(window, 100);

    assert.deepEqual(plain(backend.saved.spreadsheet_roots), [
        { label: 'Cemetery', path: '/sheets/register.xlsx' },
        { label: 'Donors', path: '/sheets/donors' },
    ]);
    assert.deepEqual(plain(backend.saved.document_roots), [], 'not mixed in with the PDF folders');
});

test('a spreadsheet path is checked as a spreadsheet', async () => {
    const backend = withConfig({ spreadsheet_roots: [{ label: 'Cemetery', path: '/sheets/register.xlsx' }] });
    await openSettings(backend);
    await new Promise(r => setTimeout(r, 500));
    assert.ok(backend.kinds.includes('spreadsheet'), `checked as ${JSON.stringify(backend.kinds)}`);
});

test('Choose file and Choose folder each fill in the path', async () => {
    const backend = withConfig({ spreadsheet_roots: [{ label: 'A', path: '/x.xlsx' }] });
    const window = await openSettings(backend);
    const row = window.document.querySelector('.spreadsheet-row');

    click(window, '.spreadsheet-row [data-pick="file"]');
    await settle(window, 50);
    assert.equal(row.querySelector('.root-path').value, '/chosen/register.xlsx');

    click(window, '.spreadsheet-row [data-pick="folder"]');
    await settle(window, 50);
    assert.equal(row.querySelector('.root-path').value, '/chosen/folder');
    assert.deepEqual(backend.picked, ['file', 'folder']);
});

test('a spreadsheet row left empty is not saved', async () => {
    const backend = withConfig({});
    const window = await openSettings(backend);
    click(window, '#add-spreadsheet-btn');
    click(window, '#settings-save');
    await settle(window, 100);
    assert.deepEqual(plain(backend.saved.spreadsheet_roots), []);
});
