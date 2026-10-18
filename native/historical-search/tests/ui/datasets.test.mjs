// Narrowing a search to the datasets it found results in.
//
// The list beside the results names each dataset (a PastPerfect catalogue, a
// photo folder, PDFs, a spreadsheet) with how many of the matches are in it.
// Ticking some narrows the list to them; the others stay listed with their
// numbers, so the choice can be widened again without searching afresh.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, type, click } from './harness.mjs';

async function search(backend) {
    const { window } = await loadPage(backend);
    type(window, 'ledger');
    await settle(window);
    return window;
}

const facets = window => [...window.document.querySelectorAll('#facets label')]
    .map(l => `${l.querySelector('.name').textContent}:${l.querySelector('.n').textContent}:${l.querySelector('input').checked}`);
const shown = window => window.document.querySelectorAll('.result').length;
const tick = (window, name) => {
    const box = window.document.querySelector(`#facets input[data-dataset="${name}"]`);
    box.checked = !box.checked;
    box.dispatchEvent(new window.Event('change', { bubbles: true }));
};

test('each dataset with matches is listed with how many', async () => {
    const window = await search(makeBackend({ total: 5, documentIds: [2, 4] }));
    assert.equal(window.document.getElementById('facets').hidden, false);
    assert.deepEqual(facets(window), ['Photo:3:false', 'Newsletters:2:false']);
    assert.equal(shown(window), 5);
});

test('ticking a dataset narrows the results to it, and the others stay listed', async () => {
    const window = await search(makeBackend({ total: 5, documentIds: [2, 4] }));
    tick(window, 'Newsletters');
    await settle(window, 100);
    assert.equal(shown(window), 2);
    assert.equal(window.document.getElementById('count').textContent, '2 results');
    assert.deepEqual(facets(window), ['Photo:3:false', 'Newsletters:2:true'],
        'the number beside Photo is what a search for all would find, so it can be added back');

    tick(window, 'Photo');
    await settle(window, 100);
    assert.equal(shown(window), 5);
});

test('Show all datasets clears the choice', async () => {
    const window = await search(makeBackend({ total: 5, documentIds: [2, 4] }));
    assert.equal(window.document.getElementById('facets-clear'), null, 'nothing to clear yet');
    tick(window, 'Photo');
    await settle(window, 100);
    assert.equal(shown(window), 3);
    click(window, '#facets-clear');
    await settle(window, 100);
    assert.equal(shown(window), 5);
    assert.deepEqual(facets(window).map(f => f.split(':')[2]), ['false', 'false']);
});

test('the search asks for exactly the datasets that are ticked', async () => {
    const backend = makeBackend({ total: 5, documentIds: [2, 4] });
    const window = await search(backend);
    tick(window, 'Newsletters');
    await settle(window, 100);
    const last = backend.calls.filter(c => c.cmd === 'search').at(-1).args;
    assert.deepEqual([...last.datasets], ['Newsletters']);
    assert.equal(last.offset, 0);
});

test('a search with only one dataset in it offers no choice', async () => {
    const window = await search(makeBackend({ total: 3 }));
    assert.equal(window.document.getElementById('facets').hidden, true);
});

test('a choice that finds nothing is still shown, ticked, so it can be undone', async () => {
    const backend = makeBackend({ total: 3 });
    const window = await search(backend);
    // A dataset the stand-in has nothing in, chosen as if by an earlier search.
    window.eval(`chosenDatasets.clear(); chosenDatasets.add('Gone'); searchAgain();`);
    await settle(window, 100);
    assert.equal(shown(window), 0);
    assert.match(window.document.querySelector('.empty').textContent, /chosen datasets/);
    assert.ok(facets(window).includes('Gone:0:true'), `got ${JSON.stringify(facets(window))}`);
});
