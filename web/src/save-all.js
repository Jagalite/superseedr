// SPDX-License-Identifier: GPL-3.0-or-later
import {canSaveFile, copyFile, downloadFile} from './save-file.js';
import {createArchive} from './save-archive.js';

// Preserve the manifest hierarchy, excluding only the engine's storage prefix.
// Reject unsafe archive paths and exact collisions without changing file names.
export function planSaveAll(torrent, host = globalThis) {
  const files = [], paths = new Set(), directories = new Set();
  let total = 0;
  const hash = torrent.info_hash?.map(byte => byte.toString(16).padStart(2, '0')).join('');
  for (const [index, file] of (torrent.files || []).entries()) {
    if (file.is_padding || file.is_skipped) continue;
    if (!canSaveFile(file)) throw Error('File length exceeds precise browser offsets');
    const parts = file.path.replace(/^payload\//, '').split('/');
    // Multi-file storage adds an info-hash suffix to avoid payload collisions.
    // Export the original torrent root, matching only this torrent's exact suffix.
    if (hash && parts.length > 1 && parts[0] === `${torrent.torrent_name} [${hash}]`) parts[0] = torrent.torrent_name;
    const path = parts.join('/');
    if (/^[a-z]:/i.test(parts[0]) || parts.some(part => !part || part === '.' || part === '..' || /[\\\x00-\x1f]/.test(part)))
      throw Error(`Cannot safely export path: ${path}`);
    const keys = parts;
    const key = keys.join('/');
    if (paths.has(key) || directories.has(key)) throw Error(`Conflicting export path: ${path}`);
    for (let end = 1; end < keys.length; end++) {
      const parent = keys.slice(0, end).join('/');
      if (paths.has(parent)) throw Error(`Conflicting export path: ${path}`);
      directories.add(parent);
    }
    paths.add(key);
    if (torrent.file_verified_bytes?.[index] !== file.length)
      throw Error('Available after every included file is fully downloaded and verified');
    total += file.length;
    if (!Number.isSafeInteger(total)) throw Error('Total size exceeds precise browser offsets');
    files.push({index, file, path, parts});
  }
  if (!files.length) throw Error('No included files to save');
  const mode = typeof host.showDirectoryPicker === 'function' && folderCompatible(files) ? 'folder' : 'zip';
  return {files, total, mode};
}

// A ZIP can preserve names that portable folder writes cannot. Check every
// prefix too, so differently cased directory names are not silently merged.
function folderCompatible(files) {
  const names = new Map();
  for (const {parts} of files) {
    if (parts.some(part => /[:<>"|?*]/.test(part) || /[. ]$/.test(part)
      || /^(con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)/i.test(part))) return false;
    for (let end = 1; end <= parts.length; end++) {
      const path = parts.slice(0, end).join('/');
      const key = path.normalize('NFC').toLowerCase();
      if (names.has(key) && names.get(key) !== path) return false;
      names.set(key, path);
    }
  }
  return true;
}

async function parentDirectory(root, parts, create) {
  let directory = root;
  for (const part of parts.slice(0, -1)) directory = await directory.getDirectoryHandle(part, {create});
  return directory;
}

async function rejectExistingFile(parent, entry) {
  try { await parent.getFileHandle(entry.parts.at(-1)); }
  catch (error) { if (error.name === 'NotFoundError') return; throw error; }
  throw Error(`Destination already contains ${entry.path}. Choose an empty folder.`);
}

export async function saveAll(torrent, source, {progress = () => {}, signal} = {}, host = globalThis) {
  const {files, total, mode} = planSaveAll(torrent, host);
  let bytes = 0, completed = 0;
  const report = (path, current = 0) => progress({bytes: bytes + current, total, completed, count: files.length, path});
  signal?.throwIfAborted();
  if (mode === 'folder') {
    // Call before any asynchronous work consumes the click's user activation.
    const root = await host.showDirectoryPicker({mode: 'readwrite'});
    // Preflight existing destinations before creating anything. Never overwrite
    // user files, including on a retry after a partially completed export.
    for (const entry of files) {
      signal?.throwIfAborted();
      try {
        const parent = await parentDirectory(root, entry.parts, false);
        await rejectExistingFile(parent, entry);
      } catch (error) {
        if (error.name === 'NotFoundError') continue;
        throw error;
      }
    }
    for (const entry of files) {
      signal?.throwIfAborted(); report(entry.path);
      const parent = await parentDirectory(root, entry.parts, true);
      // Earlier files may take minutes to copy. Recheck before each creation.
      await rejectExistingFile(parent, entry);
      const handle = await parent.getFileHandle(entry.parts.at(-1), {create: true});
      const writer = await handle.createWritable();
      try {
        await copyFile(entry.file, source(entry).read, writer, current => report(entry.path, current), signal);
        await writer.close();
      } catch (error) { try { await writer.abort(); } catch {} throw error; }
      bytes += entry.file.length; completed++; report(entry.path);
    }
    return 'saved';
  }

  // Load ZIP code only for the fallback. Store entries without compression and
  // await each add: payload bytes flow to disk, never a whole-archive JS buffer.
  const {ZipWriter, BlobReader} = await import('@zip.js/zip.js/lib/zip-core-custom.js');
  signal?.throwIfAborted();
  const archive = await createArchive(host);
  let output;
  try {
    output = await archive.handle.createWritable();
    const zip = new ZipWriter(output, {level: 0, bufferedWrite: false, useWebWorkers: false, preventClose: true});
    for (const entry of files) {
      signal?.throwIfAborted(); report(entry.path);
      const file = await source(entry).exportFile();
      if (!(file instanceof host.File) || file.size !== entry.file.length) throw Error('Incomplete file export; save canceled');
      signal?.throwIfAborted();
      await zip.add(entry.path, new BlobReader(file), {signal, onprogress: current => report(entry.path, current)});
      bytes += entry.file.length; completed++; report(entry.path);
    }
    signal?.throwIfAborted();
    await zip.close();
    signal?.throwIfAborted();
    await output.close();
    const file = await archive.handle.getFile();
    signal?.throwIfAborted();
    const name = (torrent.torrent_name || 'torrent').replace(/[\\/\x00-\x1f<>:"|?*]/g, '_');
    downloadFile(file, `${name}.zip`, host);
    return 'download_started';
  } catch (error) {
    try { await output?.abort(); } catch {}
    await archive.discard();
    if (error.name === 'QuotaExceededError') throw Error('Not enough browser storage to prepare the ZIP. Save files individually or free browser storage.');
    throw error;
  }
}
