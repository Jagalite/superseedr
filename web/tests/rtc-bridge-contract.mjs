// SPDX-License-Identifier: GPL-3.0-or-later
import {readFile} from 'node:fs/promises';
import vm from 'node:vm';
import assert from 'node:assert/strict';
import test from 'node:test';

function host(extra = {}) {
  let now = 0, serial = 0;
  const timers = new Map();
  const context = vm.createContext({
    performance: {now: () => now}, Uint8Array, TextEncoder,
    setTimeout: (run, delay) => { const id = ++serial; timers.set(id, {run, delay}); return id; },
    setInterval: (run, delay) => { const id = ++serial; timers.set(id, {run, delay}); return id; },
    clearTimeout: id => timers.delete(id), clearInterval: id => timers.delete(id),
    ...extra,
  });
  return {context, advance: ms => { now += ms; }, run: delay => {
    for (const timer of [...timers.values()]) if (timer.delay === delay) timer.run();
  }};
}
function port() {
  return {messages: [], closed: false, start() {}, close() { this.closed = true; }, postMessage(data) { this.messages.push(data); }};
}
const bridgeSource = (await readFile(new URL('../../src/networking/webtorrent/browser.js', import.meta.url), 'utf8')).replaceAll('export ', '');

test('heartbeat loss rejects old peer work; fresh handshake recovers without stale callbacks', async () => {
  const clock = host(); vm.runInContext(bridgeSource, clock.context);
  const rtc = clock.context;
  const first = port(); let ready = false;
  const initial = rtc.installRtcBridge(first).then(() => { ready = true; });
  await Promise.resolve(); assert.equal(ready, false);
  first.onmessage({data: {heartbeat: true}}); await initial;
  const peer = rtc.createRtc('[]', true);
  const creation = first.messages.find(message => message.operation === 'create');
  first.onmessage({data: {call: creation.call, value: null}});
  const reading = peer.call('read');
  await Promise.resolve();
  const rejected = assert.rejects(reading, /RTC bridge closed/);
  clock.advance(21000); clock.run(5000);
  await rejected;
  assert.equal(first.closed, true); assert.equal(rtc.rtcAvailable(), false);
  const next = port(); const installed = rtc.installRtcBridge(next);
  next.onmessage({data: {heartbeat: true}}); await installed;
  first.onmessageerror(); first.onmessage({data: {heartbeat: true}});
  await assert.rejects(peer.close(), /unavailable/);
  assert.equal(rtc.rtcAvailable(), true); assert.equal(next.closed, false);
  rtc.disposeRtcBridge(); assert.equal(next.closed, true);
});

test('unacknowledged replacement expires and allows another port', async () => {
  const clock = host(); vm.runInContext(bridgeSource, clock.context);
  const first = port();
  const rejected = assert.rejects(clock.context.installRtcBridge(first), /did not remain connected/);
  clock.advance(10000); clock.run(10000); await rejected;
  assert.equal(first.closed, true); assert.equal(clock.context.rtcAvailable(), false);
  const next = port(); const ready = clock.context.installRtcBridge(next);
  next.onmessage({data: {heartbeat: true}}); await ready;
  clock.context.disposeRtcBridge();
});

const workerSource = (await readFile(new URL('../src/engine-worker.js', import.meta.url), 'utf8')).replace(/^import .*;$/m, '');
test('worker bounds retries, closes stale ports, and stops recovery during shutdown', async () => {
  const messages = []; let network = 'connected', replacing, replaceCount = 0, freed = 0;
  const client = {
    snapshot: () => JSON.stringify({network, torrents: []}),
    replace_rtc: () => { replaceCount++; return new Promise(resolve => { replacing = () => { network = 'connected'; resolve(); }; }); },
    shutdown: async () => {}, free: () => { freed++; },
  };
  const self = {postMessage: message => messages.push(message)};
  const clock = host({self, init: async () => {}, LiveClient: {start: async () => client}});
  vm.runInContext(workerSource, clock.context);
  const requests = () => messages.filter(message => message.rtc_bridge_request).map(message => message.rtc_bridge_request);
  await self.onmessage({data: {id: 0, method: 'start', port: port()}});
  network = 'reconnecting'; clock.run(250); assert.deepEqual(requests(), [1]);
  clock.run(250); assert.deepEqual(requests(), [1]);
  clock.advance(10001); clock.run(250); assert.deepEqual(requests(), [1, 2]);
  const stale = port(); await self.onmessage({data: {rtc_bridge_reply: 1, port: stale}});
  assert.equal(stale.closed, true); assert.equal(replaceCount, 0);
  const applying = self.onmessage({data: {rtc_bridge_reply: 2, port: port()}});
  clock.advance(20000); clock.run(250); assert.deepEqual(requests(), [1, 2]);
  const duplicate = port(); await self.onmessage({data: {rtc_bridge_reply: 2, port: duplicate}});
  assert.equal(duplicate.closed, true); assert.equal(replaceCount, 1);
  replacing(); await applying;
  network = 'reconnecting'; clock.run(250); assert.deepEqual(requests(), [1, 2, 3]);
  // A host setup error backs off before trying another bridge.
  await self.onmessage({data: {rtc_bridge_reply: 3, error: 'host unavailable'}});
  clock.run(250); assert.deepEqual(requests(), [1, 2, 3]);
  clock.advance(2001); clock.run(250); assert.deepEqual(requests(), [1, 2, 3, 4]);
  await self.onmessage({data: {id: 1, method: 'shutdown'}});
  const late = port(); await self.onmessage({data: {rtc_bridge_reply: 4, port: late}});
  clock.advance(20000); clock.run(250);
  assert.equal(late.closed, true); assert.equal(freed, 1);
  assert.deepEqual(requests(), [1, 2, 3, 4]);
});
