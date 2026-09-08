import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, type, pressEnter, resultIds, click } from './harness.mjs';

test('asking for more, then pressing Enter for everything', async () => {
    const backend = makeBackend({ total: 300 });
    const { window } = await loadPage(backend);
    type(window, 'photo');
    await settle(window);
    assert.equal(resultIds(window).length, 40);

    backend.holdSearches = true;
    click(window, '#show-more-btn');      // page two, in flight
    await settle(window, 60);
    pressEnter(window);                   // everything, same query
    await settle(window, 60);
    assert.equal(backend.pending.length, 2);

    backend.release('lifo');              // "everything" lands first
    await settle(window, 150);

    const ids = resultIds(window);
    console.log(`RESULT rows=${ids.length} distinct=${new Set(ids).size}`);
    assert.equal(new Set(ids).size, ids.length, 'a record should not be listed twice');
    assert.equal(ids.length, 300, 'the full set, and only the full set');
});

test('asking for more, then clearing the box', async () => {
    const backend = makeBackend({ total: 300 });
    const { window } = await loadPage(backend);
    type(window, 'photo');
    await settle(window);

    backend.holdSearches = true;
    click(window, '#show-more-btn');
    await settle(window, 60);
    click(window, '#clear-btn');          // the results are wiped
    await settle(window, 60);
    assert.equal(resultIds(window).length, 0, 'clearing should empty the list');

    backend.release('fifo');              // the page arrives afterwards
    await settle(window, 150);
    console.log(`AFTER-CLEAR rows=${resultIds(window).length}`);
    assert.equal(resultIds(window).length, 0,
        'a page fetched before clearing should not repopulate an emptied list');
});

test('dismissing a record that is slow to arrive keeps it dismissed', async () => {
    // Every step asks the Rust side for the record and paints when it comes
    // back. Nothing marked the request as unwanted when the card was closed,
    // so a record that took its time reopened the card over whatever the
    // person had moved on to.
    const backend = makeBackend({ total: 60 });
    const { window } = await loadPage(backend);
    type(window, 'photo');
    await settle(window);

    backend.holdItems = true;
    click(window, '.result[data-id="1"]');
    await settle(window, 60);
    window.document.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    await settle(window, 40);
    assert.equal(window.document.getElementById('overlay').hidden, true, 'the card should close');

    backend.release();
    await settle(window, 150);
    assert.equal(window.document.getElementById('overlay').hidden, true,
        'a record arriving after being dismissed should not reopen the card');
});

test('the last page is offered only when there is one', async () => {
    // Off-by-one country: exactly a full page, and one more than a full page.
    for (const [total, expectMore] of [[40, false], [41, true]]) {
        const backend = makeBackend({ total });
        const { window } = await loadPage(backend);
        type(window, 'photo');
        await settle(window);
        assert.equal(!!window.document.getElementById('show-more-btn'), expectMore,
            `with ${total} matches, offering more should be ${expectMore}`);
    }
});

test('everything beyond what Enter loads at once is still reachable', async () => {
    // Pressing Enter is capped; the rest has to remain reachable, and the
    // join between the two must not repeat or skip a record.
    const backend = makeBackend({ total: 2500 });
    const { window } = await loadPage(backend);
    type(window, 'photo');
    await settle(window);
    pressEnter(window);
    await settle(window, 400);
    assert.equal(resultIds(window).length, 2000, 'the cap');
    assert.ok(window.document.getElementById('show-more-btn'), 'the rest must still be reachable');

    click(window, '#show-more-btn');
    await settle(window, 400);
    const ids = resultIds(window);
    assert.equal(ids.length, 2040);
    assert.equal(new Set(ids).size, ids.length, 'no record repeated across the boundary');

    // And stepping between records must cross it too.
    click(window, '.result[data-id="2000"]');
    await settle(window, 200);
    click(window, '#next-item-btn');
    await settle(window, 400);
    assert.match(window.document.querySelector('#card h2').textContent, /2001/,
        'stepping on should continue past the cap');
});

test('a single result reads as one, and has nowhere to step', async () => {
    const backend = makeBackend({ total: 1 });
    const { window } = await loadPage(backend);
    type(window, 'photo');
    await settle(window);
    assert.match(window.document.getElementById('count').textContent, /^1 result$/, 'not "1 results"');

    click(window, '.result[data-id="1"]');
    await settle(window, 120);
    assert.equal(window.document.getElementById('prev-item-btn').disabled, true);
    assert.equal(window.document.getElementById('next-item-btn').disabled, true);
});
