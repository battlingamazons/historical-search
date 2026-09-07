// Folder Settings.
//
// This is where someone tells the application where the collection lives, and
// getting it wrong is expensive: the folders it names are what a rebuild
// reads, and a rebuild replaces the index. Each field checks its folder as
// you type and reports back, and those checks all used to share a single
// timer, so only the last field wired up ever ran -- the others simply sat
// blank where their confirmation should have been. That was found once by
// hand and never guarded against.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, click } from './harness.mjs';

const CONFIGURED = {
    data_dir: '/collection/pp5/Data',
    images_dir: '/collection/pp5/Images',
    multimedia_dir: '/collection/pp5/Multimedia',
    photo_archive_roots: [
        { label: 'Scans', path: '/photos/Scans' },
        { label: 'Slides', path: '/photos/Slides' },
    ],
};

async function openSettings(config = CONFIGURED, validate = null) {
    const backend = makeBackend({ total: 5 });
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        if (cmd === 'get_config') return JSON.parse(JSON.stringify(config));
        if (cmd === 'validate_path' && validate) return validate(args);
        if (cmd === 'save_config') { backend.saved = args.config; return null; }
        return original(cmd, args);
    };
    const dom = await loadPage(backend);
    click(dom.window, '#settings-btn');
    await settle(dom.window, 150);
    return { backend, window: dom.window };
}

const messages = w => [...w.document.querySelectorAll('.validate-msg')].map(el => el.textContent);

test('the saved folders are shown when the dialog opens', async () => {
    const { window } = await openSettings();
    assert.equal(window.document.getElementById('data-dir-input').value, '/collection/pp5/Data');
    assert.equal(window.document.getElementById('images-dir-input').value, '/collection/pp5/Images');
    assert.equal(window.document.getElementById('multimedia-dir-input').value, '/collection/pp5/Multimedia');
    const rows = [...window.document.querySelectorAll('.photo-root-row')];
    assert.equal(rows.length, 2, 'both photo folders should be listed');
    assert.equal(rows[0].querySelector('.root-label').value, 'Scans');
    assert.equal(rows[1].querySelector('.root-path').value, '/photos/Slides');
});

test('every folder is checked, not just the last one wired up', async () => {
    // The regression this exists for. A single shared timer meant each newly
    // wired field cancelled the previous field's pending check, so only the
    // last one ever reported anything.
    const asked = [];
    const { window } = await openSettings(CONFIGURED, ({ path }) => {
        asked.push(path);
        return { ok: true, message: `checked ${path}` };
    });
    await settle(window, 600);   // the checks wait 400ms after typing stops

    for (const folder of [
        '/collection/pp5/Data', '/collection/pp5/Images',
        '/collection/pp5/Multimedia', '/photos/Scans', '/photos/Slides',
    ]) {
        assert.ok(asked.includes(folder), `${folder} was never checked`);
    }
    const shown = messages(window).filter(m => m.startsWith('checked'));
    assert.equal(shown.length, 5, `all five folders should report back, got ${shown.length}`);
});

test('a folder that cannot be used says so, next to the field it belongs to', async () => {
    const { window } = await openSettings(CONFIGURED, ({ path }) =>
        path.endsWith('Data')
            ? { ok: false, message: 'no PastPerfect tables found in it' }
            : { ok: true, message: 'looks right' });
    await settle(window, 600);

    const dataMsg = window.document.getElementById('data-dir-msg');
    assert.match(dataMsg.textContent, /no PastPerfect tables/);
    assert.ok(dataMsg.classList.contains('bad'), 'a problem should be marked as one');
    assert.ok(window.document.getElementById('images-dir-msg').classList.contains('ok'),
        'a folder that is fine should not be marked as a problem');
});

test('another photo folder can be added, and it is checked like the rest', async () => {
    const asked = [];
    const { window } = await openSettings(CONFIGURED, ({ path }) => {
        asked.push(path);
        return { ok: true, message: `checked ${path}` };
    });
    await settle(window, 600);

    click(window, '#add-root-btn');
    const rows = [...window.document.querySelectorAll('.photo-root-row')];
    assert.equal(rows.length, 3, 'a blank row should appear');

    const added = rows[2];
    added.querySelector('.root-label').value = 'Negatives';
    const path = added.querySelector('.root-path');
    path.value = '/photos/Negatives';
    path.dispatchEvent(new window.Event('input'));
    await settle(window, 600);

    assert.ok(asked.includes('/photos/Negatives'),
        'a folder added after the dialog opened must be checked too');
    assert.match(added.querySelector('.validate-msg').textContent, /checked/);
});

test('a photo folder can be removed', async () => {
    const { window } = await openSettings();
    const rows = () => [...window.document.querySelectorAll('.photo-root-row')];
    assert.equal(rows().length, 2);

    rows()[0].querySelector('.remove-btn').dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
    await settle(window, 40);

    assert.equal(rows().length, 1, 'the row should be gone');
    assert.equal(rows()[0].querySelector('.root-label').value, 'Slides', 'and the right one left');
});

test('saving sends what is on screen, tidied, and starts a rebuild', async () => {
    const { backend, window } = await openSettings();
    // Spacing of the sort that arrives with a pasted path.
    window.document.getElementById('data-dir-input').value = '  /elsewhere/Data  ';
    click(window, '#add-root-btn');
    // A blank row the user added and then did not fill in.
    click(window, '#settings-save');
    await settle(window, 300);

    assert.ok(backend.saved, 'the settings should have been saved');
    assert.equal(backend.saved.data_dir, '/elsewhere/Data', 'stray spacing should not be saved');
    assert.equal(backend.saved.photo_archive_roots.length, 2,
        'a photo folder row left empty should not be saved');
    assert.ok(backend.calls.some(c => c.cmd === 'start_refresh'),
        'saving should rebuild, since the folders may now point somewhere else');
    assert.equal(window.document.getElementById('overlay').hidden, true, 'and the dialog should close');
});

test('cancelling changes nothing', async () => {
    const { backend, window } = await openSettings();
    window.document.getElementById('data-dir-input').value = '/somewhere/else';
    click(window, '#settings-cancel');
    await settle(window, 120);

    assert.equal(backend.saved, undefined, 'nothing should have been saved');
    assert.ok(!backend.calls.some(c => c.cmd === 'start_refresh'), 'and nothing rebuilt');
    assert.equal(window.document.getElementById('overlay').hidden, true);
});

test('the folder picker fills in the field it belongs to', async () => {
    const { backend, window } = await openSettings();
    const original = backend.invoke;
    backend.invoke = async (cmd, args) =>
        cmd === 'pick_folder' ? '/chosen/by/the/picker' : original(cmd, args);

    const dataRow = window.document.getElementById('data-dir-input').parentElement;
    dataRow.querySelector('.browse-btn').dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
    await settle(window, 600);

    assert.equal(window.document.getElementById('data-dir-input').value, '/chosen/by/the/picker');
    assert.equal(window.document.getElementById('images-dir-input').value, '/collection/pp5/Images',
        'and only that field');
});
