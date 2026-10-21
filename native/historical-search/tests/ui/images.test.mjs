// The pictures on a detail card.
//
// A record can carry several images -- the front and back of an object, the
// pages of a letter -- each with its own caption, and only some have a
// high-resolution original behind them. This part was written against a
// platform that could not show its failure modes: on this machine every image
// has a preview already, so the paths taken when one is missing or will not
// load had never actually run.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, type, click } from './harness.mjs';

const image = (id, over = {}) => ({
    id, kind: 'imgthumb', caption: '', has_thumb: true, has_master: false, ...over,
});

async function openRecord(imagesFor) {
    const backend = makeBackend({ total: 3, imagesFor });
    const dom = await loadPage(backend);
    type(dom.window, 'photo');
    await settle(dom.window);
    click(dom.window, '.result[data-id="1"]');
    await settle(dom.window, 100);
    return { backend, window: dom.window };
}

const mainImage = w => w.document.getElementById('main-image');
const fallback = w => w.document.getElementById('main-image-fallback');
const strip = w => [...w.document.querySelectorAll('.strip-thumb')];
const downloadLabel = w => w.document.getElementById('download-btn')?.textContent ?? null;

test('one picture shows on its own, with no strip to choose from', async () => {
    const { window } = await openRecord(() => [image(11)]);

    assert.equal(mainImage(window).hidden, false, 'the picture should be showing');
    assert.ok(mainImage(window).getAttribute('src'), 'and have a source');
    assert.equal(fallback(window).hidden, true, 'no failure message');
    assert.equal(strip(window).length, 0, 'a single picture needs no strip');
    assert.equal(window.document.getElementById('image-position'), null, 'nor a counter');
});

test('several pictures give a strip, and choosing one swaps the picture and caption', async () => {
    const { window } = await openRecord(() => [
        image(11, { caption: 'Front of the monument' }),
        image(12, { caption: 'Back of the monument' }),
        image(13, { caption: '' }),
    ]);

    assert.equal(strip(window).length, 3, 'one thumbnail per picture');
    assert.equal(window.document.getElementById('image-caption').textContent, 'Front of the monument');
    assert.equal(window.document.getElementById('image-position').textContent, 'Image 1 of 3');
    assert.ok(strip(window)[0].classList.contains('selected'), 'the first starts selected');

    const before = mainImage(window).getAttribute('src');
    strip(window)[1].dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
    await settle(window, 40);

    assert.notEqual(mainImage(window).getAttribute('src'), before, 'the picture should change');
    assert.equal(window.document.getElementById('image-caption').textContent, 'Back of the monument');
    assert.equal(window.document.getElementById('image-position').textContent, 'Image 2 of 3');
    assert.ok(strip(window)[1].classList.contains('selected'));
    assert.ok(!strip(window)[0].classList.contains('selected'), 'only one at a time');

    // A picture with no caption of its own must not keep the previous one.
    strip(window)[2].dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
    await settle(window, 40);
    assert.equal(window.document.getElementById('image-caption').textContent, '',
        'the caption should clear rather than carry over from the previous picture');
});

test('a picture with no preview says so instead of showing a broken image', async () => {
    // The indexer could not turn this file into a preview. The results list
    // already showed a placeholder in this case; the card used to ask for an
    // image that was never going to arrive.
    const { window } = await openRecord(() => [image(11, { has_thumb: false })]);

    assert.equal(mainImage(window).hidden, true, 'no broken picture should be shown');
    assert.equal(mainImage(window).getAttribute('src'), null, 'and nothing should be requested');
    assert.equal(fallback(window).hidden, false, 'the reason should be on screen');
    assert.match(fallback(window).textContent, /No preview could be made/);
    assert.ok(downloadLabel(window), 'the original can still be downloaded');
});

test('a picture that fails to load is explained, and reported for the log', async () => {
    // What a machine with an empty preview cache, or a webview that will not
    // fetch through the custom scheme, actually produces.
    const { backend, window } = await openRecord(() => [image(11), image(12)]);
    assert.equal(fallback(window).hidden, true);

    mainImage(window).dispatchEvent(new window.Event('error'));
    await settle(window, 60);

    assert.equal(mainImage(window).hidden, true);
    assert.equal(fallback(window).hidden, false);
    assert.match(fallback(window).textContent, /could not be displayed/);
    const logged = backend.calls.filter(c => c.cmd === 'log_from_ui');
    assert.ok(logged.length > 0, 'the failure should reach the diagnostics log');
    assert.match(logged[logged.length - 1].args.message, /failed to load/);
});

test('choosing a working picture after a failed one shows it again', async () => {
    const { window } = await openRecord(() => [image(11), image(12)]);
    mainImage(window).dispatchEvent(new window.Event('error'));
    await settle(window, 60);
    assert.equal(fallback(window).hidden, false);

    strip(window)[1].dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
    await settle(window, 60);

    assert.equal(mainImage(window).hidden, false, 'the next picture should be shown');
    assert.equal(fallback(window).hidden, true, 'and the failure message cleared');
});

test('the download offer says which of the two you will actually get', async () => {
    // Only some images have a high-resolution original behind them; for the
    // rest the Images folder holds a downsized copy and there is nothing
    // larger, so promising "full resolution" would be untrue.
    const { window } = await openRecord(() => [
        image(11, { has_master: true }),
        image(12, { has_master: false }),
    ]);
    assert.match(downloadLabel(window), /full-resolution original/);

    strip(window)[1].dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
    await settle(window, 40);
    assert.match(downloadLabel(window), /Download image/);
    assert.doesNotMatch(downloadLabel(window), /full-resolution/,
        'a picture with no original must not be offered as one');
});

test('downloading asks for the picture being looked at, numbered so they do not overwrite', async () => {
    const { backend, window } = await openRecord(() => [image(11), image(12), image(13)]);

    strip(window)[2].dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
    await settle(window, 40);
    click(window, '#download-btn');
    await settle(window, 80);

    const asked = backend.calls.filter(c => c.cmd === 'download_image').pop();
    assert.ok(asked, 'a download should have been requested');
    assert.equal(asked.args.id, 13, 'it should be the picture on screen, not the first one');
    assert.equal(asked.args.kind, 'imgthumb');
    assert.match(asked.args.suggestedName, /-3$/,
        'each picture of a record needs its own name or they overwrite each other');
});

test('a record with no pictures at all shows none of the picture machinery', async () => {
    const { window } = await openRecord(() => []);

    assert.equal(mainImage(window), null);
    assert.equal(fallback(window), null);
    assert.equal(window.document.getElementById('download-btn'), null,
        'nothing to download');
    assert.ok(window.document.querySelector('#card h2'), 'but the record itself still opens');
});

// Viewing a photograph larger, and handing it to the computer's own viewer.
// The viewer itself is exercised in lightbox.test.mjs; the page's modules do
// not load in this headless DOM, so what is checked here is the card.

async function openPhoto(imagesFor, options = {}) {
    const backend = makeBackend({ total: 3, imagesFor, ...options });
    const dom = await loadPage(backend);
    type(dom.window, 'photo');
    await settle(dom.window);
    click(dom.window, '.result[data-id="1"]');
    await settle(dom.window, 100);
    return { backend, window: dom.window };
}

test('a photograph can be viewed larger or opened in the computer\'s own viewer', async () => {
    const { window } = await openPhoto(() => [image(11)]);
    const doc = window.document;
    assert.match(doc.getElementById('enlarge-btn').textContent, /View larger/);
    assert.match(doc.getElementById('open-image-btn').textContent, /photo viewer/);
    assert.equal(doc.getElementById('enlarge-btn').disabled, false);
    assert.ok(mainImage(window).classList.contains('zoomable'), 'the picture itself invites a click');
});

test('opening in the photo viewer asks the application by image id, not a path', async () => {
    const { backend, window } = await openPhoto(() => [image(11), image(12)]);
    strip(window)[1].dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
    await settle(window, 40);
    click(window, '#open-image-btn');
    await settle(window, 50);

    const call = backend.calls.find(c => c.cmd === 'open_image');
    assert.deepEqual(JSON.parse(JSON.stringify(call.args)), { kind: 'imgthumb', id: 12 }, 'the picture chosen, by id');
});

test('an image that cannot be opened is explained beside the button', async () => {
    const { window } = await openPhoto(() => [image(11)], {
        openImageError: 'the image is no longer where the index recorded it',
    });
    click(window, '#open-image-btn');
    await settle(window, 50);
    const msg = window.document.getElementById('open-image-msg');
    assert.equal(msg.hidden, false);
    assert.match(msg.textContent, /no longer where the index recorded it/);
});

test('a picture with no preview cannot be enlarged, but can still be opened elsewhere', async () => {
    const { window } = await openPhoto(() => [image(11, { has_thumb: false })]);
    const doc = window.document;
    assert.equal(doc.getElementById('enlarge-btn').disabled, true);
    assert.equal(mainImage(window).classList.contains('zoomable'), false);
    assert.equal(doc.getElementById('open-image-btn').disabled, false);
});

test('choosing another picture updates whether it can be enlarged', async () => {
    const { window } = await openPhoto(() => [image(11), image(12, { has_thumb: false })]);
    assert.equal(window.document.getElementById('enlarge-btn').disabled, false);
    strip(window)[1].dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
    await settle(window, 40);
    assert.equal(window.document.getElementById('enlarge-btn').disabled, true);
});

test('if the larger view cannot be loaded the card says so rather than doing nothing', async () => {
    const { window } = await openPhoto(() => [image(11)]);
    click(window, '#enlarge-btn');
    await settle(window, 80);
    const msg = window.document.getElementById('open-image-msg');
    assert.equal(msg.hidden, false);
    assert.match(msg.textContent, /Open in your photo viewer/);
});
