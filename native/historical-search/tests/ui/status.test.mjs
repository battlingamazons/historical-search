// The status line and the problems reported beneath it.
//
// Several problems can apply at once -- overlapping photo folders, a folder
// that could not be read, a missing memo file, an index that predates
// per-image rows. They used to be joined into the status line itself, which
// ran to several hundred characters and read as one unbroken paragraph.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle } from './harness.mjs';

const WARNINGS = [
    'The photo folder "Scans" is inside "All", so its pictures are listed twice.',
    'The photo folder "Slides" could not be read, so none of its pictures are in the index.',
    'PHOTOS.DBF: the memo file (.FPT) is missing or unreadable.',
];

async function withStatus(status) {
    const backend = makeBackend({ total: 100 });
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => cmd === 'get_status'
        ? { last_updated: 1757000000, record_count: 1234, rebuilds_completed: 0,
            in_progress: false, last_error: null, warnings: [], phase: null,
            phase_current: null, phase_total: null, ...status }
        : original(cmd, args);
    const { window } = await loadPage(backend);
    await settle(window, 150);
    return window;
}

const warningTexts = w => [...w.document.querySelectorAll('#warnings .warning')].map(e => e.textContent);

test('with nothing wrong, the status line says when and how much', async () => {
    const window = await withStatus({});
    assert.match(window.document.getElementById('last-updated').textContent, /1,234 records/);
    assert.equal(window.document.getElementById('warnings').hidden, true, 'nothing to report');
});

test('an index that has never been built says so', async () => {
    const window = await withStatus({ last_updated: null, record_count: 0 });
    assert.match(window.document.getElementById('last-updated').textContent, /not built yet/);
});

test('each problem is shown on its own rather than run together', async () => {
    const window = await withStatus({ warnings: WARNINGS });
    const shown = warningTexts(window);
    assert.equal(shown.length, 3, 'one for each problem');
    for (const w of WARNINGS) {
        assert.ok(shown.some(s => s.includes(w)), `missing: ${w}`);
    }
    // And the status line stays short enough to read beside the buttons.
    const line = window.document.getElementById('last-updated').textContent;
    assert.ok(line.length < 120,
        `the status line should not carry the problems too, it is ${line.length} characters`);
    assert.ok(!line.includes('photo folder'), 'problems belong below, not in the status line');
});

test('problems clear when they no longer apply', async () => {
    const backend = makeBackend({ total: 100 });
    let warnings = WARNINGS;
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => cmd === 'get_status'
        ? { last_updated: 1757000000, record_count: 1234, rebuilds_completed: 0,
            in_progress: false, last_error: null, warnings, phase: null,
            phase_current: null, phase_total: null }
        : original(cmd, args);
    const { window } = await loadPage(backend);
    await settle(window, 150);
    assert.equal(warningTexts(window).length, 3);

    warnings = [];                       // the rebuild fixed them
    const { click } = await import('./harness.mjs');
    click(window, '#refresh-btn');
    await settle(window, 900);
    assert.equal(warningTexts(window).length, 0, 'resolved problems should stop being shown');
    assert.equal(window.document.getElementById('warnings').hidden, true);
});

test('a folder name that looks like markup is shown as text', async () => {
    const window = await withStatus({ warnings: ['The photo folder "<img src=x onerror=\'window.__broke=1\'>" could not be read.'] });
    assert.equal(window.__broke, undefined, 'a folder name ran as code');
    assert.equal(window.document.querySelectorAll('#warnings img').length, 0);
    assert.ok(warningTexts(window)[0].includes('<img src=x'), 'shown as written');
});
