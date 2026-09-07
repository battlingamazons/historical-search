// The agreement between the window and the Rust side.
//
// These two test suites cannot see each other. The Rust tests never load the
// window; the window's tests run against a stand-in backend written by hand.
// So a field renamed on the Rust side leaves both suites passing while they
// disagree about what actually crosses between them, and the only symptom is
// the application quietly ceasing to show something.
//
// One list of field names, checked from both directions: a Rust test compares
// it against what the types really serialise to, and these compare it against
// the stand-in the window's tests believe in.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { makeBackend, loadPage, settle, type, click } from './harness.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..', '..');
const shapes = JSON.parse(readFileSync(join(root, 'src-tauri', 'tests', 'command-shapes.json'), 'utf8'));

const keys = obj => Object.keys(obj).sort();

test('the stand-in backend answers with the fields the Rust side really sends', async () => {
    const backend = makeBackend({ total: 3 });

    const response = await backend.invoke('search', { query: 'x', offset: 0, showAll: false });
    assert.deepEqual(keys(response), shapes.SearchResponse, 'search response');
    assert.deepEqual(keys(response.results[0]), shapes.SearchResult, 'a search result');

    const item = await backend.invoke('get_item', { id: 1 });
    assert.deepEqual(keys(item), shapes.ItemDetail, 'item detail');
    assert.deepEqual(keys(item.images[0]), shapes.ItemImage, 'an image on an item');
    assert.deepEqual(keys(item.fields[0]), shapes.LabeledField, 'a labelled field');

    assert.deepEqual(keys(await backend.invoke('get_status')), shapes.Status, 'status');
    assert.deepEqual(keys(await backend.invoke('get_config')), shapes.Config, 'settings');
    assert.deepEqual(
        keys(await backend.invoke('validate_path', { kind: 'generic', path: '/x' })),
        shapes.ValidationResult, 'folder check');
});

test('every command the window calls is one the application permits', async () => {
    // Registering a command and permitting it are separate lists, and when
    // they disagree the build still succeeds -- the failure arrives at
    // runtime as "not allowed", with a button that quietly does nothing.
    const page = readFileSync(join(root, 'src', 'index.html'), 'utf8');
    const capabilities = JSON.parse(
        readFileSync(join(root, 'src-tauri', 'capabilities', 'default.json'), 'utf8'));

    const permitted = new Set(
        capabilities.permissions
            .filter(p => p.startsWith('allow-'))
            .map(p => p.slice('allow-'.length).replaceAll('-', '_')));

    const called = [...page.matchAll(/invoke\('([a-z_]+)'/g)].map(m => m[1]);
    assert.ok(called.length > 0, 'the window should be calling something');

    for (const command of new Set(called)) {
        assert.ok(permitted.has(command),
            `the window calls ${command}, which is not in the permission list -- it would ` +
            `fail at runtime with "not allowed"`);
    }
});

test('the window reads the fields it is given, not ones it invents', async () => {
    // Exercises the real page against the stand-in and checks that what
    // reaches the screen came from the agreed fields. A rename on either side
    // shows up here as an empty badge or a missing count.
    const backend = makeBackend({ total: 12, exactIdFor: '2010.1.2' });
    const { window } = await loadPage(backend);
    type(window, 'photo');
    await settle(window);

    const first = window.document.querySelector('.result');
    assert.match(first.querySelector('.accessno-badge').textContent, /2010\.1\./,
        'the catalog number should be read from accessno');
    assert.match(first.querySelector('.meta').textContent, /Photo/,
        'the kind of record should be read from catalog');
    assert.match(window.document.getElementById('count').textContent, /12 results/,
        'the count should be read from total');
    assert.equal(window.document.querySelectorAll('.result.exact-match').length, 1,
        'the exact match should be read from is_exact_id');
    assert.ok(first.querySelector('img.thumb'), 'a picture should be shown when has_image is set');

    click(window, '.result[data-id="1"]');
    await settle(window, 100);
    assert.ok(window.document.querySelector('#card h2').textContent,
        'the record title should be read from title');
    assert.ok(window.document.querySelector('.field .label'),
        'detail fields should be read from label and value');
});
