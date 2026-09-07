// SPDX-License-Identifier: GPL-3.0-or-later
import assert from 'node:assert/strict';
import {expect} from '@playwright/test';
import {readFile} from 'node:fs/promises';
import {createHash} from 'node:crypto';
import {ZipReader, Uint8ArrayReader, Uint8ArrayWriter} from '@zip.js/zip.js/lib/zip-core-custom.js';

export async function runMultiFileSaveContract({browser, peer, origin, trackerUrl, zipNames = false}) {
  const seed = await peer.evaluate(async ({tracker, zipNames}) => {
    const {default: Client} = await import('/independent.js');
    const paths = zipNames ? ['Report.txt', 'report.txt', 'nested/notes:part-one.txt'] : ['notes.bin', 'nested/measurements.bin', 'nested/empty.txt'];
    window.multiFiles = paths.map((path, index) => {
      const bytes = new Uint8Array(index === 2 ? 0 : (index + 1) * 65536 + 37);
      for (let i = 0; i < bytes.length; i++) bytes[i] = (i + index * 19) & 255;
      return new File([bytes], path);
    });
    window.startMultiSeed = () => new Promise((resolve, reject) => {
      window.multiClient = new Client({dht: false, lsd: false, utPex: false, webSeeds: false, tracker: {rtcConfig: {iceServers: []}}});
      window.multiClient.on('error', reject);
      window.multiClient.seed(window.multiFiles, {name: 'orbital-collection', announce: [tracker], pieceLength: 65536}, async t => {
        const files = await Promise.all(t.files.map(async file => ({path: file.path, hash: Array.from(new Uint8Array(
          await crypto.subtle.digest('SHA-256', await file.arrayBuffer())), b => b.toString(16).padStart(2, '0')).join('')})));
        resolve({metadata: Array.from(t.torrentFile), files});
      });
    });
    return window.startMultiSeed();
  }, {tracker: trackerUrl, zipNames});
  await peer.evaluate(() => new Promise(resolve => window.multiClient.destroy(resolve)));
  const ui = await browser.newPage();
  try {
    await ui.goto(origin + '/web/client-dist/webtorrent.html');
    await expect(ui.locator('#status')).not.toHaveText('Starting…');
    await ui.locator('#torrent').setInputFiles({name: 'orbital-collection.torrent', mimeType: 'application/x-bittorrent', buffer: Buffer.from(seed.metadata)});
    const row = ui.locator('.torrent').filter({has: ui.getByRole('heading', {name: 'orbital-collection', exact: true})});
    await expect(row.locator('.file-row')).toHaveCount(3);
    await expect(row.getByRole('button', {name: 'Save all', exact: true})).toBeDisabled();
    await peer.evaluate(() => window.startMultiSeed());
    await expect(row.getByRole('button', {name: 'Save all', exact: true})).toBeEnabled({timeout: 120000});
    let folder = Object.fromEntries(seed.files.map(file => [file.path, file.hash]));
    if (zipNames) {
      await ui.evaluate(() => { window.showDirectoryPicker = () => { throw Error('incompatible folder picker opened'); }; });
      await expect(row.getByRole('button', {name: 'Save all', exact: true})).toHaveAttribute('title', /Download a ZIP/);
      await expect(row.locator('.save-status')).toContainText('Save all creates a ZIP');
    } else {
      await ui.evaluate(async () => {
        window.exportDestination = await (await navigator.storage.getDirectory()).getDirectoryHandle('save-all-engine-destination', {create: true});
        window.showDirectoryPicker = () => Promise.resolve(window.exportDestination);
      });
      await row.getByRole('button', {name: 'Save all', exact: true}).click();
      await expect(row.locator('.save-status')).toContainText('Saved all copies');
      folder = await ui.evaluate(async () => {
        const result = {};
        async function visit(directory, prefix = '') {
          for await (const [name, handle] of directory.entries()) {
            if (handle.kind === 'directory') await visit(handle, prefix + name + '/');
            else result[prefix + name] = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', await (await handle.getFile()).arrayBuffer())), b => b.toString(16).padStart(2, '0')).join('');
          }
        }
        await visit(window.exportDestination); return result;
      });
      assert.deepEqual(folder, Object.fromEntries(seed.files.map(file => [file.path, file.hash])));
      await ui.evaluate(() => { window.showDirectoryPicker = undefined; });
    }
    const completed = ui.waitForEvent('download', {timeout: 30000});
    await row.getByRole('button', {name: 'Save all', exact: true}).click();
    const artifact = await completed;
    const zip = new ZipReader(new Uint8ArrayReader(await readFile(await artifact.path())), {useWebWorkers: false});
    const archived = {};
    for (const entry of await zip.getEntries()) archived[entry.filename] = createHash('sha256').update(await entry.getData(new Uint8ArrayWriter(), {checkSignature: true})).digest('hex');
    await zip.close(); assert.deepEqual(archived, folder);
    await expect(row.locator('.save-status')).toContainText('browser files retained for seeding');
    // Remove the original seeder and fetch every file from the saved browser copy.
    await peer.evaluate(() => new Promise(resolve => window.multiClient.destroy(resolve)));
    const received = await peer.evaluate(async metadata => {
      const {default: Client} = await import('/independent.js');
      window.multiClient = new Client({dht: false, lsd: false, utPex: false, webSeeds: false, tracker: {rtcConfig: {iceServers: []}}});
      return new Promise((resolve, reject) => {
        const timeout = setTimeout(() => reject(Error('multi-file reseed deadline')), 120000);
        window.multiClient.on('error', reject);
        const t = window.multiClient.add(new Uint8Array(metadata));
        t.on('error', reject);
        t.on('done', async () => {
          clearTimeout(timeout);
          resolve(await Promise.all(t.files.map(async file => ({path: file.path, hash: Array.from(new Uint8Array(
            await crypto.subtle.digest('SHA-256', await file.arrayBuffer())), b => b.toString(16).padStart(2, '0')).join('')}))));
        });
      });
    }, seed.metadata);
    assert.deepEqual(received, seed.files);
    ui.once('dialog', dialog => dialog.accept());
    await row.getByRole('button', {name: 'Remove', exact: true}).click();
    await expect(row).toHaveCount(0, {timeout: 30000});
    await ui.getByRole('button', {name: 'Stop client', exact: true}).click();
    await expect(ui.locator('#status')).toHaveText('Stopped');
    console.log('BUILT_PAGE_MULTI_FILE_VERIFICATION_EXPORT_AND_RESEED_PASSED', {zipNames});
  } finally {
    await ui.close();
    await peer.evaluate(() => new Promise(resolve => window.multiClient.destroy(resolve)));
  }
}
