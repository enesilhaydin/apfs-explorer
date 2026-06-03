// APFS Explorer - frontend glue.
// Resolve the Tauri invoke bridge defensively: depending on the Tauri 2
// minor version + withGlobalTauri, the function is exposed at one of a few
// paths.  If none is found we surface the available keys to the status bar
// so we can diagnose without DevTools.

const detectedEl = document.getElementById('detected');
const mountedEl  = document.getElementById('mounted');
const statusEl   = document.getElementById('status-bar');
const refreshBtn = document.getElementById('refresh-btn');

// Best-effort per-session map: drive index -> writable bool, recorded when the
// GUI mounts a drive, so the MOUNTED list can show a read-only / writable badge.
// (A launcher mount discovered after a GUI restart defaults to read-only - the
// read-only launcher class is the default.)
const mountModes = {};

function resolveInvoke() {
  const t = window.__TAURI__;
  if (t) {
    if (t.core && typeof t.core.invoke === 'function') return t.core.invoke;
    if (typeof t.invoke === 'function') return t.invoke;
    if (t.tauri && typeof t.tauri.invoke === 'function') return t.tauri.invoke;
  }
  if (window.__TAURI_INTERNALS__ && typeof window.__TAURI_INTERNALS__.invoke === 'function') {
    return window.__TAURI_INTERNALS__.invoke;
  }
  return null;
}

const invoke = resolveInvoke();

// Surface any uncaught error to the status bar so a stuck UI is diagnosable.
window.addEventListener('error', (e) => {
  if (statusEl) {
    statusEl.textContent = 'JS error: ' + (e.message || e.error);
    statusEl.classList.add('error');
  }
});

if (!invoke) {
  const keys = window.__TAURI__ ? Object.keys(window.__TAURI__).join(',') : '(no __TAURI__)';
  if (statusEl) {
    statusEl.textContent = 'Tauri bridge missing. __TAURI__ keys: ' + keys;
    statusEl.classList.add('error');
  }
}

function setStatus(msg, level = 'info') {
  statusEl.textContent = msg;
  statusEl.classList.remove('error', 'success', 'busy');
  if (level === 'error')   statusEl.classList.add('error');
  if (level === 'success') statusEl.classList.add('success');
  if (level === 'busy')    statusEl.classList.add('busy');
}

function humanSize(bytes) {
  if (!bytes || bytes <= 0) return '?';
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  let i = 0;
  let v = bytes;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v >= 10 ? 0 : 1)} ${units[i]}`;
}

// Inline lucide-style SVG icons (monochrome, inherit currentColor).
const ICON_HDD = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="22" x2="2" y1="12" y2="12"/><path d="M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z"/><line x1="6" x2="6.01" y1="16" y2="16"/><line x1="10" x2="10.01" y1="16" y2="16"/></svg>';
const ICON_USB = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="10" cy="7" r="1"/><circle cx="4" cy="20" r="1"/><path d="M4.7 19.3 19 5"/><path d="m21 3-3 1 2 2 1-3Z"/><path d="M9.26 7.68 5 12l2 5"/><path d="m10 14 5 2 3.5-3.5"/><path d="m18 12 1-1 1 1-1 1Z"/></svg>';
const ICON_FOLDER = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.55 6a2 2 0 0 1-1.94 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.93a2 2 0 0 1 1.66.9l.82 1.2a2 2 0 0 0 1.66.9H18a2 2 0 0 1 2 2v2"/></svg>';

function iconFor(kind) {
  switch (kind) {
    case 'Removable':
    case 'USB':       return ICON_USB;
    default:          return ICON_HDD;
  }
}

function clearChildren(el) {
  while (el.firstChild) el.removeChild(el.firstChild);
}

// In-app confirmation modal -> Promise<boolean>. Replaces the native confirm()
// so no "tauri.localhost says" origin chrome appears. Resolves false on Cancel,
// backdrop click, or Escape; true on Confirm or Enter.
function confirmDialog({ title, message, confirmText = 'Confirm', cancelText = 'Cancel' }) {
  return new Promise((resolve) => {
    const overlay = document.getElementById('modal-overlay');
    const titleEl = document.getElementById('modal-title');
    const msgEl = document.getElementById('modal-message');
    const okBtn = document.getElementById('modal-confirm');
    const cancelBtn = document.getElementById('modal-cancel');
    titleEl.textContent = title;
    msgEl.textContent = message;
    okBtn.textContent = confirmText;
    cancelBtn.textContent = cancelText;
    overlay.hidden = false;

    const cleanup = (result) => {
      overlay.hidden = true;
      okBtn.removeEventListener('click', onOk);
      cancelBtn.removeEventListener('click', onCancel);
      overlay.removeEventListener('mousedown', onBackdrop);
      document.removeEventListener('keydown', onKey);
      resolve(result);
    };
    const onOk = () => cleanup(true);
    const onCancel = () => cleanup(false);
    const onBackdrop = (e) => { if (e.target === overlay) cleanup(false); };
    const onKey = (e) => {
      if (e.key === 'Escape') cleanup(false);
      else if (e.key === 'Enter') cleanup(true);
    };
    okBtn.addEventListener('click', onOk);
    cancelBtn.addEventListener('click', onCancel);
    overlay.addEventListener('mousedown', onBackdrop);
    document.addEventListener('keydown', onKey);
    okBtn.focus();
  });
}

function renderEmpty(parent, text) {
  clearChildren(parent);
  const d = document.createElement('div');
  d.className = 'empty';
  d.textContent = text;
  parent.appendChild(d);
}

function renderDetected(drives) {
  // Only show APFS disks (is_apfs fails open if detection is unavailable).
  const candidates = drives.filter(d => !d.mounted && d.is_apfs);
  if (candidates.length === 0) {
    renderEmpty(detectedEl, 'No APFS drives detected.');
    return;
  }
  clearChildren(detectedEl);
  for (const d of candidates) {
    const card = document.createElement('div');
    card.className = 'card';

    const icon = document.createElement('div');
    icon.className = 'card-icon';
    icon.innerHTML = iconFor(d.kind);
    card.appendChild(icon);

    const body = document.createElement('div');
    body.className = 'card-body';
    const title = document.createElement('div');
    title.className = 'card-title';
    title.textContent = `PhysicalDrive${d.index} - ${d.model || 'unknown'}`;
    body.appendChild(title);
    const meta = document.createElement('div');
    meta.className = 'card-meta';
    meta.textContent = `${humanSize(d.size_bytes)} · ${d.kind}`;
    body.appendChild(meta);
    card.appendChild(body);

    const actions = document.createElement('div');
    actions.className = 'card-actions';

    // Mode toggle: read-only (default, safe) vs writable. Writable is an
    // explicit opt-in; the engine still forces read-only on snapshot volumes.
    let writable = false;
    const seg = document.createElement('div');
    seg.className = 'segmented';
    seg.setAttribute('role', 'group');
    const roBtn = document.createElement('button');
    roBtn.type = 'button';
    roBtn.className = 'seg active';
    roBtn.textContent = 'Read-only';
    const rwBtn = document.createElement('button');
    rwBtn.type = 'button';
    rwBtn.className = 'seg warn';
    rwBtn.textContent = 'Writable';
    rwBtn.title = 'Read+write. Volumes with snapshots fall back to read-only.';
    roBtn.addEventListener('click', () => {
      writable = false; roBtn.classList.add('active'); rwBtn.classList.remove('active');
    });
    rwBtn.addEventListener('click', () => {
      writable = true; rwBtn.classList.add('active'); roBtn.classList.remove('active');
    });
    seg.appendChild(roBtn);
    seg.appendChild(rwBtn);
    actions.appendChild(seg);

    const mountBtn = document.createElement('button');
    mountBtn.className = 'btn btn-primary';
    mountBtn.textContent = 'Mount';
    mountBtn.addEventListener('click', async () => {
      if (writable) {
        const ok = await confirmDialog({
          title: 'Mount writable?',
          message: 'Changes will be written to the disk. Volumes that contain '
            + 'snapshots are mounted read-only for safety.',
          confirmText: 'Mount writable',
        });
        if (!ok) return;
      }
      mountClicked(d.index, writable);
    });
    actions.appendChild(mountBtn);

    card.appendChild(actions);
    detectedEl.appendChild(card);
  }
}

function renderMounted(drives) {
  const live = drives.filter(d => d.mounted);
  if (live.length === 0) {
    renderEmpty(mountedEl, 'Nothing mounted yet.');
    return;
  }
  clearChildren(mountedEl);
  for (const d of live) {
    const card = document.createElement('div');
    card.className = 'card';

    const icon = document.createElement('div');
    icon.className = 'card-icon';
    icon.innerHTML = ICON_FOLDER;
    card.appendChild(icon);

    const body = document.createElement('div');
    body.className = 'card-body';
    const title = document.createElement('div');
    title.className = 'card-title title-row';
    const name = document.createElement('span');
    name.className = 'title-text';
    name.textContent = `${d.letter}  ·  ${d.model || `PhysicalDrive${d.index}`}`;
    title.appendChild(name);
    // read-only / writable badge (best-effort, from this session's mount mode)
    const rw = mountModes[d.index] === true;
    const badge = document.createElement('span');
    badge.className = 'badge ' + (rw ? 'badge-warn' : 'badge-ok');
    badge.textContent = rw ? 'Writable' : 'Read-only';
    title.appendChild(badge);
    body.appendChild(title);
    const meta = document.createElement('div');
    meta.className = 'card-meta';
    meta.textContent = `${humanSize(d.size_bytes)} · ${d.kind}`;
    body.appendChild(meta);
    card.appendChild(body);

    const actions = document.createElement('div');
    actions.className = 'card-actions';
    const openBtn = document.createElement('button');
    openBtn.className = 'btn btn-outline';
    openBtn.textContent = 'Open';
    openBtn.addEventListener('click', () => openClicked(d.letter));
    actions.appendChild(openBtn);

    const unmountBtn = document.createElement('button');
    unmountBtn.className = 'btn btn-destructive';
    unmountBtn.textContent = 'Unmount';
    unmountBtn.addEventListener('click', () => unmountClicked(d.index));
    actions.appendChild(unmountBtn);

    card.appendChild(actions);
    mountedEl.appendChild(card);
  }
}

async function refresh() {
  try {
    const drives = await invoke('list_drives');
    renderDetected(drives);
    renderMounted(drives);
    const apfs = drives.filter(d => d.is_apfs).length;
    setStatus(`Ready. ${apfs} APFS drive(s).`);
  } catch (e) {
    setStatus(`Failed to list drives: ${e}`, 'error');
  }
}

async function mountClicked(index, writable) {
  setStatus(`Mounting PhysicalDrive${index}${writable ? ' (writable)' : ' (read-only)'}…`, 'busy');
  try {
    const letter = await invoke('mount_drive', { drive: index, writable });
    mountModes[index] = writable;
    setStatus(`PhysicalDrive${index} mounted at ${letter}.`, 'success');
    await refresh();
  } catch (e) {
    const msg = String(e);
    if (msg.includes('user cancelled')) {
      setStatus('UAC cancelled.', 'error');
    } else {
      setStatus(`Mount failed: ${e}`, 'error');
    }
  }
}

async function unmountClicked(index) {
  setStatus(`Unmounting PhysicalDrive${index}…`, 'busy');
  try {
    await invoke('unmount_drive', { drive: index });
    setStatus(`PhysicalDrive${index} unmounted.`, 'success');
    await refresh();
  } catch (e) {
    setStatus(`Unmount failed: ${e}`, 'error');
  }
}

async function openClicked(letter) {
  try {
    await invoke('open_drive_letter', { letter });
  } catch (e) {
    setStatus(`Open in Explorer failed: ${e}`, 'error');
  }
}

refreshBtn.addEventListener('click', () => {
  setStatus('Refreshing…', 'busy');
  refresh();
});

// Auto-refresh on load + every 3 seconds so device-arrival/removal shows up.
refresh();
setInterval(refresh, 3000);
