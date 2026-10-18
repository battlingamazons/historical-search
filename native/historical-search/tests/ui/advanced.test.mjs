// The search tips, and marking only what a search was looking for.
//
// What the box understands (AND, OR, NOT, a minus, quotation marks, brackets)
// is worked out by the application, which also says which words were being
// looked for. The window marks those and never the words that were excluded.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, type, click } from './harness.mjs';

test('Search tips lists the operators, and an example fills the search box', async () => {
    const backend = makeBackend({ total: 3 });
    const { window } = await loadPage(backend);
    click(window, '#tips-btn');
    await settle(window, 50);

    const text = window.document.querySelector('.tips-card').textContent;
    for (const word of ['OR', 'NOT', 'AND', 'town hall']) assert.ok(text.includes(word), `mentions ${word}`);
    assert.match(text, /capital/i, 'says the operators need capitals');

    click(window, '.tip-example[data-search="school NOT high"]');
    await settle(window, 200);
    assert.equal(window.document.getElementById('q').value, 'school NOT high');
    assert.equal(window.document.getElementById('overlay').hidden, true, 'the tips close');
    const asked = backend.calls.filter(c => c.cmd === 'search').at(-1).args;
    assert.equal(asked.query, 'school NOT high', 'the whole text is sent, operators and all');
});

test('only the words searched for are marked, not the excluded ones', async () => {
    const backend = makeBackend({ total: 2 });
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        const out = await original(cmd, args);
        if (cmd === 'search') {
            out.results.forEach(r => { r.snippet = 'a high school and a junior school'; });
            // What the application says it looked for.
            out.terms = ['school'];
            out.words = ['school'];
        }
        return out;
    };
    const { window } = await loadPage(backend);
    type(window, 'school NOT high');
    await settle(window);

    const marked = [...window.document.querySelectorAll('.snippet mark')].map(m => m.textContent.toLowerCase());
    assert.ok(marked.length >= 2 && marked.every(m => m === 'school'), `marked ${JSON.stringify(marked)}`);
});

test('a plain search is marked as it always was', async () => {
    const backend = makeBackend({ total: 1 });
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        const out = await original(cmd, args);
        if (cmd === 'search') out.results.forEach(r => { r.snippet = 'the 2010.19.18 loom'; });
        return out;
    };
    const { window } = await loadPage(backend);
    type(window, '2010.19.18');
    await settle(window);
    assert.deepEqual([...window.document.querySelectorAll('.snippet mark')].map(m => m.textContent), ['2010.19.18']);
});
