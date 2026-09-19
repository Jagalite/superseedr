// SPDX-License-Identifier: GPL-3.0-or-later
// Real browser filesystem handles, ZIP download and ownership across documents.
import assert from 'node:assert/strict';
import {execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {mkdtemp, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {chromium, firefox, webkit, expect} from '@playwright/test';
import {createServer} from 'vite';
const engine = process.env.SUPERSEEDR_TEST_BROWSER || 'chromium';
const server = await createServer({configFile: 'vite.webtorrent.config.ts', logLevel: 'error', server: {host: '127.0.0.1', port: 0}});
await server.listen();
const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
const profile = await mkdtemp(join(tmpdir(), 'ss-save-all-'));
const context = await ({chromium, firefox, webkit}[engine]).launchPersistentContext(profile, {headless: true, acceptDownloads: true});
await context.route('**/save-contract.html', route => route.fulfill({contentType: 'text/html', body: '<!doctype html><title>Save contract</title>'}));
const errors = [];
try {
  const page = await context.newPage();
  page.on('pageerror', error => errors.push(String(error)));
  await page.goto(origin + '/save-contract.html');
  // Fixture sources and destinations exercise browser export APIs independently
  // of the full engine, which has its own built-page multi-file contract.
  const lengths = [Number(process.env.SUPERSEEDR_TEST_EXPORT_BYTES || 65 * 1024 * 1024 + 37), 256 * 1024 + 1, 0];
  await page.evaluate(async lengths => {
    const root = await navigator.storage.getDirectory();
    const fixture = await root.getDirectoryHandle('save-all-fixture', {create: true});
    window.sources = [];
    for (const [index, length] of lengths.entries()) {
      const handle = await fixture.getFileHandle(`source-${index}`, {create: true});
      const writer = await handle.createWritable();
      for (let at = 0; at < length; at += 1024 * 1024) {
        const bytes = new Uint8Array(Math.min(1024 * 1024, length - at));
        for (let i = 0; i < bytes.length; i++) bytes[i] = (at + i + index * 17) & 255;
        await writer.write(bytes);
      }
      await writer.close(); window.sources.push(handle);
    }
    const paths = ['collection/first.bin', 'collection/nested/second.bin', 'collection/nested/empty.txt'];
    window.input = {torrent_name: 'orbital-collection', files: paths.map((path, i) => ({path: 'payload/' + path, length: lengths[i]})), file_verified_bytes: lengths};
    window.source = ({index}) => ({
      read: async (offset, length) => {
        if (length > 1024 * 1024) throw Error('unbounded read');
        return new Uint8Array(await (await window.sources[index].getFile()).slice(offset, offset + length).arrayBuffer());
      }, exportFile: () => window.sources[index].getFile(),
    });
    window.destination = await fixture.getDirectoryHandle('destination', {create: true});
    window.showDirectoryPicker = async options => {
      if (!navigator.userActivation.isActive || options.mode !== 'readwrite') throw Error('picker lost user activation');
      return window.destination;
    };
    const {saveAll} = await import('/src/save-all.js');
    const button = document.createElement('button'); button.textContent = 'Export fixture';
    button.onclick = () => { window.outcome = saveAll(window.input, window.source); };
    document.body.append(button);
  }, lengths);
  await page.getByRole('button', {name: 'Export fixture', exact: true}).click();
  assert.equal(await page.evaluate(() => window.outcome), 'saved');
  await page.evaluate(async () => {
    for (const [index, item] of window.input.files.entries()) {
      let parent = window.destination;
      const parts = item.path.replace(/^payload\//, '').split('/');
      for (const part of parts.slice(0, -1)) parent = await parent.getDirectoryHandle(part);
      const file = await (await parent.getFileHandle(parts.at(-1))).getFile();
      if (file.size !== item.length) throw Error('folder size mismatch');
      let offset = 0;
      for await (const bytes of file.stream()) {
        for (let i = 0; i < bytes.length; i++) if (bytes[i] !== ((offset + i + index * 17) & 255)) throw Error('folder byte mismatch');
        offset += bytes.length;
      }
    }
    window.showDirectoryPicker = undefined;
  });
  const downloadEvent = page.waitForEvent('download', {timeout: 120000});
  await page.getByRole('button', {name: 'Export fixture', exact: true}).click();
  const download = await downloadEvent;
  assert.equal(await page.evaluate(() => window.outcome), 'download_started');
  assert.equal(download.suggestedFilename(), 'orbital-collection.zip');
  // Independent ZIP implementation checks CRCs, paths and full contents.
  const actual = JSON.parse(execFileSync('python3', ['-c', `import hashlib,json,sys,zipfile
with zipfile.ZipFile(sys.argv[1]) as z:
    result = {}
    for name in z.namelist():
        h = hashlib.sha256()
        with z.open(name) as f:
            while chunk := f.read(1024 * 1024): h.update(chunk)
        result[name] = h.hexdigest()
    print(json.dumps(result))`, await download.path()], {encoding: 'utf8'}));
  for (const [index, name] of ['collection/first.bin', 'collection/nested/second.bin', 'collection/nested/empty.txt'].entries()) {
    const hash = createHash('sha256');
    for (let at = 0; at < lengths[index]; at += 1024 * 1024) {
      const bytes = Buffer.alloc(Math.min(1024 * 1024, lengths[index] - at));
      for (let i = 0; i < bytes.length; i++) bytes[i] = (at + i + index * 17) & 255;
      hash.update(bytes);
    }
    assert.equal(actual[name], hash.digest('hex'));
  }
  await page.evaluate(async () => {
    const {saveAll} = await import('/src/save-all.js');
    const controller = new AbortController();
    let canceled = false;
    try {
      await saveAll(window.input, window.source, {signal: controller.signal, progress: ({bytes}) => {
        if (bytes > 0) controller.abort();
      }});
    } catch (error) { if (error.name !== 'AbortError') throw error; canceled = true; }
    if (!canceled) throw Error('ZIP cancellation did not stop the write');
  });
  const other = await context.newPage(); await other.goto(origin + '/save-contract.html');
  const remaining = () => other.evaluate(async () => {
    const {cleanupArchives} = await import('/src/save-archive.js'); await cleanupArchives();
    const root = await navigator.storage.getDirectory();
    const directory = await root.getDirectoryHandle('superseedr-exports-v1');
    let count = 0; for await (const _ of directory.values()) count++; return count;
  });
  assert.equal(await remaining(), 1, 'another document cannot clean an active download');
  await page.close();
  await expect.poll(remaining, {message: 'closed documents release export scratch files'}).toBe(0);
  await other.evaluate(async lengths => {
    const fixture = await (await navigator.storage.getDirectory()).getDirectoryHandle('save-all-fixture');
    for (const [index, length] of lengths.entries()) {
      const file = await (await fixture.getFileHandle(`source-${index}`)).getFile();
      if (file.size !== length) throw Error('source removed or changed');
      const bytes = new Uint8Array(await file.slice(0, 37).arrayBuffer());
      if (!bytes.every((byte, i) => byte === ((i + index * 17) & 255))) throw Error('source bytes changed');
    }
  }, lengths);
  assert.deepEqual(errors, []);
  console.log('SAVE_ALL_FOLDER_ZIP_CONTENTS_RETAINED_SOURCES_AND_CLEANUP_PASSED', {engine, lengths});
} finally { await context.close(); await server.close(); await rm(profile, {recursive: true, force: true}); }
