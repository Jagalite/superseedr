// SPDX-License-Identifier: GPL-3.0-or-later
import './webtorrent.css';
import {createRtcHost} from './rtc-host.js';
const $ = id => document.getElementById(id);
const error = value => { $('error').textContent = String(value); };
const worker = new Worker(new URL('./engine-worker.js', import.meta.url), {type: 'module'});
const rtcHost = createRtcHost(worker);
const port = rtcHost.connect();
const closeRtc = () => rtcHost.close();
let serial = 0, stopped = false, ready = false, latest = {torrents: []};
const pending = new Map();
function call(method, ...args) {
  if (stopped || pending.size >= 32) return Promise.reject(new Error('Client is unavailable or busy'));
  return new Promise((resolve, reject) => { const id = ++serial; pending.set(id, {resolve, reject}); worker.postMessage({id, method, args}); });
}
worker.onmessage = ({data}) => {
  if (data.snapshot) { latest = data.snapshot; render(); return; }
  const item = pending.get(data.id); if (!item) return;
  pending.delete(data.id); data.error ? item.reject(new Error(data.error)) : item.resolve(data.result);
};
worker.onerror = event => { stopped = true; worker.terminate(); error(event.message); render(); for (const item of pending.values()) item.reject(new Error(event.message)); pending.clear(); closeRtc(); };
const started = new Promise((resolve, reject) => { pending.set(0, {resolve, reject}); worker.postMessage({id: 0, method: 'start', port}, [port]); });
const hex = bytes => bytes.map(byte => byte.toString(16).padStart(2, '0')).join('');
const size = bytes => { const units = ['B','KiB','MiB','GiB']; let n = bytes, unit = 0; while (n >= 1024 && unit < 3) { n /= 1024; unit++; } return `${n.toFixed(unit ? 1 : 0)} ${units[unit]}`; };
function button(label, action) { const element = document.createElement('button'); element.textContent = label; element.onclick = async () => { element.disabled = true; try { await action(); } catch (problem) { error(problem); } finally { element.disabled = false; } }; return element; }
const rows = new Map();
function render() {
  if (!ready) return;
  $('stop').disabled = stopped;
  $('add').querySelector('button').disabled = stopped;
  $('torrent').disabled = stopped;
  $('status').textContent = stopped ? 'Stopped' : latest.network === 'reconnecting' ? 'Reconnecting WebRTC…' : `${latest.torrents.length} torrent${latest.torrents.length === 1 ? '' : 's'} · ready to seed`;
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
      const mode = document.createElement('select'); mode.setAttribute('aria-label', 'Download order');
      for (const [value, label] of [['rarest_first', 'Rarest first'], ['sequential', 'Sequential']]) {
        const option = document.createElement('option'); option.value = value; option.textContent = label; mode.append(option);
      }
      mode.onchange = async () => { mode.disabled = true; try { await call('set_download_mode', hash, mode.value); } catch (problem) { error(problem); } finally { mode.disabled = false; } };
      actions.append(mode, pause, button('Remove', async () => { if (confirm('Remove this torrent and its browser files?')) await call('remove', hash, true); }));
      heading.append(title, actions);
      const progress = document.createElement('progress'); progress.max = 1;
      const details = document.createElement('p'); details.className = 'torrent-details'; const files = document.createElement('div'); files.className = 'files';
      element.append(heading, progress, details, files); $('torrents').append(element);
      row = {element, title, mode, pause, progress, details, files, fileSignature: ''}; rows.set(hash, row);
    }
    row.mode.value = torrent.download_mode || 'rarest_first'; row.mode.disabled = stopped;
    row.torrent = torrent; row.title.textContent = torrent.torrent_name || `Metadata pending · ${hash.slice(0,12)}`;
    row.pause.textContent = torrent.torrent_control_state === 'Paused' ? 'Resume' : 'Pause';
    row.progress.value = torrent.number_of_pieces_total ? torrent.number_of_pieces_completed / torrent.number_of_pieces_total : 0;
    row.details.textContent = `${torrent.activity_message} · ${size(torrent.total_size)} · ↓ ${size(torrent.download_speed_bps / 8)}/s · ↑ ${size(torrent.upload_speed_bps / 8)}/s · ${torrent.number_of_successfully_connected_peers} peers`;
    const signature = JSON.stringify(torrent.files || []);
    if (signature !== row.fileSignature) {
      row.fileSignature = signature; row.files.replaceChildren();
      (torrent.files || []).forEach((file, index) => {
        if (file.is_padding) return;
        const line = document.createElement('div'); line.className = 'file-row'; const name = document.createElement('span'); name.className = 'file-name'; name.textContent = file.path;
        const length = document.createElement('span'); length.className = 'file-size'; length.textContent = size(file.length);
        line.append(name, length, button('Save', () => save(hash, index, file))); row.files.append(line);
      });
    }
  }
  for (const [hash, row] of rows) if (!live.has(hash)) { row.element.remove(); rows.delete(hash); }
}
async function save(hash, index, file) {
  const name = file.path.split('/').pop();
  if (typeof window.showSaveFilePicker === 'function') {
    const handle = await window.showSaveFilePicker({suggestedName: name}); const writer = await handle.createWritable();
    try { for (let at = 0; at < file.length; at += 1024 * 1024) await writer.write(await call('read_file', hash, index, BigInt(at), Math.min(1024 * 1024, file.length - at))); await writer.close(); }
    catch (problem) { await writer.abort(); throw problem; }
  } else {
    if (file.length > 64 * 1024 * 1024) throw new Error('Saving this file needs a browser with a file save picker');
    const chunks = []; for (let at = 0; at < file.length; at += 1024 * 1024) chunks.push(await call('read_file', hash, index, BigInt(at), Math.min(1024 * 1024, file.length - at)));
    const url = URL.createObjectURL(new Blob(chunks)); const link = document.createElement('a'); link.href = url; link.download = name; link.click(); setTimeout(() => URL.revokeObjectURL(url), 60000);
  }
}
$('add').onsubmit = async event => { event.preventDefault(); try { await started; await call('add_magnet', $('magnet').value.trim()); $('magnet').value = ''; } catch (problem) { error(problem); } };
$('torrent').onchange = async event => { const file = event.target.files[0]; if (!file) return; try { await started; if (file.size > 4 * 1024 * 1024) throw new Error('Torrent metadata exceeds size limit'); await call('add_torrent', new Uint8Array(await file.arrayBuffer())); } catch (problem) { error(problem); } event.target.value = ''; };
$('stop').onclick = async () => { try { await started; await call('shutdown'); stopped = true; worker.terminate(); closeRtc(); render(); } catch (problem) { error(problem); } };
window.addEventListener('pagehide', () => { closeRtc(); worker.terminate(); });
started.then(async () => { ready = true; render(); const input = new URL(location.href).searchParams.get('magnet'); if (input) { $('magnet').value = input; await call('add_magnet', input); $('magnet').value = ''; } }).catch(error);
