// SPDX-License-Identifier: GPL-3.0-or-later
// Real browser TM -> shared PeerSession -> OPFS, against an independent client.
import {chromium, expect} from '@playwright/test';
import {createRequire} from 'node:module';
import {createServer} from 'node:http';
import {readFile, mkdtemp} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {resolve, sep} from 'node:path';
import {fileURLToPath} from 'node:url';
import {createHash} from 'node:crypto';
const require = createRequire(import.meta.url);
const {wsServer: WebSocketServer} = require('playwright-core/lib/utilsBundle');
const root = resolve(fileURLToPath(new URL('../..', import.meta.url)));
const clientPath = process.env.SUPERSEEDR_TEST_CLIENT || resolve(root, 'target/iso-acceptance/package/dist/webtorrent.min.js');
const independent = await readFile(clientPath);
const payload = Buffer.from(Array.from({length: 2 * 1024 * 1024 + 37}, (_, i) => (i * 13 + (i >>> 7)) & 255));
const sha256 = createHash('sha256').update(payload).digest('hex');
const http = createServer(async (request, response) => {
  try {
    const url = new URL(request.url, 'http://localhost');
    let data;
    if (url.pathname === '/') data = '<!doctype html><title>Browser transfer contract</title>';
    else if (url.pathname === '/independent.js') data = independent;
    else if (url.pathname === '/payload') data = payload;
    else {
      const path = resolve(root, '.' + url.pathname);
      if (!path.startsWith(root + sep)) throw new Error('invalid path');
      data = await readFile(path);

    }
    response.setHeader('Content-Type', url.pathname.endsWith('.wasm') ? 'application/wasm' : /\.(m?js)$/.test(url.pathname) ? 'text/javascript' : url.pathname.endsWith('.css') ? 'text/css' : 'text/html');
    response.end(data);
  } catch (error) { response.writeHead(500).end(String(error)); }
});
const tracker = new WebSocketServer({server: http, path: '/announce'});
const members = new Map();
let offers = 0, answers = 0;
tracker.on('connection', socket => {
  socket.on('message', raw => {
    const message = JSON.parse(raw.toString());
    if (message.action !== 'announce') return;
    const key = message.info_hash + message.peer_id;
    if (message.event === 'stopped') { members.delete(key); return; }
    members.set(key, {socket, peer: message.peer_id, hash: message.info_hash});
    if (message.answer) {
      answers++; members.get(message.info_hash + message.to_peer_id)?.socket.send(JSON.stringify(message)); return;
    }
    socket.send(JSON.stringify({action: 'announce', info_hash: message.info_hash, interval: 2, complete: 1, incomplete: 1}));
    const other = [...members.values()].find(value => value.hash === message.info_hash && value.peer !== message.peer_id && value.socket.readyState === 1);
    if (other) for (const offer of message.offers || []) { offers++; other.socket.send(JSON.stringify({action: 'announce', info_hash: message.info_hash, peer_id: message.peer_id, ...offer})); }
  });
  socket.on('close', () => { for (const [key, value] of members) if (value.socket === socket) members.delete(key); });
});
await new Promise(resolve => http.listen(0, '127.0.0.1', resolve));
const origin = `http://127.0.0.1:${http.address().port}`;
const trackerUrl = origin.replace('http:', 'ws:') + '/announce';
const profile = await mkdtemp(resolve(tmpdir(), 'ss-browser-transfer-'));
// Optional local-only escape hatch for machines whose mDNS cannot resolve
// Chromium's .local ICE candidates. Keep normal browser privacy defaults otherwise.
const disableMdns = process.env.SUPERSEEDR_TEST_DISABLE_MDNS === '1';
const browser = await chromium.launchPersistentContext(profile, {headless: true,
  args: disableMdns ? ['--disable-features=WebRtcHideLocalIpsWithMdns'] : [],
});
console.log('BROWSER_TEST_MODE', {disableMdns});
const errors = [];
try {
  const peer = await browser.newPage(); await peer.goto(origin);
  peer.on('pageerror', error => errors.push(String(error)));
  peer.on('console', message => console.log('peer:', message.text()));
  const contract = await peer.evaluate(async () => {
    const module = await import('/web/client-pkg/superseedr_browser_client.js'); await module.default();
    return module.browser_runtime_contract();
  });
  console.log(contract);
  const seed = await peer.evaluate(async tracker => {
    const {default: Client} = await import('/independent.js');
    window.client = new Client({dht: false, lsd: false, utPex: false, webSeeds: false, tracker: {rtcConfig: {iceServers: []}}});
    const bytes = new Uint8Array(await (await fetch('/payload')).arrayBuffer());
    return new Promise((resolve, reject) => {
      window.client.on('error', reject);
      window.client.on('warning', warning => console.log('seed warning:', String(warning)));
      window.client.seed(new File([bytes], 'orbital-data.bin'), {announce: [tracker], pieceLength: 64 * 1024}, torrent => {
        window.torrent = torrent; resolve({magnet: torrent.magnetURI, hash: torrent.infoHash, metadata: Array.from(torrent.torrentFile)});
      });
    });
  }, trackerUrl);
  const page = await browser.newPage(); await page.goto(origin);
  page.on('console', message => console.log('browser:', message.text()));
  page.on('pageerror', error => errors.push(String(error)));
  async function start() {
    await page.evaluate(async () => {
      window.snapshot = null;
      const {createRtcHost} = await import('/web/src/rtc-host.js');
      const worker = new Worker('/web/src/engine-worker.js', {type: 'module'});
      window.rtcHost = createRtcHost(worker);
      const port = window.rtcHost.connect(); window.closeRtc = () => window.rtcHost.close();
      window.worker = worker; let serial = 0; const pending = new Map();
      window.call = (method, ...args) => new Promise((resolve, reject) => {
        const id = ++serial; pending.set(id, {resolve, reject});
        worker.postMessage({id, method, args});
      });
      worker.onmessage = ({data}) => {
        if (data.snapshot) { window.snapshot = data.snapshot; return; }
        const item = pending.get(data.id); if (!item) return;
        pending.delete(data.id); data.error ? item.reject(new Error(data.error)) : item.resolve(data.result);
      };
      worker.onerror = event => { for (const item of pending.values()) item.reject(new Error(event.message)); pending.clear(); };
      await new Promise((resolve, reject) => { pending.set(0, {resolve, reject}); worker.postMessage({id: 0, method: 'start', port}, [port]); });
    });
  }
  await start();
  await page.evaluate(async magnet => {
    await window.call('add_magnet', magnet);
    try { await window.call('add_magnet', magnet); throw new Error('duplicate accepted'); }
    catch (error) { if (!String(error).includes('already has an active manager')) throw error; }
    const {openCatalog} = await import('/src/web_integration/session/catalog.js');
    try { await openCatalog(); throw new Error('second catalog owner accepted'); }
    catch (error) { if (!String(error).includes('Another tab owns')) throw error; }
  }, seed.magnet);
  const interval = setInterval(async () => { try { console.log('progress', {offers, answers, members: members.size}, await page.evaluate(() => JSON.stringify(window.snapshot))); } catch {} }, 10000);
  try { await page.waitForFunction(() => window.snapshot?.torrents?.some(t => t.is_complete), null, {timeout: 120000}); } finally { clearInterval(interval); }
  const digest = await page.evaluate(async ({hash, length}) => {
    const bytes = new Uint8Array(length);
    for (let at = 0; at < length; at += 256 * 1024) bytes.set(await window.call('read_file', hash, 0, BigInt(at), Math.min(256 * 1024, length - at)), at);
    return Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bytes)), b => b.toString(16).padStart(2, '0')).join('');
  }, {hash: seed.hash, length: payload.length});
  if (digest !== sha256) throw new Error('OPFS export digest mismatch');
  console.log('DOWNLOAD_VERIFIED', digest);
  await page.evaluate(async hash => {
    for (const [index, offset, length] of [[0, 0n, 0], [1, 0n, 1], [0, 99999999n, 1]]) {
      let rejected = false;
      try { await window.call('read_file', hash, index, offset, length); } catch { rejected = true; }
      if (!rejected) throw new Error('invalid range accepted');
    }
  }, seed.hash);
  await peer.evaluate(() => new Promise(resolve => window.client.destroy(resolve)));
  await page.evaluate(async () => { await window.call('shutdown'); window.worker.terminate(); window.closeRtc(); });
  await start();
  await page.waitForFunction(() => window.snapshot?.torrents?.some(t => t.is_complete), null, {timeout: 30000}).catch(async error => { console.log("RESTORE_STATE", await page.evaluate(() => JSON.stringify(window.snapshot))); throw error; });
  console.log('RELOAD_RECHECK_VERIFIED');
  // Drop the actual Window bridge and let the worker's heartbeat expire. Delay
  // the first host reply past the request deadline, then deliver its stale port.
  await page.evaluate(() => {
    window.rtcHost.close();
    window.recoveryRequests = 0;
    window.delayedRtc = event => {
      if (!event.data.rtc_bridge_request) return;
      window.recoveryRequests++;
      if (window.recoveryRequests === 1) {
        window.staleBridgeId = event.data.rtc_bridge_request;
      } else {
        window.worker.removeEventListener('message', window.delayedRtc);
        import('/web/src/rtc-host.js').then(({createRtcHost}) => {
          window.rtcHost = createRtcHost(window.worker);
          // Redispatch so the new host handles this current request.
          window.worker.dispatchEvent(new MessageEvent('message', {data: event.data}));
          const stale = new MessageChannel();
          window.worker.postMessage({rtc_bridge_reply: window.staleBridgeId, port: stale.port2}, [stale.port2]);
          stale.port1.close();
        });
      }
    };
    window.worker.addEventListener('message', window.delayedRtc);
  });
  await page.waitForFunction(() => window.snapshot?.network === 'reconnecting', null, {timeout: 30000});
  await page.waitForFunction(() => window.snapshot?.network === 'connected' && window.recoveryRequests >= 2, null, {timeout: 30000});
  console.log('RTC_BRIDGE_TIMEOUT_RETRY_STALE_REPLY_RECOVERY_VERIFIED');
  let reseedTimer;
  const upload = await Promise.race([peer.evaluate(async magnet => {
    const {default: Client} = await import('/independent.js');
    window.client = new Client({dht: false, lsd: false, utPex: false, webSeeds: false, tracker: {rtcConfig: {iceServers: []}}});
    return new Promise((resolve, reject) => {
      window.client.on('error', reject);
      const torrent = window.client.add(magnet); torrent.on('error', reject);
      torrent.on('done', async () => {
        const buffer = await torrent.files[0].arrayBuffer();
        resolve(Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', buffer)), b => b.toString(16).padStart(2, '0')).join(''));
      });
    });
  }, seed.magnet), new Promise((_, reject) => { reseedTimer = setTimeout(() => reject(new Error("reseed deadline")), 120000); })]).finally(() => clearTimeout(reseedTimer));
  if (upload !== sha256) throw new Error('browser seed digest mismatch');
  await page.evaluate(async () => { await window.call('shutdown'); window.worker.terminate(); window.closeRtc(); });
  await peer.evaluate(() => new Promise(resolve => window.client.destroy(resolve)));
  if (process.env.SUPERSEEDR_TEST_BUILT_UI === '1') {
    const ui = await browser.newPage();
    ui.on('pageerror', error => errors.push(String(error)));
    await ui.addInitScript(() => { window.showSaveFilePicker = undefined; });
    await ui.goto(origin + '/web/client-dist/webtorrent.html');
    const row = ui.locator('.torrent');
    await expect(row).toHaveCount(1);
    await ui.waitForFunction(() => document.querySelector('progress')?.value === 1, null, {timeout: 30000});
    const saved = ui.waitForEvent('download');
    await row.getByRole('button', {name: 'Save', exact: true}).click();
    const artifact = await saved;
    if (createHash('sha256').update(await readFile(await artifact.path())).digest('hex') !== sha256) throw new Error('page export mismatch');
    await row.getByRole('button', {name: 'Pause', exact: true}).click();
    await expect(row.getByRole('button', {name: 'Resume', exact: true})).toBeVisible();
    await row.getByRole('button', {name: 'Resume', exact: true}).click();
    await expect(row.getByRole('button', {name: 'Pause', exact: true})).toBeVisible();
    // UI upload still routes through duplicate protection; invalid input leaves the live manager intact.
    await ui.locator('#torrent').setInputFiles({name: 'orbital-data.torrent', mimeType: 'application/x-bittorrent', buffer: Buffer.from(seed.metadata)});
    await expect(ui.locator('#error')).toContainText('already has an active manager');
    await expect(row).toHaveCount(1);
    ui.once('dialog', dialog => dialog.accept());
    await row.getByRole('button', {name: 'Remove', exact: true}).click();
    await expect(row).toHaveCount(0, {timeout: 30000});
    await ui.getByRole('button', {name: 'Stop client', exact: true}).click();
    await expect(ui.locator('#status')).toHaveText('Stopped');
    // Fresh startup consumes the parameter. The previous confirmed deletion must not restore a duplicate.
    await ui.goto(origin + '/web/client-dist/webtorrent.html?magnet=' + encodeURIComponent(seed.magnet));
    await expect(row).toHaveCount(1);
    await expect(ui.locator('#error')).toHaveText('');
    ui.once('dialog', dialog => dialog.accept());
    await row.getByRole('button', {name: 'Remove', exact: true}).click();
    await expect(row).toHaveCount(0, {timeout: 30000});
    await ui.locator('#torrent').setInputFiles({name: 'orbital-data.torrent', mimeType: 'application/x-bittorrent', buffer: Buffer.from(seed.metadata)});
    await expect(row.getByRole('heading', {name: 'orbital-data.bin'})).toBeVisible();
    await expect(row.getByRole('button', {name: 'Save', exact: true})).toBeVisible();
    await ui.getByRole('button', {name: 'Stop client', exact: true}).click();
    await expect(ui.locator('#status')).toHaveText('Stopped');
    console.log('BUILT_PAGE_RESTORE_SAVE_PAUSE_RESUME_UPLOAD_REMOVE_PARAMETER_VERIFIED');
  }
  // Removal accepted immediately before global shutdown must stay removed on
  // the next startup, for both retained payload and deleted payload requests.
  for (const files of [false, true]) {
    await start();
    // The optional built-page test leaves this fixture in the catalog.
    await page.evaluate(async ({hash, metadata}) => {
      const snapshot = JSON.parse(await window.call('snapshot'));
      if (!snapshot.torrents.some(t => t.info_hash.map(b => b.toString(16).padStart(2, '0')).join('') === hash)) {
        await window.call('add_torrent', new Uint8Array(metadata));
      }
    }, seed);
    await page.evaluate(async ({hash, files}) => {
      await Promise.all([window.call('remove', hash, files), window.call('shutdown')]);
      window.worker.terminate(); window.closeRtc();
    }, {hash: seed.hash, files});
    await start();
    await page.evaluate(async hash => {
      const snapshot = JSON.parse(await window.call('snapshot'));
      if (snapshot.torrents.some(t => t.info_hash.map(b => b.toString(16).padStart(2, '0')).join('') === hash)) throw new Error('removed torrent restored after shutdown');
      await window.call('shutdown'); window.worker.terminate(); window.closeRtc();
    }, seed.hash);
  }
  console.log('REMOVE_DURING_SHUTDOWN_KEEP_AND_DELETE_VERIFIED');
  if (errors.length) throw new Error(errors.join('\n'));
  console.log(JSON.stringify({download: digest, reseed: upload, bytes: payload.length, offers, answers, profile}));
} finally {
  await browser.close(); for (const member of members.values()) member.socket.terminate(); tracker.close(); http.close();
}
