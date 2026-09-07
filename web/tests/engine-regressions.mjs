// SPDX-License-Identifier: GPL-3.0-or-later
import assert from 'node:assert/strict';
import {expect} from '@playwright/test';

export async function runEngineRegressions({page, peer, start, trackerUrl}) {
  // Read the old inline format, migrate to binary records, and prove a failed
  // transaction cannot publish new settings without their metadata.
  await page.evaluate(async () => {
    const {openCatalog, readCatalog, writeCatalog, closeCatalog} = await import('/src/web_integration/session/catalog.js');
    const owner = await openCatalog();
    try {
      const hash = 'ab'.repeat(20);
      const legacy = JSON.stringify({settings: {torrents: []}, metadata: {[hash]: '0001ff'}});
      await new Promise((resolve, reject) => {
        const tx = owner.db.transaction('catalog', 'readwrite');
        tx.objectStore('catalog').put(legacy, 'snapshot');
        tx.oncomplete = resolve; tx.onabort = () => reject(tx.error);
      });
      if (await readCatalog(owner) !== legacy) throw Error('legacy catalog changed');
      await writeCatalog(owner, legacy);
      const bytes = await new Promise(resolve => {
        const request = owner.db.transaction('catalog').objectStore('catalog').get('metadata:' + hash);
        request.onsuccess = () => resolve(request.result);
      });
      if (!(bytes instanceof Uint8Array) || String(bytes) !== '0,1,255') throw Error('metadata is not binary');
      const aborted = {db: {transaction(...args) {
        const tx = owner.db.transaction(...args); queueMicrotask(() => tx.abort()); return tx;
      }}};
      let rejected = false;
      try { await writeCatalog(aborted, JSON.stringify({settings: {torrents: ['changed']}, metadata: {}})); }
      catch { rejected = true; }
      if (!rejected || await readCatalog(owner) !== legacy) throw Error('aborted checkpoint changed durable catalog');
      await writeCatalog(owner, JSON.stringify({settings: {torrents: []}, metadata: {}}));
      if (Object.keys(JSON.parse(await readCatalog(owner)).metadata).length) throw Error('removed metadata retained');
    } finally { closeCatalog(owner); }
  });
  console.log('CATALOG_LEGACY_BINARY_MIGRATION_AND_ATOMIC_ABORT_VERIFIED');

  await start();
  const hashes = [];
  for (let i = 0; i < 10; i++) {
    const name = `archive-${i}.bin`, padding = Buffer.alloc(1800000, 41);
    const metadata = Buffer.concat([Buffer.from(`d4:infod6:lengthi37e4:name${name.length}:${name}7:padding${padding.length}:`), padding, Buffer.from('12:piece lengthi16384e6:pieces20:'), Buffer.alloc(20, 41), Buffer.from('ee')]);
    const hash = await page.evaluate(async encoded => {
      const hash = await window.call('add_torrent', Uint8Array.from(atob(encoded), c => c.charCodeAt(0)));
      await window.call('pause', hash); return hash;
    }, metadata.toString('base64'));
    hashes.push(hash);
    console.log('LARGE_CATALOG_ADDED', i + 1);
  }
  await page.evaluate(async () => { await window.call('shutdown'); window.worker.terminate(); window.closeRtc(); });
  const durable = await page.evaluate(async () => {
    const {openCatalog, readCatalog, closeCatalog} = await import('/src/web_integration/session/catalog.js');
    const owner = await openCatalog();
    try { const c = JSON.parse(await readCatalog(owner)); return {rows: c.settings.torrents.length, metadata: Object.keys(c.metadata).length}; }
    finally { closeCatalog(owner); }
  });
  assert.deepEqual(durable, {rows: 10, metadata: 10});
  await start();
  await page.waitForFunction(() => window.snapshot?.torrents.length === 10 && window.snapshot.torrents.every(t => t.torrent_control_state === 'Paused'));
  assert.equal(await page.evaluate(() => window.snapshot.error), null);
  for (const hash of hashes) await page.evaluate(hash => window.call('remove', hash, true), hash);
  await page.waitForFunction(() => window.snapshot.torrents.length === 0);
  await page.evaluate(async () => { await window.call('shutdown'); window.worker.terminate(); window.closeRtc(); });
  console.log('LARGE_CATALOG_ALL_TEN_TORRENTS_RESTORED_AND_REMOVED');

  // A valid torrent can be incompatible with browser constraints only after a
  // magnet receives its metadata. The host must retain a recoverable stopped row.
  const unsupported = await peer.evaluate(async tracker => {
    const {default: Client} = await import('/independent.js');
    window.failureSeed = new Client({dht: false, lsd: false, utPex: false, webSeeds: false, tracker: {rtcConfig: {iceServers: []}}});
    return new Promise((resolve, reject) => {
      window.failureSeed.on('error', reject);
      window.failureSeed.seed(new File([new Uint8Array(4096)], 'distant-orbit.bin'), {announce: [tracker], pieceLength: 64 * 1024 * 1024}, t => resolve({hash: t.infoHash, magnet: t.magnetURI}));
    });
  }, trackerUrl);
  for (const files of [false, true]) {
    await start();
    await page.evaluate(magnet => window.call('add_magnet', magnet), unsupported.magnet);
    await page.waitForFunction(() => window.snapshot?.torrents.some(t => t.manager_error?.includes('32 MiB')), null, {timeout: 45000});
    assert.equal(await page.evaluate(() => window.snapshot.torrents[0].torrent_control_state), 'Paused');
    await page.evaluate(async () => { await window.call('shutdown'); window.worker.terminate(); window.closeRtc(); });
    await start();
    await page.waitForFunction(() => window.snapshot?.torrents.some(t => t.manager_error?.includes('32 MiB')), null, {timeout: 45000});
    await page.evaluate(hash => window.call('resume', hash), unsupported.hash);
    await page.waitForFunction(() => window.snapshot?.torrents.some(t => !t.manager_error));
    await page.waitForFunction(() => window.snapshot?.torrents.some(t => t.manager_error?.includes('32 MiB')), null, {timeout: 45000});
    await page.evaluate(async ({hash, files}) => { await window.call('remove', hash, files); }, {...unsupported, files});
    await page.waitForFunction(() => window.snapshot.torrents.length === 0);
    await page.evaluate(async () => { await window.call('shutdown'); window.worker.terminate(); window.closeRtc(); });
  }
  if (process.env.SUPERSEEDR_TEST_BUILT_UI === '1') {
    await start();
    await page.evaluate(magnet => window.call('add_magnet', magnet), unsupported.magnet);
    await page.waitForFunction(() => window.snapshot?.torrents.some(t => t.manager_error), null, {timeout: 45000});
    await page.evaluate(async () => { await window.call('shutdown'); window.worker.terminate(); window.closeRtc(); });
    const ui = await peer.context().newPage();
    await ui.goto(new URL('/web/client-dist/webtorrent.html', page.url()).href);
    const row = ui.locator('.torrent');
    await expect(row.getByRole('button', {name: 'Retry', exact: true})).toBeVisible();
    await expect(row.locator('.torrent-details')).toContainText('Stopped:');
    await row.getByRole('button', {name: 'Retry', exact: true}).click();
    await expect(row.getByRole('button', {name: 'Pause', exact: true})).toBeVisible();
    await expect(row.getByRole('button', {name: 'Retry', exact: true})).toBeVisible({timeout: 45000});
    ui.once('dialog', dialog => dialog.accept());
    await row.getByRole('button', {name: 'Remove', exact: true}).click();
    await expect(row).toHaveCount(0);
    await ui.getByRole('button', {name: 'Stop client', exact: true}).click();
    await expect(ui.locator('#status')).toHaveText('Stopped');
    await ui.close();
    console.log('BUILT_PAGE_FAILED_MANAGER_RESTORE_RETRY_REMOVE_VERIFIED');
  }
  await peer.evaluate(() => new Promise(resolve => window.failureSeed.destroy(resolve)));
  console.log('FAILED_MANAGER_RELOAD_RETRY_KEEP_AND_DELETE_REMOVAL_VERIFIED');

  await page.evaluate(async () => {
    const {openPayload, submitPayload, removeClosedPayload} = await import('/src/persistence/payload/opfs.js');
    const namespace = 'v1-' + 'cd'.repeat(20);
    const layout = {files: [{path: 'payload/quiet-orbit.bin', length: 0, global_start_offset: 0, is_padding: false}], total_size: 0};
    const owner = await openPayload(namespace, JSON.stringify(layout), false);
    let rejected = false;
    try { await removeClosedPayload(namespace); } catch { rejected = true; }
    if (!rejected) throw Error('recovery deleted owned payload');
    await submitPayload(owner, JSON.stringify({kind: 'close'}), new Uint8Array());
    await removeClosedPayload(namespace);
    await removeClosedPayload(namespace);
    const root = await (await navigator.storage.getDirectory()).getDirectoryHandle('superseedr-payload-v1');
    try { await root.getDirectoryHandle(namespace); throw Error('recovery retained payload'); }
    catch (error) { if (error.name !== 'NotFoundError') throw error; }
  });
  console.log('FAILED_PAYLOAD_REMOVAL_RESPECTS_OWNERSHIP_AND_IS_IDEMPOTENT');
  // Seed the durable crash boundary: deletion accepted, payload still present.
  // Exercise delete, keep-data, and cleanup blocked by another physical owner.
  await page.evaluate(async () => {
    const {openCatalog, readCatalog, writeCatalog, closeCatalog} = await import('/src/web_integration/session/catalog.js');
    const {openPayload, submitPayload} = await import('/src/persistence/payload/opfs.js');
    const owner = await openCatalog();
    try {
      const catalog = JSON.parse(await readCatalog(owner));
      catalog.settings.torrents = [];
      for (const [tag, deleteFiles] of [['a1', true], ['a2', false], ['a3', true]]) {
        const hash = tag.repeat(20);
        catalog.settings.torrents.push({torrent_or_magnet: 'magnet:?xt=urn:btih:' + hash,
          name: 'Quiet archive', download_path: 'payload', torrent_control_state: 'Deleting',
          delete_files: deleteFiles, validation_status: false, file_priorities: {}});
        const layout = {files: [{path: 'payload/quiet.bin', length: 4, global_start_offset: 0, is_padding: false, is_skipped: false}], total_size: 4};
        const store = await openPayload('v1-' + hash, JSON.stringify(layout), false);
        await submitPayload(store, JSON.stringify({kind: 'write', spans: [{index: 0, local: 0, position: 0, length: 4, padding: false, skipped: false}]}), new Uint8Array([1, 2, 3, 4]));
        if (tag === 'a3') window.recoveryOwner = store;
        else await submitPayload(store, JSON.stringify({kind: 'close'}), new Uint8Array());
      }
      await writeCatalog(owner, JSON.stringify(catalog));
    } finally { closeCatalog(owner); }
  });
  await start();
  await page.waitForFunction(() => window.snapshot?.torrents.length === 1 && window.snapshot.torrents[0].manager_error?.includes('interrupted deletion'));
  await page.evaluate(async () => { await window.call('shutdown'); window.worker.terminate(); window.closeRtc(); });
  const recovered = await page.evaluate(async () => {
    const {openCatalog, readCatalog, closeCatalog} = await import('/src/web_integration/session/catalog.js');
    const owner = await openCatalog();
    let rows;
    try { rows = JSON.parse(await readCatalog(owner)).settings.torrents; }
    finally { closeCatalog(owner); }
    const root = await (await navigator.storage.getDirectory()).getDirectoryHandle('superseedr-payload-v1');
    try { await root.getDirectoryHandle('v1-' + 'a1'.repeat(20)); throw Error('interrupted deletion retained payload'); }
    catch (error) { if (error.name !== 'NotFoundError') throw error; }
    const kept = await root.getDirectoryHandle('v1-' + 'a2'.repeat(20));
    const bytes = [...new Uint8Array(await (await (await kept.getFileHandle('file-0')).getFile()).arrayBuffer())];
    return {rows: rows.map(row => row.torrent_or_magnet), bytes};
  });
  assert.deepEqual(recovered, {rows: ['magnet:?xt=urn:btih:' + 'a3'.repeat(20)], bytes: [1, 2, 3, 4]});
  await start();
  await page.waitForFunction(() => window.snapshot?.torrents.some(t => t.manager_error));
  await page.evaluate(async () => {
    const {submitPayload, removeClosedPayload} = await import('/src/persistence/payload/opfs.js');
    await submitPayload(window.recoveryOwner, JSON.stringify({kind: 'close'}), new Uint8Array());
    await window.call('remove', 'a3'.repeat(20), true);
    await removeClosedPayload('v1-' + 'a2'.repeat(20));
  });
  await page.waitForFunction(() => window.snapshot.torrents.length === 0);
  await page.evaluate(async () => { await window.call('shutdown'); window.worker.terminate(); window.closeRtc(); });
  console.log('INTERRUPTED_DELETION_DELETE_KEEP_LOCK_FAILURE_AND_RELOAD_RETRY_VERIFIED');

}
