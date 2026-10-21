// The enlarged photograph viewer.
//
// Its arithmetic is plain functions, checked directly; the viewer itself is
// built into a headless DOM with a stand-in for the picture it would load.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
import { fitScale, clampPan, zoomAbout, openLightbox } from '../../src/lightbox.mjs';

test('a big picture is fitted to the stage, a small one is not stretched', () => {
    assert.equal(fitScale(1000, 500, 4000, 2000), 0.25);
    assert.equal(fitScale(1000, 500, 4000, 1000), 0.25, 'the tighter side decides');
    assert.equal(fitScale(1000, 500, 200, 100), 1, 'never above its own size');
    assert.equal(fitScale(0, 0, 200, 100), 1, 'an unmeasured stage does not divide by zero');
});

test('a picture is centred when smaller than the stage and cannot be dragged out of view when larger', () => {
    assert.deepEqual(clampPan(0, 0, 0.5, 1000, 600, 1000, 800), { tx: 250, ty: 100 });
    assert.deepEqual(clampPan(500, 500, 2, 1000, 600, 1000, 800), { tx: 0, ty: 0 }, 'cannot show space before the top left');
    assert.deepEqual(clampPan(-9999, -9999, 2, 1000, 600, 1000, 800), { tx: -1000, ty: -1000 }, 'nor past the bottom right');
});

test('zooming keeps the spot under the pointer under the pointer', () => {
    const view = { scale: 1, tx: -200, ty: -100 };
    const next = zoomAbout(view, 2, 300, 200, 0.25, 1000, 600, 4000, 3000);
    // The picture point under (300,200) was ((300+200)/1, (200+100)/1) = (500, 300).
    assert.equal(next.scale, 2);
    assert.equal(next.tx + 500 * next.scale, 300);
    assert.equal(next.ty + 300 * next.scale, 200);
});

test('zoom is limited at both ends', () => {
    const view = { scale: 1, tx: 0, ty: 0 };
    assert.equal(zoomAbout(view, 100, 0, 0, 0.25, 1000, 600, 4000, 3000).scale, 8);
    assert.equal(zoomAbout(view, 0.0001, 0, 0, 0.25, 1000, 600, 4000, 3000).scale, 0.25, 'no smaller than fitting');
});

// ---------------------------------------------------------------- the viewer

function build(over = {}) {
    const dom = new JSDOM('<!doctype html><body><button id="opener">open</button></body>', { pretendToBeVisual: true });
    const win = dom.window;
    // jsdom lays nothing out: give the stage a size, and let pictures "load".
    Object.defineProperty(win.HTMLElement.prototype, 'clientWidth', {
        get() { return this.classList.contains('lb-stage') ? 1000 : 0; },
    });
    Object.defineProperty(win.HTMLElement.prototype, 'clientHeight', {
        get() { return this.classList.contains('lb-stage') ? 500 : 0; },
    });
    win.Image = class {
        set src(url) {
            this._src = url;
            win.setTimeout(() => {
                this.naturalWidth = 4000; this.naturalHeight = 2000;
                if (url.includes('bad')) this.onerror(); else this.onload();
            }, 0);
        }
    };
    const calls = { loads: [], external: [], index: [], closed: 0, escapedToPage: 0 };
    win.document.addEventListener('keydown', (e) => { if (e.key === 'Escape') calls.escapedToPage += 1; });
    win.document.getElementById('opener').focus();
    const box = openLightbox({
        title: 'Main Street, 1920', count: 2, index: 0, doc: win.document,
        load: async (i) => { calls.loads.push(i); if (over.fail) throw over.fail; return { url: `hsimg://x/large-${i}${over.bad ? '-bad' : ''}`, caption: `caption ${i}` }; },
        openExternal: (i) => { calls.external.push(i); if (over.externalFails) return Promise.reject(new Error(over.externalFails)); },
        onIndex: (i) => calls.index.push(i),
        onClose: () => { calls.closed += 1; },
    });
    const $ = sel => win.document.querySelector(sel);
    const wait = (ms = 20) => new Promise(r => win.setTimeout(r, ms));
    const key = (k) => win.document.dispatchEvent(new win.KeyboardEvent('keydown', { key: k, bubbles: true, cancelable: true }));
    const press = (act) => $(`[data-act="${act}"]`).dispatchEvent(new win.MouseEvent('click', { bubbles: true }));
    return { win, box, calls, $, wait, key, press };
}

test('it opens on the picture, fitted, and says what the zoom is', async () => {
    const t = build();
    await t.wait();
    assert.equal(t.$('.lb-img').hidden, false);
    assert.equal(t.$('.lb-msg').hidden, true);
    assert.equal(t.$('.lb-zoom').textContent, 'Fit (25%)', '4000 across a 1000 stage');
    assert.equal(t.$('.lb-caption').textContent, 'caption 0');
    assert.equal(t.$('.lb-title').textContent, 'Main Street, 1920');
    assert.equal(t.$('.lb').getAttribute('aria-modal'), 'true');
});

test('the zoom buttons and 100% change the size, and Fit returns', async () => {
    const t = build();
    await t.wait();
    t.press('actual');
    assert.equal(t.$('.lb-zoom').textContent, '100%');
    assert.match(t.$('.lb-img').style.transform, /scale\(1\)/);
    t.press('out');
    assert.equal(t.$('.lb-zoom').textContent, '71%');
    t.press('in');
    t.press('in');
    assert.equal(t.$('.lb-zoom').textContent, '140%');
    t.press('fit');
    assert.equal(t.$('.lb-zoom').textContent, 'Fit (25%)');
});

test('keys zoom, and Escape closes only the viewer', async () => {
    const t = build();
    await t.wait();
    t.key('1');
    assert.equal(t.$('.lb-zoom').textContent, '100%');
    t.key('0');
    assert.equal(t.$('.lb-zoom').textContent, 'Fit (25%)');
    t.key('+');
    assert.equal(t.$('.lb-zoom').textContent, '35%');
    t.key('Escape');
    assert.equal(t.win.document.querySelector('.lb'), null, 'removed');
    assert.equal(t.calls.closed, 1);
    assert.equal(t.calls.escapedToPage, 0, 'the card underneath is not also closed');
    assert.equal(t.win.document.activeElement.id, 'opener', 'focus goes back to where it was');
});

test('the wheel zooms about the pointer', async () => {
    const t = build();
    await t.wait();
    t.press('actual');
    const before = t.$('.lb-img').style.transform;
    const wheel = new t.win.WheelEvent('wheel', { deltaY: -200, clientX: 100, clientY: 100, bubbles: true, cancelable: true });
    t.$('.lb-stage').dispatchEvent(wheel);
    assert.equal(wheel.defaultPrevented, true, 'the page does not scroll behind it');
    assert.notEqual(t.$('.lb-img').style.transform, before);
    assert.notEqual(t.$('.lb-zoom').textContent, '100%');
});

test('arrow keys move between the record\'s pictures when fitted, and pan when zoomed in', async () => {
    const t = build();
    await t.wait();
    t.key('ArrowRight');
    await t.wait();
    assert.deepEqual(t.calls.loads, [0, 1]);
    assert.deepEqual(t.calls.index, [0, 1], 'the card is told which picture is showing');
    assert.equal(t.$('.lb-count').textContent, '2 of 2');
    assert.equal(t.$('[data-act="next"]').disabled, true, 'nothing after the last');
    t.key('ArrowRight');
    await t.wait();
    assert.deepEqual(t.calls.loads, [0, 1], 'and no going past it');

    t.press('actual');
    const centred = t.$('.lb-img').style.transform;
    t.key('ArrowLeft');
    assert.deepEqual(t.calls.loads, [0, 1], 'zoomed in, the arrow pans and does not change picture');
    const panned = t.$('.lb-img').style.transform;
    assert.notEqual(panned, centred, 'it moved along the picture');
    for (let i = 0; i < 40; i++) t.key('ArrowLeft');
    assert.match(t.$('.lb-img').style.transform, /translate\(0px, /, 'and stops at the picture\'s left edge');
});

test('opening in the photo viewer is for the picture being shown', async () => {
    const t = build();
    await t.wait();
    t.press('next');
    await t.wait();
    t.press('external');
    assert.deepEqual(t.calls.external, [1]);
});

test('when the larger copy cannot be made, it says why and still offers the photo viewer', async () => {
    const t = build({ fail: 'This image could not be enlarged.' });
    await t.wait();
    assert.equal(t.$('.lb-img').hidden, true);
    assert.match(t.$('.lb-msg').textContent, /could not be enlarged/);
    assert.match(t.$('.lb-msg').textContent, /Open in your photo viewer/);
    t.press('external');
    assert.deepEqual(t.calls.external, [0], 'the original can still be opened');
});

test('a picture that will not display is explained rather than left blank', async () => {
    const t = build({ bad: true });
    await t.wait();
    assert.match(t.$('.lb-msg').textContent, /could not be shown larger/);
});

test('a slow picture that arrives after the person moved on is not shown', async () => {
    const t = build();
    await t.wait();
    t.press('next');
    t.press('prev');
    await t.wait(40);
    assert.equal(t.$('.lb-count').textContent, '1 of 2');
    assert.equal(t.$('.lb-caption').textContent, 'caption 0');
});

test('a single picture has no previous and next buttons', async () => {
    const dom = new JSDOM('<!doctype html><body></body>');
    openLightbox({ title: 'x', count: 1, index: 0, doc: dom.window.document, load: async () => ({ url: 'u' }) });
    assert.equal(dom.window.document.querySelector('.lb-pager').hidden, true);
});

test('if the picture cannot be handed to the photo viewer, the viewer says so', async () => {
    const t = build({ externalFails: 'the image is no longer where the index recorded it' });
    await t.wait();
    assert.equal(t.$('.lb-note').hidden, true, 'nothing to say until something goes wrong');
    t.press('external');
    await t.wait();
    assert.equal(t.$('.lb-note').hidden, false);
    assert.match(t.$('.lb-note').textContent, /no longer where the index recorded it/);
    assert.equal(t.$('.lb-img').hidden, false, 'and the picture stays up');
});
