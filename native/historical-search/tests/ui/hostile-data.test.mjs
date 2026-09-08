// Catalogue text that happens to look like markup.
//
// Everything on screen comes from a database volunteers type into, and titles
// and descriptions there contain ampersands, quotation marks and angle
// brackets as a matter of course -- "Smith & Sons", 'the "new" school', a
// measurement given as 6" x 4". None of that should be able to change the
// shape of the page, whether by accident or otherwise.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, type, click } from './harness.mjs';

const NASTY = `<script>window.__broke = true;</script>` +
    `<img src=x onerror="window.__broke = true">` +
    `" onmouseover="window.__broke = true" ` +
    `'&amp;<b>bold</b> & 6" x 4' <>`;

async function pageShowing(record) {
    const backend = makeBackend({ total: 1 });
    Object.assign(backend.records[0], record);
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        const r = await original(cmd, args);
        if (cmd === 'get_item' && r) {
            r.title = record.title ?? r.title;
            r.accessno = record.accessno ?? r.accessno;
            r.catalog = record.catalog ?? r.catalog;
            r.fields = [{ label: NASTY, value: NASTY }];
            r.images = [{ id: 1, kind: 'imgthumb', caption: NASTY, has_thumb: true, has_master: true }];
        }
        return r;
    };
    const dom = await loadPage(backend);
    type(dom.window, 'photo');
    await settle(dom.window);
    return dom.window;
}

test('markup in a record title or number is shown, not obeyed', async () => {
    const window = await pageShowing({ title: NASTY, accessno: NASTY, catalog: NASTY, snippet: NASTY });

    assert.equal(window.__broke, undefined, 'text from the catalogue ran as code');
    assert.equal(window.document.querySelectorAll('#results script').length, 0,
        'a script element was created from catalogue text');
    assert.equal(window.document.querySelectorAll('#results img[src="x"]').length, 0,
        'an image element was created from catalogue text');

    // And it is still readable: the characters should appear as written.
    const shown = window.document.querySelector('.result').textContent;
    assert.ok(shown.includes('6" x 4'), 'quotation marks should survive as themselves');
    assert.ok(shown.includes('<b>bold</b>'), 'angle brackets should be shown, not interpreted');
    assert.equal(window.document.querySelectorAll('#results b').length, 0,
        'no element should have been made from the text');
});

test('markup in a detail card, its fields and its captions is shown, not obeyed', async () => {
    const window = await pageShowing({ title: NASTY });
    click(window, '.result[data-id="1"]');
    await settle(window, 120);

    assert.equal(window.__broke, undefined, 'text from the catalogue ran as code');
    const card = window.document.getElementById('card');
    assert.equal(card.querySelectorAll('script, b, img[src="x"]').length, 0,
        'elements were created from catalogue text');
    assert.ok(card.querySelector('.field .value').textContent.includes('<b>bold</b>'),
        'field values should be shown as written');

    // The caption also goes into a title attribute on the strip, which is a
    // second way out of the text and into the markup.
    const stripTitle = card.querySelector('.strip-thumb')?.getAttribute('title');
    if (stripTitle !== null && stripTitle !== undefined) {
        assert.ok(stripTitle.includes('onmouseover'),
            'the caption should be inside the attribute, not have escaped it');
        assert.equal(card.querySelectorAll('[onmouseover]').length, 0,
            'the caption broke out of the attribute it was placed in');
    }
});

test('searching for something that looks like markup highlights safely', async () => {
    // The search terms are used to build a pattern and wrapped in marks, so
    // they are a second path from typed text into the page.
    const backend = makeBackend({ total: 3 });
    backend.records.forEach(r => { r.title = `a ${NASTY} title`; });
    const { window } = await loadPage(backend);

    type(window, '<script>');
    await settle(window);
    assert.equal(window.__broke, undefined, 'the search box ran as code');
    assert.equal(window.document.querySelectorAll('#results script').length, 0);

    type(window, '&');
    await settle(window);
    const marked = window.document.querySelector('#results mark');
    if (marked) {
        assert.equal(marked.textContent, '&',
            'an ampersand should highlight as itself, not as part of an escape sequence');
    }
    assert.ok(window.document.querySelector('.result').textContent.includes('&'),
        'the ampersand should still be readable in the result');
});

test('a folder path that looks like markup is put in the field, not the page', async () => {
    const backend = makeBackend({ total: 1 });
    const original = backend.invoke;
    backend.invoke = async (cmd, args) =>
        cmd === 'get_config'
            ? { data_dir: NASTY, images_dir: '', multimedia_dir: '',
                photo_archive_roots: [{ label: NASTY, path: NASTY }] }
            : original(cmd, args);
    const { window } = await loadPage(backend);

    click(window, '#settings-btn');
    await settle(window, 200);

    assert.equal(window.__broke, undefined, 'a saved folder path ran as code');
    const card = window.document.getElementById('card');
    assert.equal(card.querySelectorAll('script, b, img[src="x"]').length, 0,
        'elements were created from a saved folder path');
    assert.equal(card.querySelector('#data-dir-input').value, NASTY,
        'the path should be in the field exactly as saved');
});

test('a second search word never damages the highlighting of the first', async () => {
    // Terms used to be marked one after another, over text that already held
    // the tags added by earlier ones. Any later term matching the letters of
    // "mark" then tore the tag apart -- and typing a second word beginning
    // with a, m, r or k does exactly that, which is most of them: "photo
    // album", "school records", "map of the town".
    const backend = makeBackend({ total: 4 });
    backend.records.forEach(r => { r.title = 'Photo of the archive'; r.snippet = 'a marked album'; });
    const { window } = await loadPage(backend);

    for (const query of ['photo a', 'photo ar', 'photo mark', 'photo m', 'photo k', 'photo rk']) {
        type(window, query);
        await settle(window);
        const row = window.document.querySelector('.result');
        assert.ok(row, `no results for ${query}`);
        const shown = row.textContent;
        assert.ok(!shown.includes('rk>') && !shown.includes('<m'),
            `typing ${JSON.stringify(query)} left the tag showing as text: ${JSON.stringify(shown)}`);
        // The snippet is left as the record wrote it, so it is the part that
        // must still read as English rather than as torn-up markup.
        assert.ok(shown.includes('a marked album'),
            `the text should still read normally for ${JSON.stringify(query)}, got ${JSON.stringify(shown)}`);
        assert.equal(row.querySelectorAll('mark').length > 0, true,
            `something should still be highlighted for ${query}`);
    }
});

test('overlapping search words highlight the longer match', async () => {
    const backend = makeBackend({ total: 2 });
    backend.records.forEach(r => { r.title = 'Photograph of a school'; });
    const { window } = await loadPage(backend);

    type(window, 'photo photograph');
    await settle(window);
    const marks = [...window.document.querySelectorAll('.result .title mark')].map(m => m.textContent);
    assert.ok(marks.some(m => m.toLowerCase() === 'photograph'),
        `the fuller word should be highlighted, got ${JSON.stringify(marks)}`);
});
