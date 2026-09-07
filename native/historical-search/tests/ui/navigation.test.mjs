// Stepping between items from an open record.
//
// The card shows a position ("12 of 247") and Previous/Next buttons whose
// enabled state depends on how much has been loaded so far, which is not the
// same as how much matched. Walking off the end of what is loaded has to
// fetch more and carry on, and none of it may survive a rebuild that
// renumbers every record.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, type, pressEnter, resultIds, click } from './harness.mjs';

async function openFirstSearch(total, query = 'photo') {
    const backend = makeBackend({ total });
    const dom = await loadPage(backend);
    type(dom.window, query);
    await settle(dom.window);
    return { backend, window: dom.window };
}

function cardTitle(window) {
    const h = window.document.querySelector('#card h2');
    return h ? h.textContent : null;
}

function position(window) {
    const el = window.document.querySelector('.nav-position');
    return el ? el.textContent.trim() : null;
}

test('opening a result shows that record, and the position counts against the full total', async () => {
    const { window } = await openFirstSearch(137);

    click(window, '.result[data-id="1"]');
    await settle(window, 80);

    assert.equal(cardTitle(window), 'Record number 1');
    assert.equal(window.document.getElementById('overlay').hidden, false);
    assert.equal(position(window), '1 of 137', 'the position should count against everything that matched');
    assert.equal(window.document.getElementById('prev-item-btn').disabled, true, 'nothing before the first');
    assert.equal(window.document.getElementById('next-item-btn').disabled, false);
});

test('Next and Previous walk through the loaded records', async () => {
    const { window } = await openFirstSearch(137);
    click(window, '.result[data-id="5"]');
    await settle(window, 80);
    assert.equal(position(window), '5 of 137');

    click(window, '#next-item-btn');
    await settle(window, 80);
    assert.equal(cardTitle(window), 'Record number 6');
    assert.equal(position(window), '6 of 137');

    click(window, '#prev-item-btn');
    await settle(window, 80);
    assert.equal(cardTitle(window), 'Record number 5');
});

test('stepping past the last loaded record fetches the next page and carries on', async () => {
    // The join between two separate mechanisms: one page is loaded, but 137
    // matched, so Next from the fortieth has to go and get more before it can
    // move.
    const { window } = await openFirstSearch(137);
    assert.equal(resultIds(window).length, 40);

    click(window, '.result[data-id="40"]');
    await settle(window, 80);
    assert.equal(position(window), '40 of 137');
    assert.equal(window.document.getElementById('next-item-btn').disabled, false,
        'more matched, so Next must still be available on the last loaded record');

    click(window, '#next-item-btn');
    await settle(window, 250);
    assert.equal(cardTitle(window), 'Record number 41', 'it should continue into the next page');
    assert.equal(position(window), '41 of 137');
    assert.equal(resultIds(window).length, 80, 'the next page should now be behind the card');
});

test('Next stops at the genuine end rather than running off it', async () => {
    const { window } = await openFirstSearch(30);
    click(window, '.result[data-id="30"]');
    await settle(window, 80);

    assert.equal(position(window), '30 of 30');
    assert.equal(window.document.getElementById('next-item-btn').disabled, true,
        'there is nothing after the last record');

    click(window, '#next-item-btn');
    await settle(window, 120);
    assert.equal(cardTitle(window), 'Record number 30', 'the card should not have moved');
});

test('the arrow keys step between records too', async () => {
    const { window } = await openFirstSearch(50);
    click(window, '.result[data-id="10"]');
    await settle(window, 80);

    const press = key => window.document.dispatchEvent(
        new window.KeyboardEvent('keydown', { key, bubbles: true }));

    press('ArrowRight');
    await settle(window, 80);
    assert.equal(cardTitle(window), 'Record number 11');

    press('ArrowLeft');
    await settle(window, 80);
    assert.equal(cardTitle(window), 'Record number 10');

    press('Escape');
    await settle(window, 40);
    assert.equal(window.document.getElementById('overlay').hidden, true, 'Escape should close the card');
});

test('a record arriving late does not replace the one now being looked at', async () => {
    // Every step asks the Rust side for a record, and with a held arrow key
    // several are in flight together. If an earlier answer is allowed to land
    // after a later one, the card shows one record while the position counter
    // describes another -- and stepping on from there goes wrong too.
    const { backend, window } = await openFirstSearch(137);
    click(window, '.result[data-id="3"]');
    await settle(window, 80);
    assert.equal(cardTitle(window), 'Record number 3');

    backend.holdItems = true;
    click(window, '#next-item-btn');      // asks for 4
    await settle(window, 30);
    click(window, '#next-item-btn');      // asks for 5
    await settle(window, 30);
    assert.equal(backend.pending.length, 2, 'both records should be in flight');

    // The later request answers first; the earlier one arrives afterwards and
    // must be discarded rather than painted over the top.
    backend.release('lifo');
    await settle(window, 120);

    assert.equal(cardTitle(window), 'Record number 5',
        'the card should show the record most recently asked for');
    assert.equal(position(window), '5 of 137',
        'and the counter should agree with it');
});

test('an open record is closed when the index is rebuilt underneath it', async () => {
    // Rebuilding renumbers every record, so the ids the open card is holding
    // would then point somewhere else -- Download would fetch a different
    // picture and Next would jump to an unrelated record, neither of which
    // looks wrong on screen.
    const { backend, window } = await openFirstSearch(60);
    click(window, '.result[data-id="2"]');
    await settle(window, 80);
    assert.equal(window.document.getElementById('overlay').hidden, false);

    click(window, '#refresh-btn');
    await settle(window, 900);

    assert.equal(window.document.getElementById('overlay').hidden, true,
        'the card should not survive a rebuild');
    assert.match(window.document.getElementById('last-updated').textContent,
        /closed because the index was rebuilt/);
    assert.ok(backend.calls.some(c => c.cmd === 'start_refresh'));
});

test('Folder Settings is not closed by a rebuild finishing', async () => {
    // The same sweep must not throw away what someone is in the middle of
    // typing into the settings dialog.
    const { window } = await openFirstSearch(60);
    click(window, '#refresh-btn');
    await settle(window, 60);
    click(window, '#settings-btn');
    await settle(window, 120);
    assert.ok(window.document.querySelector('.settings-card'), 'settings should be open');

    await settle(window, 900);
    assert.equal(window.document.getElementById('overlay').hidden, false,
        'settings must stay open when a rebuild finishes');
    assert.ok(window.document.querySelector('.settings-card'));
});
