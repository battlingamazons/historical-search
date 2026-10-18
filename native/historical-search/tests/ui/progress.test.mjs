// What the window shows while the index is being built.
//
// The first build reads every photograph in the collection -- tens of
// gigabytes -- and on the society's own computer that took over two hours. It
// was force-quit for looking hung, which is the failure this is about: the
// work was fine, the reporting of it was not. So what is checked here is that
// there is a count, a percentage, and an estimate of the time left, and that
// the estimate does not say something untrue when it cannot know yet.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBackend, loadPage, settle, click, type } from './harness.mjs';

/// A page with a clock the test controls, since the estimate is made from
/// elapsed time and a test cannot wait ten real seconds for each case.
async function pageWithFakeClock() {
    const backend = makeBackend({ total: 100 });
    const { window } = await loadPage(backend);
    let now = 1_000_000;
    window.Date.now = () => now;
    return { window, tick: seconds => { now += seconds * 1000; } };
}

const progress = (window, phase, current, total) =>
    window.formatProgress({ phase, phase_current: current, phase_total: total });

test('progress shows how far along it is, in numbers and not only a bar', async () => {
    const { window } = await pageWithFakeClock();
    const shown = progress(window, 'Preparing pictures in LHS Photos', 4200, 8000);
    assert.match(shown, /4,200/, 'the count should be readable, with separators');
    assert.match(shown, /8,000/, 'and what it is counting towards');
    assert.match(shown, /53%/, 'and a percentage');
    assert.match(shown, /Preparing pictures in LHS Photos/, 'and what it is doing');
});

test('no estimate is offered until there is enough to base one on', async () => {
    const { window, tick } = await pageWithFakeClock();
    // First sighting: nothing is known about the rate yet.
    assert.doesNotMatch(progress(window, 'Preparing pictures', 0, 20000), /left|nearly/,
        'an estimate from a single sample would be invented, not measured');

    // A moment later, barely any movement: still not enough.
    tick(3);
    assert.doesNotMatch(progress(window, 'Preparing pictures', 5, 20000), /left|nearly/,
        'three seconds and five images is not a measurement');
});

test('once the rate is known the estimate is in plain words', async () => {
    const { window, tick } = await pageWithFakeClock();
    progress(window, 'Preparing pictures', 0, 20000);

    // 1,000 images in 100 seconds -- 10 a second, so 19,000 left is about
    // 32 minutes.
    tick(100);
    const shown = progress(window, 'Preparing pictures', 1000, 20000);
    assert.match(shown, /about 32 minutes left/, `got ${JSON.stringify(shown)}`);

    // Near the end it should stop quoting minutes at someone who can watch it
    // finish.
    tick(1);
    assert.match(progress(window, 'Preparing pictures', 19990, 20000), /nearly there/);
});

test('a long job is described in hours rather than a hundred minutes', async () => {
    const { window, tick } = await pageWithFakeClock();
    progress(window, 'Preparing pictures', 0, 20000);
    // 100 images in 100 seconds: one a second, so 19,900 left is 5.5 hours.
    tick(100);
    const shown = progress(window, 'Preparing pictures', 100, 20000);
    assert.match(shown, /about 6 hours left/, `got ${JSON.stringify(shown)}`);
});

test('moving to the next phase measures that phase, not the one before', async () => {
    const { window, tick } = await pageWithFakeClock();
    progress(window, 'Preparing pictures for Photo records', 0, 4000);
    tick(100);
    progress(window, 'Preparing pictures for Photo records', 2000, 4000);

    // A new phase starts from nothing: the previous phase's rate says nothing
    // about this one, which reads different files of a different size.
    const first = progress(window, 'Preparing pictures in LHS Photos', 0, 8000);
    assert.doesNotMatch(first, /left|nearly/, 'a new phase has no measurement yet');

    tick(50);
    const shown = progress(window, 'Preparing pictures in LHS Photos', 500, 8000);
    // 500 in 50s = 10/s, 7500 left = 750s = about 13 minutes. If it had kept
    // the previous phase's starting point it would say something far shorter.
    assert.match(shown, /about 13 minutes left/, `got ${JSON.stringify(shown)}`);
});

test('a rebuild started over does not leave the estimate stuck', async () => {
    // The phase name is the same for a second rebuild of the same folder, so
    // the count going backwards is the only sign that it began again. Measured
    // from the old starting point, the difference stays negative and no
    // estimate would ever appear again for the rest of the run.
    const { window, tick } = await pageWithFakeClock();
    progress(window, 'Preparing pictures in LHS Photos', 0, 8000);
    tick(100);
    progress(window, 'Preparing pictures in LHS Photos', 4000, 8000);

    // Started again from the beginning.
    progress(window, 'Preparing pictures in LHS Photos', 0, 8000);
    tick(100);
    const shown = progress(window, 'Preparing pictures in LHS Photos', 1000, 8000);
    assert.match(shown, /about 12 minutes left/,
        `the estimate should recover after a restart, got ${JSON.stringify(shown)}`);
});

test('saving folders during a rebuild says the rebuild will be done again', async () => {
    // The application answers `false` when a rebuild is already running, and
    // the window used to throw that answer away: it then followed the rebuild
    // already under way and announced success, for an index built from the
    // folders the user had just replaced.
    const backend = makeBackend({ total: 100 });
    let status = {
        last_updated: null, record_count: 0, rebuilds_completed: 0, in_progress: true,
        last_error: null, warnings: [], phase: 'Preparing pictures in LHS Photos',
        phase_current: 40, phase_total: 8000,
    };
    const original = backend.invoke;
    backend.invoke = async (cmd, args) => {
        if (cmd === 'get_status') return { ...status };
        if (cmd === 'start_refresh') return false;   // one is already running
        return original(cmd, args);
    };

    const { window } = await loadPage(backend);
    await settle(window, 150);
    click(window, '#refresh-btn');
    await settle(window, 150);

    const line = window.document.getElementById('last-updated').textContent;
    assert.match(line, /already running/i,
        `the window should say the request was noted, got ${JSON.stringify(line)}`);
    assert.match(line, /start again|finishes/i,
        'and that the rebuild will be done again with the saved folders');

    // And it must still be there after the next poll: the wait is hours long,
    // and a message written once is overwritten 700ms later.
    await settle(window, 900);
    assert.match(window.document.getElementById('last-updated').textContent, /already running/i,
        'the notice should last as long as the wait does');

    // Once the application starts the owed rebuild, it speaks for itself.
    status = { ...status, phase: 'Starting again, with the folders you just saved…',
               phase_current: null, phase_total: null };
    await settle(window, 900);
    const during = window.document.getElementById('last-updated').textContent;
    assert.match(during, /Starting again/);
    assert.doesNotMatch(during, /already running/i,
        'the notice should go once the owed rebuild has begun');

    status = { ...status, in_progress: false, last_updated: 1757000000, record_count: 1234,
               phase: null, phase_current: null, phase_total: null };
    await settle(window, 1000);
    window.close();
});

test('an open record is closed when one rebuild hands over to the next', async () => {
    // The card is closed because record numbering changes, and that happens
    // when a rebuild finishes -- not when the window stops seeing one running.
    // Those came apart as soon as a rebuild could hand straight over to
    // another: the first one replaced the index, the window never saw a moment
    // without a rebuild in progress, and the card stayed open holding ids that
    // now pointed at different records.
    const backend = makeBackend({ total: 60 });
    let status = {
        last_updated: 1757000000, record_count: 60, rebuilds_completed: 0, in_progress: true,
        last_error: null, warnings: [], phase: 'Preparing pictures in LHS Photos',
        phase_current: 10, phase_total: 900,
    };
    const original = backend.invoke;
    backend.invoke = async (cmd, args) =>
        cmd === 'get_status' ? { ...status } : original(cmd, args);

    const { window } = await loadPage(backend);
    await settle(window, 150);

    // Someone searches and opens a record while waiting for the rebuild.
    type(window, 'photo');
    await settle(window);
    click(window, '.result[data-id="2"]');
    await settle(window, 120);
    assert.equal(window.document.getElementById('overlay').hidden, false,
        'the record should be open to begin with');

    // The first rebuild finishes and the second begins in the same breath:
    // still in progress, but the index underneath has been replaced.
    status = { ...status, rebuilds_completed: 1,
               phase: 'Starting again, with the folders you just saved…',
               phase_current: null, phase_total: null };
    await settle(window, 900);

    assert.equal(window.document.getElementById('overlay').hidden, true,
        'the record must be closed: its numbering belongs to the index that was just replaced');
    window.close();
});

test('a phase with no count of its own just says what it is doing', async () => {
    const { window } = await pageWithFakeClock();
    assert.equal(progress(window, 'Reading image links', null, null), 'Reading image links');
    assert.equal(window.formatProgress({ phase: null }), 'Working…');
});

test('the window shows the progress it is given while a rebuild runs', async () => {
    // The same thing end to end: the status line, not the function alone.
    const backend = makeBackend({ total: 100 });
    let status = {
        last_updated: null, record_count: 0, rebuilds_completed: 0, in_progress: true,
        last_error: null, warnings: [], phase: 'Preparing pictures in LHS Photos',
        phase_current: 1200, phase_total: 8000,
    };
    const original = backend.invoke;
    backend.invoke = async (cmd, args) =>
        cmd === 'get_status' ? { ...status } : original(cmd, args);

    const { window } = await loadPage(backend);
    await settle(window, 150);
    const line = window.document.getElementById('last-updated').textContent;
    assert.match(line, /1,200 \/ 8,000/, `the status line should carry the count, got ${line}`);
    assert.match(line, /15%/);
    assert.match(line, /Preparing pictures in LHS Photos/);

    // A rebuild outlives the page it was started from, so the window can open
    // onto one already running. It used to report "not built yet" and offer a
    // refresh button that the application would then refuse -- no progress,
    // no explanation, nothing that worked.
    const button = window.document.getElementById('refresh-btn');
    assert.equal(button.disabled, true, 'the button should not invite a second rebuild');
    assert.match(button.textContent, /Refreshing/);

    // And it stops asking once the rebuild is over, rather than polling for
    // the rest of the session. (Left running, this test itself never ends,
    // which is how the check came to be here.)
    status = { ...status, in_progress: false, last_updated: 1757000000, record_count: 1234,
               phase: null, phase_current: null, phase_total: null };
    await settle(window, 1000);
    assert.match(window.document.getElementById('last-updated').textContent, /1,234 records/,
        'the ordinary status should come back when the rebuild finishes');
    window.close();
});
