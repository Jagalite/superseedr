// SPDX-License-Identifier: GPL-3.0-or-later
// Opt-in external input: independent protocol client, production OPFS through Wasm.
import { chromium } from '@playwright/test';
import { createServer } from 'node:http';
import { readFile, open } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
const config = JSON.parse(await readFile(process.argv[2], 'utf8'));
const storageRoot = resolve(fileURLToPath(new URL('../storage-contract/', import.meta.url)));
const info = Buffer.from(config.info, 'hex');
const tracker = Buffer.from(config.tracker);
const metainfo = Buffer.concat([Buffer.from(`d8:announce${tracker.length}:`), tracker, Buffer.from('4:info'), info, Buffer.from('e')]);
let exported = 0, destination, exportHash = createHash('sha256');
if (config.mode !== 'seed') destination = await open(config.export, 'wx');
const server = createServer(async (request, response) => {
  try {
    const url = new URL(request.url, 'http://localhost');
    if (url.pathname === '/export' && request.method === 'POST') {
      if (Number(url.searchParams.get('offset')) !== exported) throw new Error('out-of-order export');
      for await (const bytes of request) {
        let at = 0;
        while (at < bytes.length) { const result = await destination.write(bytes, at); if (!result.bytesWritten) throw new Error('short export'); at += result.bytesWritten; }
        exportHash.update(bytes); exported += bytes.length;
      }
      response.end(); return;
    }
    if (url.pathname === '/finish') {
      await destination.sync(); await destination.close(); destination = null;
      const digest = exportHash.digest('hex');
      if (digest !== config.sha256 || exported !== config.length) throw new Error(`export mismatch: ${digest} ${exported}`);
      response.end(JSON.stringify({ sha256: digest, bytes: exported })); return;
    }
    let bytes;
    if (url.pathname === '/') bytes = Buffer.from('<!doctype html><title>External image acceptance</title><input type="file">');
    else if (url.pathname === '/client.js') bytes = await readFile(config.client_dist);
    else if (url.pathname === '/torrent') bytes = metainfo;
    else if (url.pathname.startsWith('/storage/')) {
      const path = resolve(storageRoot, url.pathname.slice('/storage/'.length));
      if (!path.startsWith(storageRoot + sep)) throw new Error('invalid static path');
      bytes = await readFile(path);
    } else { response.writeHead(404).end(); return; }
    response.setHeader('Content-Type', url.pathname.endsWith('.wasm') ? 'application/wasm' : /\.(m?js)$/.test(url.pathname) ? 'text/javascript' : 'text/html');
    response.end(bytes);
  } catch (error) { console.error(error); response.writeHead(500).end(String(error)); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const browser = config.browser_profile
  ? await chromium.launchPersistentContext(config.browser_profile, { headless: true })
  : await chromium.launch({ headless: true });
let storageFinished;
const storageCompletion = new Promise(resolve => { storageFinished = resolve; });
let failed;
const failure = new Promise((_, reject) => { failed = reject; });
try {
  const page = await browser.newPage();
  page.on('console', message => console.log(message.text()));
  page.on('pageerror', error => { console.error('PAGE_ERROR', error); failed(error); });
  page.on('crash', () => console.error('PAGE_CRASH'));
  page.on('requestfailed', request => console.error('REQUEST_FAILED', request.url(), request.failure()));
  await page.exposeFunction('storageFinished', storageFinished);
  await page.exposeFunction('acceptanceFailed', message => failed(new Error(message)));
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  if (config.mode !== 'sink') await page.locator('input').setInputFiles(config.source);
  await page.evaluate(config => {
    window.acceptanceSetup = (async () => {
    localStorage.debug = 'webtorrent:peer,simple-peer,bittorrent-tracker:*';
    console.log('LOADING_CLIENT');
    const { default: WebTorrent } = await import('/client.js');
    console.log('CLIENT_LOADED');
    const source = document.querySelector('input').files[0];
    const worker = new Worker('/storage/image-worker.mjs', { type: 'module' });
    let serial = 0;
    const pending = new Map();
    worker.onmessage = ({ data }) => { const p = pending.get(data.id); if (p) { pending.delete(data.id); data.error ? p.reject(new Error(data.error)) : p.resolve(data.result); } };
    worker.onerror = event => window.acceptanceFailed(event.message);
    const call = (method, ...args) => new Promise((resolve, reject) => { const id = ++serial; pending.set(id, { resolve, reject }); worker.postMessage({ id, method, args }); });
    if (config.mode === 'storage') {
      await call('open', 'v1-' + config.hash, config.length);
      for (let offset = 0; offset < config.length; offset += 4 * 1024 * 1024) {
        await call('write', offset, new Uint8Array(await source.slice(offset, offset + 4 * 1024 * 1024).arrayBuffer()));
      }
      console.log('STORAGE_WRITTEN ' + config.length);
      await call('close');
      const result = await call('export');
      console.log('VERIFIED ' + JSON.stringify(result));
      window.storageFinished();
      return;
    }
    class SourceStore {
      constructor(size) { this.size = size; }
      get(index, opts, callback) {
        if (typeof opts === 'function') { callback = opts; opts = {}; }
        const offset = index * this.size + (opts.offset || 0);
        const length = opts.length ?? Math.min(this.size, config.length - index * this.size);
        source.slice(offset, offset + length).arrayBuffer().then(bytes => callback(null, new Uint8Array(bytes)), callback);
      }
      put(_index, _bytes, callback) { callback(new Error('source is read-only')); }
      close(callback) { callback?.(); }
      destroy(callback) { callback?.(); }
    }
    class DestinationStore {
      constructor(size, opts) {
        this.size = size; this.length = opts.length; this.written = new Set();
        this.ready = call('open', 'v1-' + config.hash, opts.length);
      }
      get(index, opts, callback) {
        if (typeof opts === 'function') { callback = opts; opts = {}; }
        if (!this.written.has(index)) { queueMicrotask(() => callback(new Error('piece not stored'))); return; }
        this.ready.then(() => call('read', index * this.size + (opts.offset || 0), opts.length ?? Math.min(this.size, this.length - index * this.size))).then(bytes => callback(null, bytes), callback);
      }
      put(index, bytes, callback) {
        this.ready.then(() => call('write', index * this.size, bytes)).then(() => { this.written.add(index); callback(null); }, callback);
      }
      close(callback) { this.ready.then(() => call('close')).then(() => callback?.(), callback); }
      destroy(callback) { this.close(callback); }
    }
    const client = new WebTorrent({ dht: false, lsd: false, utPex: false, webSeeds: false, utp: false,
      natUpnp: false, natPmp: false, tracker: { rtcConfig: { iceServers: [] } } });
    window.client = client;
    client.on('error', error => window.acceptanceFailed(String(error)));
    console.log('CLIENT_CREATED');
    const id = config.mode === 'seed' ? new Uint8Array(await (await fetch('/torrent')).arrayBuffer()) : config.magnet;
    console.log('ADDING_TORRENT');
    const torrent = client.add(id, { store: config.mode === 'seed' ? SourceStore : DestinationStore, storeCacheSlots: 0, destroyStoreOnDestroy: false });
    console.log('TORRENT_ADDED');
    torrent.on('ready', () => { console.log('TORRENT_READY ' + torrent.infoHash); if (torrent.infoHash !== config.hash) window.acceptanceFailed('unexpected info hash'); });
    torrent.on('warning', error => console.log('WARNING ' + String(error)));
    torrent.on('error', error => window.acceptanceFailed(String(error)));
    torrent.on('wire', (wire, address) => { console.log('WIRE ' + address); wire.on('timeout', () => console.log('WIRE_REQUEST_TIMEOUT ' + wire.requests.length)); });
    let finished = false;
    torrent.on('done', async () => {
      if (finished) return; finished = true;
      if (config.mode === 'seed') { console.log('READY'); return; }
      try {
        console.log('BROWSER_COMPLETE ' + JSON.stringify({ downloaded: torrent.downloaded, uploaded: torrent.uploaded, length: torrent.length }));
        await new Promise((resolve, reject) => client.destroy(error => error ? reject(error) : resolve()));
        const result = await call('export');
        console.log('VERIFIED ' + JSON.stringify(result));
      } catch (error) { window.acceptanceFailed(String(error)); }
    });
    if (config.mode === 'sink') console.log('READY');
    console.log('SETUP_COMPLETE');
    return true;
    })();
    window.acceptanceSetup.catch(error => window.acceptanceFailed(String(error)));
    return true;
  }, config);
  await Promise.race([new Promise(resolve => { process.stdin.once('data', resolve); process.once('SIGTERM', resolve); if (config.mode !== 'storage') { process.stdin.once('end', resolve); process.stdin.resume(); } }), failure, ...(config.mode === 'storage' ? [storageCompletion] : [])]);
  await page.evaluate(async () => { if (window.client && !window.client.destroyed) await new Promise(resolve => window.client.destroy(resolve)); });
} finally {
  await browser.close(); server.close(); if (destination) await destination.close();
}
