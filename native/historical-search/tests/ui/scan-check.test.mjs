// Checking a scan against the collection: a file is dropped on the window, or
// chosen, and the records that use an identical picture, or one that looks the
// same, come up as search results -- the same cards, opening the same way --
// under a strip that says what was found, each tagged with how it matched.
//
// The thing most worth protecting is how "nothing found" reads. This finds the
// same photograph scanned again, resized, turned, reframed or cropped, and a
// small piece of it that shows a face, but not a small piece with no face in
// it. If "nothing found" were worded as "this is new" a
// volunteer would add a duplicate -- so it must never be.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, click, type, resultIds } from './harness.mjs';

const text = (window, selector) => window.document.querySelector(selector).textContent;
const drop = (backend, paths) => backend.listeners['tauri://drag-drop']({ payload: { paths } });
const answer = (matches = [], over = {}) => ({
    list_files: 5000, same_size: 2, unreadable: 0, missing: 0,
    identical: matches.filter(m => m.kind === 'identical').length,
    is_collection_file: false, files_without_record: 0, compared: 5000, not_yet_learned: 0,
    look_alike_problem: null, matches, ...over,
});
const identical = { kind: 'identical', percent: 100 };
const likely = percent => ({ kind: 'likely', percent });
const possible = percent => ({ kind: 'possible', percent });
const closest = percent => ({ kind: 'closest', percent });

async function dropOnto(backend, paths = ['/scans/new scan.tif']) {
    const { window } = await loadPage(backend);
    drop(backend, paths);
    await settle(window, 100);
    return window;
}

test('dropping a scan shows the record that uses an identical file, as a search result', async () => {
    const backend = makeBackend({ total: 5, scanResult: answer([identical]) });
    const window = await dropOnto(backend);

    const asked = backend.calls.filter(c => c.cmd === 'photo_check_scan');
    assert.equal(asked.length, 1);
    assert.equal(asked[0].args.path, '/scans/new scan.tif');
    assert.match(text(window, '#scan-result .scan-verdict'), /already in the collection.*identical file/i);
    assert.deepEqual(resultIds(window), ['1'], 'the record is in the ordinary results list');
    assert.match(text(window, '#count'), /1 result/);
    assert.match(text(window, '#results'), /Record number 1/, 'drawn by the same card as any search result');
    assert.match(text(window, '.result .scan-tag'), /Identical file/);
});

test('a look-alike is listed with how alike it is, best first', async () => {
    const backend = makeBackend({ total: 5, scanResult: answer([likely(94), possible(71), closest(52)]) });
    const window = await dropOnto(backend);
    assert.match(text(window, '#scan-result .scan-verdict'), /Very probably already in the collection/);
    assert.deepEqual(resultIds(window), ['1', '2', '3'], 'in the order the application gave them');
    const tags = [...window.document.querySelectorAll('.result .scan-tag')].map(t => t.textContent);
    assert.deepEqual(tags, ['94% alike', 'Possible: 71% alike', 'Nearest: 52% alike']);
    const classes = [...window.document.querySelectorAll('.result .scan-tag')].map(t => t.className.replace('scan-tag ', ''));
    assert.deepEqual(classes, ['likely', 'possible', 'closest']);
});

test('only a possible match says possibly, and asks for a person to compare', async () => {
    const backend = makeBackend({ total: 5, scanResult: answer([possible(70)]) });
    const window = await dropOnto(backend);
    const verdict = text(window, '#scan-result .scan-verdict');
    assert.match(verdict, /Possibly already in the collection/);
    assert.match(verdict, /please compare/i);
    assert.doesNotMatch(verdict, /very probably|identical/i);
});

test('with only the nearest pictures, it says no close match and does not call the photograph new', async () => {
    const backend = makeBackend({ total: 5, scanResult: answer([closest(55), closest(51)]) });
    const window = await dropOnto(backend);
    const shown = text(window, '#scan-result');
    assert.match(shown, /No close match found/);
    assert.match(shown, /worn or cropped copy/);
    assert.match(shown, /does not mean the photograph is new/i);
    assert.doesNotMatch(shown, /this photograph is new|it is new|not in the collection|not here yet/i);
    assert.equal(resultIds(window).length, 2);
});

test('with nothing at all, it says so and says what the check can and cannot find', async () => {
    const backend = makeBackend({ total: 3, scanResult: answer([], { same_size: 3, compared: 16553 }) });
    const window = await dropOnto(backend);
    const shown = text(window, '#scan-result');
    assert.match(shown, /No match found in the collection/);
    assert.match(shown, /scanned again.*resized.*turned/i);
    assert.match(shown, /small piece that shows a face/i);
    assert.match(shown, /small piece with no face in it may not be found/i);
    assert.match(shown, /does not mean the photograph is new/i);
    assert.doesNotMatch(shown, /this photograph is new|it is new|not in the collection|not here yet/i);
    assert.match(shown, /16,553 pictures/);
    assert.match(shown, /3 files of the same size/);
    assert.equal(resultIds(window).length, 0);
});

test('a strong match does not carry the warning about crops', async () => {
    const backend = makeBackend({ total: 5, scanResult: answer([likely(97)]) });
    const window = await dropOnto(backend);
    assert.doesNotMatch(text(window, '#scan-result'), /may not be found/);
});

test('a copy found as a cropped one says so beside its record', async () => {
    const backend = makeBackend({ total: 5, scanResult: answer([{ ...likely(88), cropped: true }, likely(95)]) });
    const window = await dropOnto(backend);
    const tags = [...window.document.querySelectorAll('.result .scan-tag')].map(t => t.textContent);
    assert.deepEqual(tags, ['88% alike · cropped copy', '95% alike']);
});

test('the record opens exactly as it does from a typed search', async () => {
    const backend = makeBackend({ total: 5, scanResult: answer([identical]) });
    const window = await dropOnto(backend);
    click(window, '.result');
    await settle(window, 150);
    assert.ok(backend.calls.some(c => c.cmd === 'get_item' && c.args.id === 1), 'the record was opened by its id');
    assert.equal(window.document.getElementById('overlay').hidden, false);
});

test('the scan is shown back by name from what the application holds, never by path', async () => {
    const backend = makeBackend({ total: 5, scanResult: answer([identical]) });
    const window = await dropOnto(backend, ['C:\\Users\\someone\\Scans\\new scan.tif']);
    const strip = window.document.getElementById('scan-result');
    assert.match(strip.querySelector('figcaption').textContent, /^new scan\.tif$/);
    assert.match(decodeURIComponent(strip.querySelector('img').src), /dropped-\d+/);
    assert.doesNotMatch(strip.textContent, /Users|someone/, 'the folder it came from is not displayed');
});

test('an identical file that no record uses is said to be so', async () => {
    const backend = makeBackend({ total: 3, scanResult: answer([], { identical: 1, files_without_record: 1 }) });
    const window = await dropOnto(backend);
    assert.match(text(window, '#scan-result .scan-verdict'), /identical file is in the collection, but no record uses it/i);
});

test('matching pictures without a record are mentioned beside the records that were found', async () => {
    const backend = makeBackend({ total: 3, scanResult: answer([identical], { files_without_record: 2 }) });
    const window = await dropOnto(backend);
    assert.match(text(window, '#scan-result'), /2 matching pictures are not attached to any record/);
});

test('a file that is itself in the collection is told so, not called a duplicate', async () => {
    const backend = makeBackend({ total: 3, scanResult: answer([identical], { is_collection_file: true }) });
    const window = await dropOnto(backend, ['/collection/first.tif']);
    assert.match(text(window, '#scan-result'), /one of the collection.s own files/i);
});

test('pictures not yet learned, and files that could not be read or have gone, are reported, not hidden', async () => {
    const backend = makeBackend({ total: 3, scanResult: answer([], { same_size: 5, unreadable: 2, missing: 1, not_yet_learned: 40 }) });
    const window = await dropOnto(backend);
    const shown = text(window, '#scan-result');
    assert.match(shown, /40 pictures have not been learned yet, so they were not compared/);
    assert.match(shown, /2 files of the same size could not be read/);
    assert.match(shown, /1 are no longer where the list says/);
    assert.match(shown, /Refresh the search index/);
    assert.doesNotMatch(shown, /Update photo list/, 'there is no such button any more');
});

test('when the look-alike comparison could not be made, the identical-file result still stands', async () => {
    const backend = makeBackend({ total: 3, scanResult: answer([identical], { look_alike_problem: 'That file could not be read as a picture.', compared: 0 }) });
    const window = await dropOnto(backend);
    assert.match(text(window, '#scan-result .scan-verdict'), /identical file belongs to the record below/);
    assert.match(text(window, '#scan-result'), /comparison by how pictures look could not be made.*Only identical files were checked/);
    assert.equal(resultIds(window).length, 1);
});

test('a problem, such as the list not being made yet, is shown in words', async () => {
    const backend = makeBackend({ total: 3 });
    backend.scanError = 'There are no photographs to check against yet. Add the photo folders under Folder Settings, then refresh the search index.';
    const window = await dropOnto(backend);
    assert.match(text(window, '#scan-result .scan-verdict'), /could not be checked/);
    assert.match(text(window, '#scan-result'), /Folder Settings/);
});

test('a dropped file name that looks like markup is shown as text', async () => {
    const backend = makeBackend({ total: 3, scanResult: answer([identical]) });
    const window = await dropOnto(backend, ['/scans/<img src=x onerror=alert(1)>.tif']);
    const strip = window.document.getElementById('scan-result');
    assert.equal(strip.querySelector('figcaption img'), null, 'a name cannot add an element');
    assert.match(strip.querySelector('figcaption').textContent, /<img src=x onerror=alert\(1\)>\.tif/);
    assert.equal((window.__alerts || []).length, 0);
});

test('dropping several files checks only the first and says so', async () => {
    const backend = makeBackend({ total: 3 });
    const window = await dropOnto(backend, ['/scans/a.tif', '/scans/b.tif', '/scans/c.tif']);
    const asked = backend.calls.filter(c => c.cmd === 'photo_check_scan');
    assert.equal(asked.length, 1);
    assert.equal(asked[0].args.path, '/scans/a.tif');
    assert.match(text(window, '#scan-result'), /3 files were dropped; only the first was checked/);
});

test('a drop with nothing in it does nothing', async () => {
    const backend = makeBackend({ total: 3 });
    const { window } = await loadPage(backend);
    drop(backend, []);
    await settle(window, 50);
    assert.equal(backend.calls.filter(c => c.cmd === 'photo_check_scan').length, 0);
    assert.equal(window.document.getElementById('scan-result').hidden, true);
});

test('choosing a file checks it, and cancelling the chooser does nothing', async () => {
    const backend = makeBackend({ total: 3, pickedScan: '/scans/chosen.tif' });
    const { window } = await loadPage(backend);
    click(window, '#scan-pick-btn');
    await settle(window, 100);
    assert.equal(backend.calls.filter(c => c.cmd === 'photo_check_scan').at(-1).args.path, '/scans/chosen.tif');

    const cancelled = makeBackend({ total: 3, pickedScan: null });
    const second = await loadPage(cancelled);
    click(second.window, '#scan-pick-btn');
    await settle(second.window, 100);
    assert.equal(cancelled.calls.filter(c => c.cmd === 'photo_check_scan').length, 0);
});

test('the drop box lights up while something is held over the window', async () => {
    const backend = makeBackend({ total: 3 });
    const { window } = await loadPage(backend);
    const box = window.document.getElementById('scan-drop');
    backend.listeners['tauri://drag-enter']({});
    assert.ok(box.classList.contains('over'));
    backend.listeners['tauri://drag-leave']({});
    assert.ok(!box.classList.contains('over'));
    backend.listeners['tauri://drag-enter']({});
    drop(backend, ['/scans/a.tif']);
    assert.ok(!box.classList.contains('over'), 'and goes back when the file is let go');
});

test('an answer that arrives after a newer drop is not shown', async () => {
    const backend = makeBackend({ total: 5 });
    backend.holdScan = true;
    const { window } = await loadPage(backend);
    drop(backend, ['/scans/first.tif']);
    await settle(window, 30);
    backend.holdScan = false;
    backend.scanResult = answer([identical, likely(90)]);
    drop(backend, ['/scans/second.tif']);
    await settle(window, 60);
    backend.pending.forEach(release => release()); // the first answer limps in late
    await settle(window, 60);
    assert.match(text(window, '#scan-result'), /second\.tif/);
    assert.doesNotMatch(text(window, '#scan-result'), /first\.tif/);
    assert.equal(resultIds(window).length, 2, 'the newer answer is the one listed');
});

test('typing a search afterwards replaces the scan result, as any new search would', async () => {
    const backend = makeBackend({ total: 5, scanResult: answer([identical]) });
    const window = await dropOnto(backend);
    assert.equal(window.document.getElementById('scan-result').hidden, false);
    type(window, 'record');
    await settle(window, 300);
    assert.equal(window.document.getElementById('scan-result').hidden, true, 'the strip goes away');
    assert.ok(resultIds(window).length > 1, 'and the search results are shown');
    assert.equal(window.document.querySelectorAll('.scan-tag').length, 0, 'with no scan tags left on them');
});

test('a search still on its way does not land on top of the scan result', async () => {
    const backend = makeBackend({ total: 5, scanResult: answer([identical]) });
    backend.holdSearches = true;
    const { window } = await loadPage(backend);
    type(window, 'record');
    await settle(window, 200);
    drop(backend, ['/scans/a.tif']);
    await settle(window, 100);
    backend.pending.forEach(release => release());
    await settle(window, 100);
    assert.deepEqual(resultIds(window), ['1'], 'the scan result stands');
});

test('the search box is cleared when a scan is checked', async () => {
    const backend = makeBackend({ total: 5, scanResult: answer([identical]) });
    const { window } = await loadPage(backend);
    type(window, 'something typed');
    await settle(window, 300);
    drop(backend, ['/scans/a.tif']);
    await settle(window, 100);
    assert.equal(window.document.getElementById('q').value, '');
});

test('the strip can be closed', async () => {
    const backend = makeBackend({ total: 3, scanResult: answer([identical]) });
    const window = await dropOnto(backend);
    click(window, '#scan-result .scan-close');
    assert.equal(window.document.getElementById('scan-result').hidden, true);
});

test('"Show more matches" checks again asking for more, and the button goes away', async () => {
    const backend = makeBackend({ total: 5, scanResult: answer([closest(52), closest(50), closest(48)]) });
    const window = await dropOnto(backend);
    const button = window.document.querySelector('#scan-more-btn');
    assert.ok(button, 'offered after a check');
    button.click();
    await settle(window, 100);
    const asked = backend.calls.filter(c => c.cmd === 'photo_check_scan');
    assert.equal(asked.length, 2);
    assert.equal(asked[0].args.more, false);
    assert.equal(asked[1].args.more, true);
    assert.equal(asked[1].args.path, '/scans/new scan.tif');
    assert.equal(window.document.querySelector('#scan-more-btn'), null, 'offered only once');
});
