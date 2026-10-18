// Loads the real src/index.html into a headless DOM with a stand-in for the
// Rust side, so the window's own state machine can be exercised.
//
// The page is loaded as shipped rather than copied or refactored into
// something more testable: the point is to check the file that actually runs,
// including the parts that are easy to get subtly wrong -- the interplay
// between typing, pressing Enter for everything, "Show more results", and
// stepping between items, all of which depend on state that several handlers
// write to.

import { JSDOM } from 'jsdom';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const PAGE = join(here, '..', '..', 'src', 'index.html');

/// Builds a fake collection of `total` records.
///
/// Deliberately mirrors the shapes the Rust commands actually return,
/// including the snake_case field names that cross the boundary -- getting
/// those wrong in a stub would make the tests agree with themselves and
/// nothing else.
export function makeBackend({
    total = 0, pageSize = 40, maxAll = 2000, exactIdFor = null, imagesFor = null,
    // Ids of records that are PDFs rather than catalogue records or photos.
    documentIds = [],
    // What opening a PDF answers with: null for success, or the message the
    // real command gives when the file has gone.
    openDocumentError = null,
} = {}) {
    const records = Array.from({ length: total }, (_, i) => ({
        id: i + 1,
        catalog: 'Photo',
        accessno: `2010.1.${i + 1}`,
        title: `Record number ${i + 1}`,
        snippet: `description of record ${i + 1}`,
        has_image: true,
        is_document: documentIds.includes(i + 1),
        is_exact_id: false,
    }));
    // A PDF has no picture and no catalog number.
    for (const r of records) {
        if (r.is_document) { r.has_image = false; r.accessno = ''; r.catalog = 'Newsletters'; }
    }

    const backend = {
        calls: [],
        records,
        // Lets a test hold a response open, to reproduce answers arriving out
        // of the order they were asked for.
        pending: [],
        holdSearches: false,
        holdItems: false,
        holdConfig: false,
        // Rebuilds the stand-in has finished. The real command reports this so
        // the window can tell that record numbering has changed; modelling it
        // here keeps the stand-in honest, and a test that relies on a rebuild
        // having happened is then relying on the same signal the application
        // really sends.
        rebuildsCompleted: 0,

        async invoke(cmd, args = {}) {
            backend.calls.push({ cmd, args });
            switch (cmd) {
                case 'get_status':
                    return {
                        last_updated: 1700000000, record_count: total,
                        rebuilds_completed: backend.rebuildsCompleted, in_progress: false,
                        last_error: null, warnings: [], phase: null,
                        phase_current: null, phase_total: null,
                    };
                // Returns a boolean like the real one, and the rebuild it
                // stands for finishes at once.
                case 'start_refresh':
                    backend.rebuildsCompleted += 1;
                    return true;
                case 'get_config': {
                    const cfg = { data_dir: '', images_dir: '', multimedia_dir: '', photo_archive_roots: [], document_roots: [] };
                    if (backend.holdConfig) {
                        return new Promise(resolve => backend.pending.push(() => resolve(cfg)));
                    }
                    return cfg;
                }
                case 'get_diagnostics':
                    return { text: 'diagnostics\nplatform: test\nno index', probe_image: null,
                             log_path: '/somewhere/diagnostics.log' };
                case 'save_diagnostics': return true;
                case 'validate_path': return { ok: true, message: 'Folder found.' };
                case 'search': {
                    const run = () => {
                        const limit = args.showAll ? maxAll : pageSize;
                        const offset = args.offset ?? 0;
                        const query = args.query.trim();
                        // Records are tagged with the query that asked for
                        // them. Without that, every search returns the same
                        // thing and a test cannot tell results for one query
                        // apart from results for another -- which is exactly
                        // what the ordering guards exist to prevent.
                        // Tagged through the title, which is a field the
                        // Rust side really sends. An extra field here would
                        // be a stand-in more generous than the real thing,
                        // and the window could come to rely on it.
                        const matched = query
                            ? records.map(r => ({ ...r, title: `${query} record ${r.id}` }))
                            : [];
                        const slice = matched.slice(offset, offset + limit).map(r => ({
                            ...r,
                            is_exact_id: exactIdFor !== null && r.accessno === exactIdFor,
                        }));
                        return { results: slice, has_more: offset + limit < matched.length, total: matched.length };
                    };
                    if (backend.holdSearches) {
                        return new Promise(resolve => backend.pending.push(() => resolve(run())));
                    }
                    return run();
                }
                case 'get_item': {
                    const r = records.find(x => x.id === args.id);
                    if (!r) return null;
                    const item = {
                        id: r.id, catalog: r.catalog, accessno: r.accessno, title: r.title,
                        images: imagesFor
                            ? imagesFor(r)
                            : [{ id: r.id * 100, kind: 'imgthumb', caption: '', has_thumb: true, has_master: false }],
                        fields: [{ label: 'Title', value: r.title }],
                        document: r.is_document
                            ? { file_name: `${r.title}.pdf`, pages: 7, has_text: true }
                            : null,
                    };
                    if (r.is_document) item.images = [];
                    if (backend.holdItems) {
                        return new Promise(resolve => backend.pending.push(() => resolve(item)));
                    }
                    return item;
                }
                case 'log_from_ui': return null;
                case 'download_image': return true;
                case 'open_document':
                    if (openDocumentError) throw openDocumentError;
                    return null;
                default: return null;
            }
        },
        // Releases held responses, oldest first unless told otherwise.
        release(order = 'fifo') {
            const queued = backend.pending.splice(0);
            const run = order === 'lifo' ? queued.reverse() : queued;
            run.forEach(fn => fn());
        },
    };
    return backend;
}

export async function loadPage(backend) {
    const html = readFileSync(PAGE, 'utf8');
    const dom = new JSDOM(html, {
        runScripts: 'dangerously',
        url: 'http://localhost/',
        beforeParse(window) {
            window.__TAURI__ = {
                core: {
                    invoke: (cmd, args) => backend.invoke(cmd, args),
                    convertFileSrc: (p, scheme) => `${scheme}://localhost/${encodeURIComponent(p)}`,
                },
            };
            // jsdom has no clipboard and does not implement alert; the page
            // uses both on paths these tests touch.
            window.alert = (msg) => { window.__alerts = window.__alerts || []; window.__alerts.push(msg); };
            window.navigator.clipboard = { writeText: async () => {} };
        },
    });
    await settle(dom.window);
    return dom;
}

/// Waits for the page's debounce and any promises it started.
/// The search box waits 120ms before asking, so anything shorter tests
/// nothing; this leaves room without being slow.
export function settle(window, ms = 220) {
    return new Promise(resolve => window.setTimeout(resolve, ms));
}

export function type(window, text) {
    const q = window.document.getElementById('q');
    q.value = text;
    q.dispatchEvent(new window.Event('input'));
}

export function pressEnter(window) {
    const q = window.document.getElementById('q');
    q.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
}

export function resultIds(window) {
    return [...window.document.querySelectorAll('.result')].map(el => el.dataset.id);
}

export function click(window, selector) {
    const el = window.document.querySelector(selector);
    if (!el) throw new Error(`nothing matching ${selector} to click`);
    el.dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
    return el;
}

/// Which query each rendered result actually came from, read off the titles.
export function resultQueries(window) {
    return [...window.document.querySelectorAll('.result .title')]
        .map(el => el.textContent.trim().split(' ')[0]);
}
