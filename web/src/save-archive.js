// SPDX-License-Identifier: GPL-3.0-or-later
// Export scratch space is separate from the engine-owned torrent payloads.
const DIRECTORY = 'superseedr-exports-v1';
const archiveName = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}\.zip$/;
const lockName = name => `${DIRECTORY}/${name}`;

export async function cleanupArchives(host = globalThis) {
  const root = await host.navigator.storage.getDirectory();
  let directory;
  try { directory = await root.getDirectoryHandle(DIRECTORY); }
  catch (error) { if (error.name === 'NotFoundError') return; throw error; }
  for await (const [name, handle] of directory.entries()) {
    if (handle.kind !== 'file' || !archiveName.test(name)) continue;
    // A different open page may still be preparing or downloading this archive.
    await host.navigator.locks.request(lockName(name), {ifAvailable: true}, async lock => {
      if (lock) {
        try { await directory.removeEntry(name); }
        catch (error) { if (error.name !== 'NotFoundError') throw error; }
      }
    });
  }
}

export function createArchive(host = globalThis) {
  const name = `${host.crypto.randomUUID()}.zip`;
  // Keep a successful export locked for this document's lifetime. There is no
  // browser download-complete API; the next page cleans it after this one closes.
  return new Promise((resolve, reject) => {
    host.navigator.locks.request(lockName(name), async () => {
      const root = await host.navigator.storage.getDirectory();
      const directory = await root.getDirectoryHandle(DIRECTORY, {create: true});
      const handle = await directory.getFileHandle(name, {create: true});
      await new Promise(release => resolve({handle, async discard() {
        try { await directory.removeEntry(name); } finally { release(); }
      }}));
    }).catch(reject);
  });
}
