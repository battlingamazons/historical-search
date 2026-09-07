// The window's search state machine.
//
// Several handlers write to the same few variables -- which query is current,
// how far through the results we are, which records are loaded, and which one
// is open -- and they can run in any order because each is waiting on the Rust
// side. These check the combinations a person actually produces: typing while
// a page is still loading, pressing Enter part-way through, stepping past the
// end of what has been loaded.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, type, pressEnter, resultIds, resultQueries, click } from './harness.mjs';

test('typing shows results, and the count is the real total rather than what fits on a page', async () => {
    const backend = makeBackend({ total: 137 });
    const { window } = await loadPage(backend);

    type(window, 'photo');
    await settle(window);

    assert.equal(resultIds(window).length, 40, 'one page of results should be shown');
    assert.match(window.document.getElementById('count').textContent, /137 results/);
    assert.ok(window.document.getElementById('show-more-btn'), 'more results should be offered');
});

test('an empty search clears the results rather than leaving the previous ones up', async () => {
    const backend = makeBackend({ total: 50 });
    const { window } = await loadPage(backend);

    type(window, 'photo');
    await settle(window);
    assert.ok(resultIds(window).length > 0);

    type(window, '');
    await settle(window);
    assert.equal(resultIds(window).length, 0, 'clearing the box should clear the results');
    assert.equal(window.document.getElementById('count').textContent, '');
});

test('Show more appends the next page without repeating or skipping records', async () => {
    const backend = makeBackend({ total: 100 });
    const { window } = await loadPage(backend);

    type(window, 'photo');
    await settle(window);
    assert.deepEqual(resultIds(window), Array.from({ length: 40 }, (_, i) => String(i + 1)));

    click(window, '#show-more-btn');
    await settle(window);
    assert.equal(resultIds(window).length, 80, 'the second page should be added to the first');

    click(window, '#show-more-btn');
    await settle(window);
    const ids = resultIds(window);
    assert.equal(ids.length, 100, 'the last page completes the set');
    assert.equal(new Set(ids).size, ids.length, 'no record should appear twice');
    assert.deepEqual(ids, Array.from({ length: 100 }, (_, i) => String(i + 1)), 'and none skipped');
    assert.equal(window.document.getElementById('show-more-btn'), null, 'nothing more to offer');
});

test('pressing Enter loads everything at once', async () => {
    const backend = makeBackend({ total: 300 });
    const { window } = await loadPage(backend);

    type(window, 'photo');
    await settle(window);
    assert.equal(resultIds(window).length, 40);

    pressEnter(window);
    await settle(window);
    assert.equal(resultIds(window).length, 300, 'Enter should bring back the whole set');
});

test('a slow answer to an old search does not overwrite a newer one', async () => {
    // The failure this guards against: type something, type something else
    // before the first answer arrives, and end up looking at results for a
    // query you have already moved on from.
    const backend = makeBackend({ total: 60 });
    const { window } = await loadPage(backend);

    backend.holdSearches = true;
    type(window, 'first');
    await settle(window, 200);
    type(window, 'second');
    await settle(window, 200);
    assert.equal(backend.pending.length, 2, 'both searches should be in flight');

    // Answer the newer one first, then the stale one.
    backend.release('lifo');
    await settle(window, 60);

    const q = window.document.getElementById('q');
    assert.equal(q.value, 'second');
    assert.equal(resultIds(window).length, 40, 'results should be shown once, not twice over');
    // The point of the guard: what is on screen must belong to what is in the
    // box. Counting results cannot tell the difference, since both searches
    // return a full page.
    assert.deepEqual([...new Set(resultQueries(window))], ['second'],
        'the stale answer overwrote the results for the query actually typed');
});

test('loading another page while a new search is under way does not mix the two together', async () => {
    const backend = makeBackend({ total: 200 });
    const { window } = await loadPage(backend);

    type(window, 'photo');
    await settle(window);
    assert.equal(resultIds(window).length, 40);

    // Ask for more, then change the search before the page comes back. The
    // new search is answered first, so the stale page arrives once the
    // results on screen already belong to something else -- which is the
    // order that actually catches a missing guard.
    backend.holdSearches = true;
    click(window, '#show-more-btn');
    await settle(window, 50);
    type(window, 'different');
    await settle(window, 200);
    assert.equal(backend.pending.length, 2, 'both the extra page and the new search should be waiting');

    backend.release('lifo');           // the new search answers first
    await settle(window, 120);

    const ids = resultIds(window);
    const queries = resultQueries(window);
    assert.equal(new Set(ids).size, ids.length, 'no record should appear twice');
    assert.deepEqual([...new Set(queries)], ['different'],
        `every result should belong to the current search, found: ${[...new Set(queries)].join(', ')}`);
    assert.equal(ids.length, 40, `expected exactly one page for the new search, got ${ids.length}`);
});

test('the clear button empties the box and the results', async () => {
    const backend = makeBackend({ total: 80 });
    const { window } = await loadPage(backend);

    type(window, 'photo');
    await settle(window);
    assert.equal(window.document.getElementById('clear-btn').hidden, false);

    click(window, '#clear-btn');
    await settle(window, 60);
    assert.equal(window.document.getElementById('q').value, '');
    assert.equal(resultIds(window).length, 0);
    assert.equal(window.document.getElementById('clear-btn').hidden, true);
});

test('an exact catalog-number match is marked, and only that one', async () => {
    const backend = makeBackend({ total: 30, exactIdFor: '2010.1.7' });
    const { window } = await loadPage(backend);

    type(window, '2010.1.7');
    await settle(window);

    const marked = [...window.document.querySelectorAll('.result.exact-match')];
    assert.equal(marked.length, 1, 'exactly one result should be marked as the exact match');
    assert.equal(marked[0].dataset.id, '7');
    assert.match(marked[0].textContent, /Exact ID match/);
});
