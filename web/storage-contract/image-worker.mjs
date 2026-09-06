// SPDX-License-Identifier: GPL-3.0-or-later
import init, { Store } from './pkg/superseedr_storage_contract.js';
const ready = init();
let store, namespace, length, closed = false;
let physicalWritten = 0, maxEnd = 0;
const originalWrite = FileSystemSyncAccessHandle.prototype.write;
FileSystemSyncAccessHandle.prototype.write = function(bytes, options) {
  const count = originalWrite.call(this, bytes, options);
  if (physicalWritten === 0) console.log('FIRST_WRITE ' + JSON.stringify({ at: options.at, requested: bytes.byteLength, count, size: this.getSize() }));
  physicalWritten += count; maxEnd = Math.max(maxEnd, options.at + count);
  return count;
};
onmessage = async ({ data: { id, method, args } }) => {
  try {
    await ready;
    let result;
    if (method === 'open') {
      console.log('STORAGE_QUOTA ' + JSON.stringify(await navigator.storage.estimate()));
      const probeFile = await (await navigator.storage.getDirectory()).getFileHandle('raw-probe.bin', {create: true});
      const probe = await probeFile.createSyncAccessHandle();
      const requested = new Uint8Array(4 * 1024 * 1024);
      const count = originalWrite.call(probe, requested, {at: 0});
      console.log('RAW_OPFS ' + JSON.stringify({requested: requested.length, count, size: probe.getSize()}));
      probe.flush(); probe.close();
      [namespace, length] = args;
      store = await Store.open_file(namespace, length);
      if (!await store.allocate()) throw new Error('destination must be fresh');
    } else if (method === 'close') {
      if (!closed) { await store.close(); closed = true; }
    } else if (method === 'export') {
      if (!closed) throw new Error('close before persistent reopen');
      store = await Store.open_file(namespace, length);
      const storedLength = await store.inspect(0);
      const root = await navigator.storage.getDirectory();
      const directory = await (await root.getDirectoryHandle('superseedr-payload-v1')).getDirectoryHandle(namespace);
      const physicalLength = (await (await directory.getFileHandle('file-0')).getFile()).size;
      console.log('STORAGE_SIZE ' + JSON.stringify({ physicalLength, inspected: String(storedLength), expected: length, physicalWritten, maxEnd }));
      if (BigInt(storedLength) !== BigInt(length)) throw new Error(`wrong persisted file length: ${storedLength} (${typeof storedLength}), expected ${length}`);
      for (let offset = 0; offset < length; offset += 4 * 1024 * 1024) {
        const bytes = await store.read(offset, Math.min(4 * 1024 * 1024, length - offset));
        const response = await fetch('/export?offset=' + offset, { method: 'POST', body: bytes });
        if (!response.ok) throw new Error(await response.text());
        await response.arrayBuffer();
      }
      const response = await fetch('/finish');
      if (!response.ok) throw new Error(await response.text());
      result = { ...await response.json(), storage: store.stats(), reopened: true };
      await store.close();
    } else result = await store[method](...args);
    postMessage({ id, result });
  } catch (error) { postMessage({ id, error: String(error) }); }
};
