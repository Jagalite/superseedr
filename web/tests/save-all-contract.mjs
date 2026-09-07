// SPDX-License-Identifier: GPL-3.0-or-later
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {File} from 'node:buffer';
import {randomUUID} from 'node:crypto';
import {ZipReader, BlobReader, Uint8ArrayWriter} from '@zip.js/zip.js/lib/zip-core-custom.js';
import {saveAll, planSaveAll} from '../src/save-all.js';
import {cleanupArchives} from '../src/save-archive.js';

const missing = () => new DOMException('Missing entry', 'NotFoundError');
function directory(events = [], path = '') {
  const children = new Map();
  return {kind: 'directory', children, async *entries() { yield* children; },
    async getDirectoryHandle(name, {create = false} = {}) {
      if (!children.has(name)) { if (!create) throw missing(); children.set(name, directory(events, path + name + '/')); }
      const child = children.get(name);
      if (child.kind !== 'directory') throw new DOMException('Not a directory', 'TypeMismatchError');
      return child;
    },
    async getFileHandle(name, {create = false} = {}) {
      if (!children.has(name)) {
        if (!create) throw missing();
        const child = {kind: 'file', data: new Uint8Array(), async getFile() { return new File([child.data], name); },
          async createWritable() {
            const chunks = [];
            const stream = new WritableStream({
              async write(bytes) { await new Promise(resolve => setImmediate(resolve)); chunks.push(bytes.slice()); events.push(['write', path + name, bytes.length]); },
              close() { child.data = Buffer.concat(chunks); events.push(['close', path + name]); },
              abort() { events.push(['abort', path + name]); },
            });
            stream.write = bytes => { const writer = stream.getWriter(); return writer.write(bytes).finally(() => writer.releaseLock()); };
            return stream;
          }};
        children.set(name, child);
      }
      const child = children.get(name);
      if (child.kind !== 'file') throw new DOMException('Not a file', 'TypeMismatchError');
      return child;
    },
    async removeEntry(name) { if (!children.delete(name)) throw missing(); },
  };
}
function torrent(files) {
  return {torrent_name: 'orbital-collection', files: files.map(file => ({...file, path: 'payload/' + file.path, length: file.length ?? 4})),
    file_verified_bytes: files.map(file => file.length ?? 4)};
}
function archiveHost() {
  const root = directory(), locks = new Set(), downloads = [];
  const host = {File, crypto: {randomUUID}, navigator: {
    storage: {getDirectory: async () => root}, locks: {async request(name, options, callback) {
      if (!callback) { callback = options; options = {}; }
      if (locks.has(name)) { assert.ok(options.ifAvailable); return callback(null); }
      locks.add(name);
      try { return await callback({name}); } finally { locks.delete(name); }
    }},
  }, URL: {createObjectURL(file) { downloads.push(file); return 'blob:archive'; }, revokeObjectURL() {}},
  document: {body: {append() {}}, createElement: () => ({click() {}, remove() {}})}};
  return {host, root, locks, downloads};
}

test('plan preserves file indices and nested paths while excluding padding and skipped files', () => {
  const input = torrent([{path: 'collection/readme.txt'}, {path: 'pad', is_padding: true},
    {path: 'omitted.bin', is_skipped: true}, {path: 'collection/nested/empty.txt', length: 0}]);
  input.file_verified_bytes[1] = input.file_verified_bytes[2] = 0;
  const plan = planSaveAll(input);
  assert.deepEqual(plan.files.map(({index, path}) => [index, path]), [[0, 'collection/readme.txt'], [3, 'collection/nested/empty.txt']]);
  assert.equal(plan.total, 4);
});

test('unverified, invalid, unsafe and conflicting manifests fail before opening a picker', async () => {
  const cases = [torrent([]), torrent([{path: 'x', length: -1}]), torrent([{path: 'x', length: Number.MAX_SAFE_INTEGER + 1}])];
  const incomplete = torrent([{path: 'x'}]); incomplete.file_verified_bytes = [3]; cases.push(incomplete);
  for (const path of ['/absolute', '../escape', 'a/../b', 'a//b', 'C:/x', 'a\\b', 'x/..']) cases.push(torrent([{path}]));
  for (const paths of [['x', 'x'], ['x', 'x/y'], ['x/y', 'x']]) cases.push(torrent(paths.map(path => ({path}))));
  for (const input of cases) await assert.rejects(saveAll(input, () => assert.fail('unexpected source'), {}, {
    showDirectoryPicker: () => assert.fail('unexpected picker'),
  }));
});

test('one synchronous picker preserves nested files, empty files and bounded sequential writes', async () => {
  const events = [], root = directory(events); let picked = false, position = 0, last;
  const input = torrent([{path: 'collection/large.bin', length: 2 * 1024 * 1024 + 7}, {path: 'collection/nested/empty.txt', length: 0}]);
  const result = saveAll(input, ({index}) => ({read: async (offset, length) => {
    assert.equal(index, 0); assert.equal(offset, position); assert.ok(length <= 1024 * 1024);
    assert.equal(events.filter(event => event[0] === 'write').reduce((sum, event) => sum + event[2], 0), offset);
    position += length; return new Uint8Array(length).fill(17);
  }}), {progress: value => { last = value; }}, {showDirectoryPicker: options => {
    picked = true; assert.equal(options.mode, 'readwrite'); return Promise.resolve(root);
  }});
  assert.ok(picked, 'picker must run before returning to the event loop');
  assert.equal(await result, 'saved');
  assert.deepEqual(events.filter(event => event[0] === 'close').map(event => event[1]), ['collection/large.bin', 'collection/nested/empty.txt']);
  assert.equal(last.completed, 2); assert.equal(last.bytes, input.files[0].length);
  assert.equal((await (await root.getDirectoryHandle('collection')).getFileHandle('large.bin')).data.length, position);
});

test('all destination paths are checked before any writes; existing content is retained', async () => {
  const events = [], root = directory(events);
  const existing = await root.getFileHandle('second.bin', {create: true}); existing.data = new Uint8Array([99]);
  await assert.rejects(saveAll(torrent([{path: 'first.bin'}, {path: 'second.bin'}]), () => assert.fail('unexpected read'), {},
    {showDirectoryPicker: async () => root}), /already contains/);
  assert.equal(root.children.has('first.bin'), false); assert.deepEqual([...existing.data], [99]); assert.deepEqual(events, []);
});

test('cancellation preserves completed files and aborts the current destination', async () => {
  const events = [], root = directory(events), controller = new AbortController();
  await assert.rejects(saveAll(torrent([{path: 'first.bin'}, {path: 'second.bin'}]), ({index}) => ({read: async () => {
    if (index === 1) controller.abort(); return new Uint8Array(4);
  }}), {signal: controller.signal}, {showDirectoryPicker: async () => root}), {name: 'AbortError'});
  assert.deepEqual(events.map(event => event.slice(0, 2)), [['write', 'first.bin'], ['close', 'first.bin'], ['abort', 'second.bin']]);
  assert.equal(root.children.get('first.bin').data.length, 4);
});

test('a short verified read aborts the current file and never starts the next', async () => {
  const events = [], root = directory(events);
  await assert.rejects(saveAll(torrent([{path: 'first.bin'}, {path: 'second.bin'}]), () => ({read: async () => new Uint8Array(3)}), {},
    {showDirectoryPicker: async () => root}), /Incomplete file read/);
  assert.deepEqual(events, [['abort', 'first.bin']]); assert.equal(root.children.has('second.bin'), false);
});

test('ZIP fallback preserves names and bytes with retained sources and active archive ownership', async () => {
  const {host, root, locks, downloads} = archiveHost();
  host.showDirectoryPicker = true; // callable feature detection
  const input = torrent([{path: 'collection/first.bin'}, {path: 'collection/nested/empty.txt', length: 0}]);
  const sources = [new File([new Uint8Array([1, 2, 3, 4])], 'first.bin'), new File([], 'empty.txt')];
  assert.equal(await saveAll(input, ({index}) => ({exportFile: async () => sources[index]}), {}, host), 'download_started');
  const zip = new ZipReader(new BlobReader(downloads[0]), {useWebWorkers: false});
  const entries = await zip.getEntries();
  assert.deepEqual(entries.map(entry => entry.filename), ['collection/first.bin', 'collection/nested/empty.txt']);
  assert.deepEqual([...await entries[0].getData(new Uint8ArrayWriter(), {checkSignature: true})], [1, 2, 3, 4]);
  assert.equal((await entries[1].getData(new Uint8ArrayWriter())).length, 0); await zip.close();
  assert.equal(await sources[0].text(), '\x01\x02\x03\x04');
  const directory = await root.getDirectoryHandle('superseedr-exports-v1');
  await cleanupArchives(host); assert.equal(directory.children.size, 1, 'active download must survive cleanup');
  locks.clear(); // document lifetime ends
  const unrelated = await directory.getFileHandle('unrelated', {create: true});
  await cleanupArchives(host); assert.equal(directory.children.size, 1); assert.equal(directory.children.get('unrelated'), unrelated);
});

test('failed, canceled or short ZIP source exports discard scratch files without downloading', async () => {
  for (const mode of ['reject', 'short', 'cancel', 'handoff']) {
    const {host, root, locks, downloads} = archiveHost(); const controller = new AbortController();
    if (mode === 'handoff') host.document.createElement = () => ({click() { throw Error('handoff failed'); }, remove() {}});
    await assert.rejects(saveAll(torrent([{path: 'sample.bin'}]), () => ({exportFile: async () => {
      if (mode === 'reject') throw Error('range no longer verified');
      if (mode === 'cancel') controller.abort();
      return new File([new Uint8Array(mode === 'short' ? 3 : 4)], 'sample.bin');
    }}), {signal: controller.signal}, host));
    assert.equal((await root.getDirectoryHandle('superseedr-exports-v1')).children.size, 0);
    assert.equal(locks.size, 0); assert.equal(downloads.length, mode === 'handoff' ? 1 : 0);
  }
});


test('ZIP output quota failure removes the unfinished archive and releases ownership', async () => {
  const {host, root, locks, downloads} = archiveHost();
  const scratch = await root.getDirectoryHandle('superseedr-exports-v1', {create: true});
  const get = scratch.getFileHandle.bind(scratch);
  scratch.getFileHandle = async (...args) => {
    const handle = await get(...args);
    handle.createWritable = async () => new WritableStream({write() { throw new DOMException('Full', 'QuotaExceededError'); }});
    return handle;
  };
  await assert.rejects(saveAll(torrent([{path: 'sample.bin'}]), () => ({exportFile: async () => new File([new Uint8Array(4)], 'sample.bin')}), {}, host), /Not enough browser storage/);
  assert.equal(scratch.children.size, 0); assert.equal(locks.size, 0); assert.equal(downloads.length, 0);
});


test('export removes only the engine-generated root suffix for this torrent', () => {
  const input = torrent([{path: `collection [${'ab'.repeat(20)}]/nested/item.bin`}]);
  input.torrent_name = 'collection'; input.info_hash = Array(20).fill(0xab);
  assert.equal(planSaveAll(input).files[0].path, 'collection/nested/item.bin');
  input.info_hash[0] = 0xcd;
  assert.equal(planSaveAll(input).files[0].path, `collection [${'ab'.repeat(20)}]/nested/item.bin`);
});


test('a destination appearing after preflight is retained instead of overwritten', async () => {
  const events = [], root = directory(events);
  await assert.rejects(saveAll(torrent([{path: 'first.bin'}, {path: 'second.bin'}]), () => ({read: async () => {
    const external = await root.getFileHandle('second.bin', {create: true}); external.data = new Uint8Array([99]);
    return new Uint8Array(4);
  }}), {}, {showDirectoryPicker: async () => root}), /already contains/);
  assert.deepEqual([...root.children.get('second.bin').data], [99]);
  assert.deepEqual(events.filter(event => event[0] === 'close'), [['close', 'first.bin']]);
});


test('portable folder restrictions select ZIP without disabling or renaming verified files', async () => {
  for (const names of [
    ['collection/Report.txt', 'collection/report.txt'],
    ['collection/notes:part-one.txt', 'collection/NUL.bin', 'collection/trailing.'],
    ['collection/café.txt', 'collection/cafe\u0301.txt'],
    ['collection/Upper/one.bin', 'collection/upper/two.bin'],
  ]) {
    const {host, downloads} = archiveHost();
    host.showDirectoryPicker = () => assert.fail('incompatible folder picker must not open');
    const input = torrent(names.map(path => ({path})));
    assert.equal(planSaveAll(input, host).mode, 'zip');
    assert.equal(await saveAll(input, ({index}) => ({exportFile: async () => new File([new Uint8Array(4).fill(index + 1)], 'source.bin')}), {}, host), 'download_started');
    const zip = new ZipReader(new BlobReader(downloads[0]), {useWebWorkers: false});
    const entries = await zip.getEntries();
    assert.deepEqual(entries.map(entry => entry.filename), names);
    for (const [index, entry] of entries.entries()) assert.deepEqual([...await entry.getData(new Uint8ArrayWriter(), {checkSignature: true})], Array(4).fill(index + 1));
    await zip.close();
  }
});
