// SPDX-License-Identifier: GPL-3.0-or-later
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {saveFile, canSaveFile} from '../src/save-file.js';

test('large saves await each write and use bounded reads without removing browser data', async () => {
  let position = 0, closed = false, writes = 0;
  const file = {path: 'collection/sample.bin', length: 70 * 1024 * 1024 + 37};
  const host = {showSaveFilePicker: async options => {
    assert.equal(options.suggestedName, 'sample.bin');
    return {createWritable: async () => ({write: async bytes => {
      await new Promise(resolve => setImmediate(resolve)); position += bytes.length; writes++;
    }, close: async () => { closed = true; }, abort: async () => assert.fail('unexpected abort')})};
  }};
  await saveFile(file, {read: async (offset, length) => {
    assert.equal(offset, position, 'the previous disk write must finish before another read');
    assert.ok(length <= 1024 * 1024); return new Uint8Array(length);
  }}, () => {}, host);
  assert.equal(position, file.length); assert.equal(writes, 71); assert.equal(closed, true);
});

test('short or rejected reads abort the destination without closing a partial save', async () => {
  for (const read of [async () => new Uint8Array(3), async () => { throw Error('range no longer verified'); }]) {
    let aborted = false;
    const host = {showSaveFilePicker: async () => ({createWritable: async () => ({
      write: async () => assert.fail('unexpected write'), close: async () => assert.fail('unexpected close'), abort: async () => { aborted = true; },
    })})};
    await assert.rejects(saveFile({path: 'sample.bin', length: 4}, {read}, undefined, host));
    assert.equal(aborted, true);
  }
});

test('picker cancellation reads nothing; empty files close without a zero-length engine read', async () => {
  const cancel = Object.assign(Error('Canceled'), {name: 'AbortError'});
  await assert.rejects(saveFile({path: 'sample.bin', length: 4}, {read: () => assert.fail('unexpected read')}, undefined,
    {showSaveFilePicker: async () => { throw cancel; }}), error => error === cancel);
  let closed = false;
  await saveFile({path: 'empty.bin', length: 0}, {read: () => assert.fail('unexpected read')}, undefined,
    {showSaveFilePicker: async () => ({createWritable: async () => ({close: async () => { closed = true; }})})});
  assert.equal(closed, true);
});

test('invalid lengths fail before requesting an export', async () => {
  for (const length of [Number.MAX_SAFE_INTEGER + 1, -1, NaN]) {
    const file = {path: 'sample.bin', length};
    assert.equal(canSaveFile(file), false);
    await assert.rejects(saveFile(file, {exportFile: () => assert.fail('unexpected export')}, undefined, {}));
  }
});

test('fallback hands a file-backed source to the browser at any supported size without reads or timers', async () => {
  for (const size of [0, 65 * 1024 * 1024, 2 * 1024 ** 3]) {
    class BackedFile { constructor() { this.size = size; } }
    const source = new BackedFile(); let clicked = false, removed = false;
    const link = {click() { clicked = true; }, remove() { removed = true; }};
    const host = {File: BackedFile, URL: {
      createObjectURL(file) { assert.equal(file, source); return 'blob:retained'; },
      revokeObjectURL() { assert.fail('download may still be reading'); },
    }, document: {createElement: () => link, body: {append() {}}},
    setTimeout() { assert.fail('download has no completion deadline'); }};
    assert.equal(canSaveFile({length: size}), true);
    assert.equal(await saveFile({path: 'folder/orbital.bin', length: size}, {
      read: () => assert.fail('fallback must not read bytes into JavaScript'), exportFile: async () => source,
    }, undefined, host), 'download_started');
    assert.equal(link.download, 'orbital.bin'); assert.ok(clicked && removed);
  }
});

test('failed export, short file and failed handoff never report success', async () => {
  class BackedFile { constructor(size) { this.size = size; } }
  for (const exportFile of [async () => { throw Error('client stopped'); }, async () => new BackedFile(3), async () => new Uint8Array(4)]) {
    await assert.rejects(saveFile({path: 'orbital.bin', length: 4}, {exportFile}, undefined, {File: BackedFile,
      URL: {createObjectURL() { assert.fail('unexpected URL'); }}}));
  }
  let revoked = false, removed = false;
  await assert.rejects(saveFile({path: 'orbital.bin', length: 4}, {exportFile: async () => new BackedFile(4)}, undefined, {
    File: BackedFile, URL: {createObjectURL: () => 'blob:failed', revokeObjectURL() { revoked = true; }},
    document: {body: {append() {}}, createElement: () => ({click() { throw Error('handoff failed'); }, remove() { removed = true; }})},
  }), /handoff failed/);
  assert.ok(revoked && removed);
});
