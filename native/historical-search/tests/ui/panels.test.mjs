// Panels that are fetched before they can be shown.
//
// A record, Folder Settings and Diagnostics are all asked for and then drawn
// when the answer arrives. Each used to draw whatever happened, so dismissing
// one that was slow brought it back by itself, and asking for a different one
// meant whichever answered last won -- settings could appear on top of the
// record you had opened instead.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, type, click } from './harness.mjs';

test('Folder Settings dismissed before it appears stays dismissed', async () => {
    const backend = makeBackend({ total: 10 });
    const { window } = await loadPage(backend);
    backend.holdConfig = true;
    click(window, '#settings-btn');
    await settle(window, 80);

    // Change your mind before the settings arrive.
    window.document.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    await settle(window, 40);
    assert.equal(window.document.getElementById('overlay').hidden, true, 'dismissed');

    backend.release();
    await settle(window, 150);
    const reopened = !window.document.getElementById('overlay').hidden;
    assert.equal(reopened, false, 'settings dismissed before they appeared should stay dismissed');
});

test('settings arriving late do not replace the record being looked at', async () => {
    const backend = makeBackend({ total: 10 });
    const { window } = await loadPage(backend);
    type(window, 'photo');
    await settle(window);

    backend.holdConfig = true;
    click(window, '#settings-btn');
    await settle(window, 60);
    window.document.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    await settle(window, 40);
    click(window, '.result[data-id="3"]');     // look at a record instead
    await settle(window, 100);
    const showingRecord = window.document.querySelector('#card h2')?.textContent;

    backend.release();                          // the settings finally arrive
    await settle(window, 150);
    const nowShowing = window.document.querySelector('#card h2')?.textContent;
    const isSettings = !!window.document.querySelector('.settings-card');
    assert.ok(!isSettings, 'settings should not replace the record being looked at');
});
