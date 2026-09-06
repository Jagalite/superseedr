// SPDX-License-Identifier: GPL-3.0-or-later
import './webtorrent.css';
import {createRtcHost} from './rtc-host.js';
import {saveFile, canSaveFile} from './save-file.js';
const $ = id => document.getElementById(id);
const error = value => { $('error').textContent = String(value); };
const worker = new Worker(new URL('./engine-worker.js', import.meta.url), {type: 'module'});
const rtcHost = createRtcHost(worker);
const port = rtcHost.connect();
const closeRtc = () => rtcHost.close();
let serial = 0, stopped = false, ready = false, saving = false, latest = {torrents: []};
const pending = new Map();
function call(method, ...args) {
  if (stopped || pending.size >= 32) return Promise.reject(new Error('Client is unavailable or busy'));
  return new Promise((resolve, reject) => { const id = ++serial; pending.set(id, {resolve, reject}); worker.postMessage({id, method, args}); });
}
worker.onmessage = ({data}) => {
  if (data.snapshot) { latest = {...data.snapshot, torrents: data.snapshot.torrents || []}; render(); return; }
  const item = pending.get(data.id); if (!item) return;
  pending.delete(data.id); data.error ? item.reject(new Error(data.error)) : item.resolve(data.result);
};
function closeClient(reason) {
  stopped = true; worker.terminate(); closeRtc();
  for (const item of pending.values()) item.reject(new Error(reason));
  pending.clear(); render();
}
worker.onerror = event => { error(event.message); closeClient(event.message); };
const started = new Promise((resolve, reject) => { pending.set(0, {resolve, reject}); worker.postMessage({id: 0, method: 'start', port}, [port]); });
const hex = bytes => bytes.map(byte => byte.toString(16).padStart(2, '0')).join('');
const size = bytes => { const units = ['B','KiB','MiB','GiB']; let n = bytes, unit = 0; while (n >= 1024 && unit < 3) { n /= 1024; unit++; } return `${n.toFixed(unit ? 1 : 0)} ${units[unit]}`; };
function button(label, action) { const element = document.createElement('button'); element.textContent = label; element.onclick = async () => { element.dataset.busy = 'true'; element.disabled = true; try { await action(); } catch (problem) { error(problem); } finally { delete element.dataset.busy; element.disabled = stopped; } }; return element; }
const rows = new Map();
function render() {
  if (!ready) return;
  $('stop').disabled = stopped;
  $('add').querySelector('button').disabled = stopped;
  $('torrent').disabled = stopped;
  $('status').textContent = stopped ? 'Stopped' : latest.network === 'reconnecting' ? 'Reconnecting WebRTC…' : `${latest.torrents.length} torrent${latest.torrents.length === 1 ? '' : 's'}`;
  if (latest.error) error(latest.error);
  const live = new Set();
  for (const torrent of latest.torrents) {
    const hash = hex(torrent.info_hash); live.add(hash);
    let row = rows.get(hash);
    if (!row) {
      $('torrents').querySelector('.empty')?.remove();
      const element = document.createElement('article'); element.className = 'torrent';
      const heading = document.createElement('div'); heading.className = 'torrent-heading';
      const title = document.createElement('h2'); const actions = document.createElement('div'); actions.className = 'torrent-actions';
      const pause = button('Pause', () => call(row.torrent.torrent_control_state === 'Paused' ? 'resume' : 'pause', hash));
      actions.append(pause, button('Remove', async () => { if (confirm('Remove this torrent and its browser files? Any file still being saved may fail. Wait for your browser downloads to finish first.')) await call('remove', hash, true); }));
      heading.append(title, actions);
      const progress = document.createElement('progress'); progress.max = 1;
      const details = document.createElement('p'); details.className = 'torrent-details'; const files = document.createElement('div'); files.className = 'files';
      element.append(heading, progress, details, files); $('torrents').append(element);
      row = {element, title, pause, progress, details, files, fileRows: [], fileSignature: ''}; rows.set(hash, row);
    }
    row.torrent = torrent; row.title.textContent = torrent.torrent_name || `Metadata pending · ${hash.slice(0,12)}`;
    row.element.querySelectorAll('.torrent-actions button').forEach(control => { control.disabled = stopped || control.dataset.busy === 'true'; });
    row.pause.textContent = torrent.torrent_control_state === 'Paused' ? 'Resume' : 'Pause';
    row.progress.value = torrent.number_of_pieces_total ? torrent.number_of_pieces_completed / torrent.number_of_pieces_total : 0;
    row.progress.setAttribute('aria-label', `${row.title.textContent} download progress`);
    const state = torrent.torrent_control_state === 'Paused' ? 'Paused' : torrent.is_complete ? 'Seeding' : torrent.activity_message || 'Waiting for metadata';
    row.details.textContent = `${state} · ${(row.progress.value * 100).toFixed(1)}% · ${size(torrent.total_size)} · ↓ ${size(torrent.download_speed_bps / 8)}/s · ↑ ${size(torrent.upload_speed_bps / 8)}/s · ${torrent.number_of_successfully_connected_peers} peers`;
    const signature = JSON.stringify(torrent.files || []);
    if (signature !== row.fileSignature) {
      row.fileSignature = signature; row.files.replaceChildren(); row.fileRows = [];
      (torrent.files || []).forEach((file, index) => {
        if (file.is_padding) return;
        const line = document.createElement('div'); line.className = 'file-row'; const name = document.createElement('span'); name.className = 'file-name'; name.textContent = file.path.replace(/^payload\//, '');
        const length = document.createElement('span'); length.className = 'file-size'; length.textContent = size(file.length);
        const status = document.createElement('span'); status.className = 'file-status'; status.setAttribute('role', 'status');
        const save = document.createElement('button'); save.textContent = 'Save file';
        const entry = {index, file, save, status, result: '', active: false};
        save.onclick = async () => {
          if (save.disabled) return;
          saving = true; entry.active = true; entry.result = ''; error(''); render();
          try {
            status.textContent = 'Preparing save…';
            const outcome = await saveFile(file, {
              read: (offset, length) => call('read_file', hash, index, BigInt(offset), length),
              exportFile: () => call('export_file', hash, index),
            },
              bytes => { status.textContent = `Saving ${Math.floor(bytes / Math.max(1, file.length) * 100)}%…`; });
            entry.result = `${outcome === 'saved' ? 'Saved copy' : 'Download started; check your browser downloads'} · browser file retained for seeding`;
          } catch (problem) {
            if (problem.name !== 'AbortError') error(problem);
          } finally { saving = false; entry.active = false; render(); }
        };
        line.append(name, length, status, save); row.files.append(line); row.fileRows.push(entry);
      });
    }
    for (const entry of row.fileRows) {
      const verified = torrent.file_verified_bytes?.[entry.index];
      const complete = Number.isSafeInteger(verified) && verified === entry.file.length;
      const supported = canSaveFile(entry.file);
      if (!complete) entry.result = '';
      entry.save.disabled = stopped || saving || !complete || !supported;
      entry.save.title = !supported ? 'File length exceeds precise browser offsets' : !complete ? 'Available after this file is fully downloaded and verified' : 'Save a copy; keep browser data for seeding';
      if (!entry.active) entry.status.textContent = entry.result || (entry.file.is_skipped ? 'Skipped' : !supported && complete ? 'File length is unsupported' : complete ? 'Verified · ready to save' : verified == null ? 'Waiting for verification' : `${size(verified)} / ${size(entry.file.length)} verified`);
    }
  }
  if (!live.size && !$('torrents').querySelector('.empty')) {
    const empty = document.createElement('p'); empty.className = 'empty'; empty.textContent = 'Paste a magnet or open a torrent to begin.'; $('torrents').append(empty);
  }
  for (const [hash, row] of rows) if (!live.has(hash)) { row.element.remove(); rows.delete(hash); }
}
$('add').onsubmit = async event => { event.preventDefault(); error(''); try { await started; await call('add_magnet', $('magnet').value.trim()); $('magnet').value = ''; } catch (problem) { error(problem); } };
$('torrent').onchange = async event => { const file = event.target.files[0]; if (!file) return; error(''); try { await started; if (file.size > 4 * 1024 * 1024) throw new Error('Torrent metadata exceeds size limit'); await call('add_torrent', new Uint8Array(await file.arrayBuffer())); } catch (problem) { error(problem); } event.target.value = ''; };
$('stop').onclick = async () => { try { await started; await call('shutdown'); closeClient('Client stopped during the operation'); } catch (problem) { error(problem); } };
window.addEventListener('pagehide', () => { closeRtc(); worker.terminate(); });
started.then(async () => { ready = true; render(); const input = new URL(location.href).searchParams.get('magnet'); if (input) { $('magnet').value = input; await call('add_magnet', input); $('magnet').value = ''; } }).catch(error);
