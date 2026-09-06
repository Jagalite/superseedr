// SPDX-License-Identifier: GPL-3.0-or-later
const CHUNK = 1024 * 1024;
export function canSaveFile(file) {
  return Number.isSafeInteger(file.length) && file.length >= 0;
}

// Both sources are admitted by the manager. Saving never mutates OPFS.
export async function saveFile(file, {read, exportFile}, progress = () => {}, host = globalThis) {
  if (!canSaveFile(file)) throw new Error('Invalid file length');
  const name = file.path.split(/[\\/]/).pop() || 'download';
  async function copy(write) {
    for (let offset = 0; offset < file.length; offset += CHUNK) {
      const length = Math.min(CHUNK, file.length - offset);
      const bytes = await read(offset, length);
      if (!(bytes instanceof Uint8Array) || bytes.byteLength !== length) throw new Error('Incomplete file read; save canceled');
      await write(bytes);
      progress(offset + length);
    }
  }
  if (typeof host.showSaveFilePicker === 'function') {
    // Invoke while the click still has transient user activation.
    const handle = await host.showSaveFilePicker({suggestedName: name});
    const writer = await handle.createWritable();
    try { await copy(bytes => writer.write(bytes)); await writer.close(); }
    catch (error) { try { await writer.abort(); } catch {} throw error; }
    return 'saved';
  } else {
    const source = await exportFile();
    if (!(source instanceof host.File) || source.size !== file.length)
      throw new Error('Incomplete file export; save canceled');
    // Keep the source URL alive for the document lifetime: a normal download has
    // no observable completion and a timed revoke can interrupt a slow save.
    const url = host.URL.createObjectURL(source);
    const link = host.document.createElement('a'); link.href = url; link.download = name;
    try { host.document.body.append(link); link.click(); }
    catch (error) { host.URL.revokeObjectURL(url); throw error; }
    finally { link.remove(); }
    return 'download_started';
  }
}
