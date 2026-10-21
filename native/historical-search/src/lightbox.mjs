// A photograph shown large, inside the application.
//
// For reading small writing on a photograph -- a caption on a mount, a name
// on a sign -- which the picture on the card is too small to allow. It can be
// zoomed and moved, and a button hands the original to the computer's own
// picture viewer for anything this does not do.
//
// The picture is an enlarged JPEG made by the application (`prepare_large_image`)
// from the high-resolution original where there is one. The window never holds
// a path: it asks by id, as it does for every other file.
//
// Loaded only when a photograph is first enlarged, like the PDF viewer.

const MIN_SCALE_FLOOR = 0.02;
const MAX_SCALE = 8;

/// The scale at which the whole picture fits the stage. Never above 1: a
/// small picture is shown at its own size, not stretched.
export function fitScale(stageW, stageH, imgW, imgH) {
  if (!(stageW > 0 && stageH > 0 && imgW > 0 && imgH > 0)) return 1;
  return Math.min(stageW / imgW, stageH / imgH, 1);
}

/// Keeps the picture from being moved out of view. Where it is smaller than
/// the stage it is centred; where it is larger, an edge cannot come past the
/// stage's edge.
export function clampPan(tx, ty, scale, stageW, stageH, imgW, imgH) {
  const w = imgW * scale;
  const h = imgH * scale;
  const axis = (t, view, size) => (size <= view ? (view - size) / 2 : Math.min(0, Math.max(view - size, t)));
  return { tx: axis(tx, stageW, w), ty: axis(ty, stageH, h) };
}

/// The view after zooming by `factor` about the point (cx, cy) of the stage:
/// the spot under the pointer stays under it. `fit` is the smallest scale
/// allowed, so the picture cannot be shrunk to a dot.
export function zoomAbout(view, factor, cx, cy, fit, stageW, stageH, imgW, imgH) {
  const floor = Math.max(Math.min(fit, 1), MIN_SCALE_FLOOR);
  const scale = Math.min(MAX_SCALE, Math.max(floor, view.scale * factor));
  const ratio = scale / view.scale;
  const moved = clampPan(cx - (cx - view.tx) * ratio, cy - (cy - view.ty) * ratio, scale, stageW, stageH, imgW, imgH);
  return { scale, ...moved };
}

const percent = scale => `${Math.round(scale * 100)}%`;

/// Opens the viewer.
///
///   count, index  how many pictures the record has, and which to show first
///   load(i)       resolves to { url, caption } for picture i, or rejects with
///                 a reason fit to show
///   openExternal(i)  hands picture i to the computer's picture viewer
///   onIndex(i)    told when a different picture is being shown
///   onClose()     told when it is closed
///   title         the record's name
///   doc           the document to build in (the page's own unless a test says)
///
/// Returns { close }.
export function openLightbox({ title = '', count = 1, index = 0, load, openExternal, onIndex, onClose, doc = document }) {
  const win = doc.defaultView;
  const opener = doc.activeElement;
  const root = doc.createElement('div');
  root.className = 'lb';
  root.setAttribute('role', 'dialog');
  root.setAttribute('aria-modal', 'true');
  root.setAttribute('aria-label', title ? `${title}, enlarged` : 'Enlarged picture');
  root.tabIndex = -1;
  root.innerHTML = `
    <div class="lb-bar">
      <span class="lb-title"></span>
      <span class="lb-group lb-pager" hidden>
        <button type="button" class="lb-btn" data-act="prev" aria-label="Previous picture">&larr;</button>
        <span class="lb-count"></span>
        <button type="button" class="lb-btn" data-act="next" aria-label="Next picture">&rarr;</button>
      </span>
      <span class="lb-group">
        <button type="button" class="lb-btn" data-act="out" aria-label="Zoom out">&minus;</button>
        <span class="lb-zoom" aria-live="polite"></span>
        <button type="button" class="lb-btn" data-act="in" aria-label="Zoom in">+</button>
        <button type="button" class="lb-btn" data-act="fit">Fit</button>
        <button type="button" class="lb-btn" data-act="actual">100%</button>
      </span>
      <button type="button" class="lb-btn lb-wide" data-act="external">Open in your photo viewer</button>
      <button type="button" class="lb-btn lb-wide" data-act="close">Close</button>
    </div>
    <div class="lb-caption" hidden></div>
    <div class="lb-stage"><img class="lb-img" alt="" hidden><div class="lb-msg" role="status"></div></div>
    <div class="lb-hint">Scroll to zoom &middot; drag to move &middot; double-click to zoom in or out &middot; Esc to close</div>`;
  const $ = sel => root.querySelector(sel);
  const stage = $('.lb-stage');
  const img = $('.lb-img');
  const msg = $('.lb-msg');
  const captionEl = $('.lb-caption');
  const zoomEl = $('.lb-zoom');
  $('.lb-title').textContent = title;
  // Pressing on a picture and moving would otherwise start the browser's own
  // drag of the image, which cancels the pointer events the panning uses. The
  // stylesheet asks for this too, but not every webview honours it.
  img.draggable = false;
  img.addEventListener('dragstart', (e) => e.preventDefault());
  if (count > 1) $('.lb-pager').hidden = false;

  let current = index;
  let natW = 0;
  let natH = 0;
  let view = { scale: 1, tx: 0, ty: 0 };
  let fitting = true;
  let token = 0;
  let closed = false;

  const size = () => ({ w: stage.clientWidth, h: stage.clientHeight });
  const fit = () => fitScale(size().w, size().h, natW, natH);

  function paint() {
    img.style.transform = `translate(${view.tx}px, ${view.ty}px) scale(${view.scale})`;
    zoomEl.textContent = natW ? (fitting ? `Fit (${percent(view.scale)})` : percent(view.scale)) : '';
    const dragging = natW && view.scale > fit() + 0.001;
    stage.classList.toggle('lb-pannable', Boolean(dragging));
  }

  function setView(next, stillFitting = false) {
    view = next;
    fitting = stillFitting;
    paint();
  }

  function showFit() {
    if (!natW) return;
    const { w, h } = size();
    const scale = fit();
    setView({ scale, ...clampPan(0, 0, scale, w, h, natW, natH) }, true);
  }

  /// Zooms to `scale`, about the stage's centre unless a point is given.
  function zoomTo(scale, cx, cy) {
    if (!natW) return;
    const { w, h } = size();
    cx = cx ?? w / 2;
    cy = cy ?? h / 2;
    setView(zoomAbout(view, scale / view.scale, cx, cy, fit(), w, h, natW, natH));
    if (Math.abs(view.scale - fit()) < 0.0005) fitting = true;
    paint();
  }

  function zoomBy(factor, cx, cy) {
    zoomTo(view.scale * factor, cx, cy);
  }

  function pan(dx, dy) {
    const { w, h } = size();
    setView({ scale: view.scale, ...clampPan(view.tx + dx, view.ty + dy, view.scale, w, h, natW, natH) }, fitting);
  }

  function say(text) {
    img.hidden = true;
    natW = natH = 0;
    msg.hidden = false;
    msg.textContent = text;
    paint();
  }

  async function show(i) {
    current = i;
    const mine = ++token;
    if (onIndex) onIndex(i);
    $('.lb-count').textContent = `${i + 1} of ${count}`;
    $('[data-act="prev"]').disabled = i <= 0;
    $('[data-act="next"]').disabled = i >= count - 1;
    captionEl.hidden = true;
    say('Preparing a larger view…');
    let loaded;
    try {
      loaded = await load(i);
    } catch (reason) {
      if (mine === token && !closed) {
        say(`${String(reason && reason.message ? reason.message : reason)} You can still use "Open in your photo viewer".`);
      }
      return;
    }
    if (mine !== token || closed) return;
    if (loaded.caption) {
      captionEl.textContent = loaded.caption;
      captionEl.hidden = false;
    }
    const probe = new win.Image();
    probe.onload = () => {
      if (mine !== token || closed) return;
      natW = probe.naturalWidth;
      natH = probe.naturalHeight;
      img.src = loaded.url;
      img.style.width = `${natW}px`;
      img.style.height = `${natH}px`;
      img.hidden = false;
      msg.hidden = true;
      showFit();
    };
    probe.onerror = () => {
      if (mine === token && !closed) {
        say('This picture could not be shown larger. You can still use "Open in your photo viewer".');
      }
    };
    probe.src = loaded.url;
  }

  function go(delta) {
    const next = current + delta;
    if (next >= 0 && next < count) show(next);
  }

  function close() {
    if (closed) return;
    closed = true;
    token += 1;
    doc.removeEventListener('keydown', onKey, true);
    win.removeEventListener('resize', onResize);
    root.remove();
    if (opener && typeof opener.focus === 'function' && doc.contains(opener)) opener.focus();
    if (onClose) onClose();
  }

  const actions = {
    prev: () => go(-1),
    next: () => go(1),
    out: () => zoomBy(1 / 1.4),
    in: () => zoomBy(1.4),
    fit: showFit,
    actual: () => zoomTo(1),
    external: () => openExternal && openExternal(current),
    close,
  };
  root.addEventListener('click', (e) => {
    const button = e.target.closest && e.target.closest('[data-act]');
    if (button && !button.disabled) actions[button.dataset.act]();
  });

  // Captured before anything else on the page sees it, and stopped: Escape
  // must close only the viewer and not the card underneath, and the arrow
  // keys must not move the card on to the next record.
  function onKey(e) {
    const stop = () => { e.preventDefault(); e.stopImmediatePropagation(); };
    switch (e.key) {
      case 'Escape': stop(); close(); break;
      case '+': case '=': stop(); zoomBy(1.4); break;
      case '-': case '_': stop(); zoomBy(1 / 1.4); break;
      case '0': stop(); showFit(); break;
      case '1': stop(); zoomTo(1); break;
      case 'ArrowLeft': case 'ArrowRight': case 'ArrowUp': case 'ArrowDown': {
        stop();
        const step = 80;
        if (natW && view.scale > fit() + 0.001) {
          pan(e.key === 'ArrowLeft' ? step : e.key === 'ArrowRight' ? -step : 0,
              e.key === 'ArrowUp' ? step : e.key === 'ArrowDown' ? -step : 0);
        } else if (e.key === 'ArrowLeft') go(-1);
        else if (e.key === 'ArrowRight') go(1);
        break;
      }
      default: break;
    }
  }
  doc.addEventListener('keydown', onKey, true);

  function onResize() {
    if (!natW) return;
    if (fitting) showFit();
    else zoomBy(1);
  }
  win.addEventListener('resize', onResize);

  // The wheel zooms about the pointer. A page's own scrolling is not wanted
  // here, so it is cancelled; `passive: false` is what allows that.
  stage.addEventListener('wheel', (e) => {
    if (!natW) return;
    e.preventDefault();
    const rect = stage.getBoundingClientRect();
    const lines = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? 400 : 1;
    zoomBy(Math.exp(-e.deltaY * lines * 0.0015), e.clientX - rect.left, e.clientY - rect.top);
  }, { passive: false });

  let drag = null;
  stage.addEventListener('pointerdown', (e) => {
    if (!natW || e.button > 0) return;
    drag = { x: e.clientX, y: e.clientY };
    if (stage.setPointerCapture) { try { stage.setPointerCapture(e.pointerId); } catch (_) { /* not capturable */ } }
    stage.classList.add('lb-dragging');
  });
  stage.addEventListener('pointermove', (e) => {
    if (!drag) return;
    pan(e.clientX - drag.x, e.clientY - drag.y);
    drag = { x: e.clientX, y: e.clientY };
  });
  const endDrag = () => { drag = null; stage.classList.remove('lb-dragging'); };
  // The browser delivers pointer moves once a frame, so a quick flick can end
  // with its last stretch still undelivered; the release carries where it ended.
  stage.addEventListener('pointerup', (e) => {
    if (drag) pan(e.clientX - drag.x, e.clientY - drag.y);
    endDrag();
  });
  stage.addEventListener('pointercancel', endDrag);

  // Double-click goes in to a size where writing can be read, or back out.
  stage.addEventListener('dblclick', (e) => {
    if (!natW) return;
    const rect = stage.getBoundingClientRect();
    if (fitting) zoomTo(Math.max(1, fit() * 2.5), e.clientX - rect.left, e.clientY - rect.top);
    else showFit();
  });

  doc.body.appendChild(root);
  root.focus();
  show(current);
  return { close };
}
