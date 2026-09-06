// SPDX-License-Identifier: GPL-3.0-or-later
import init, {LiveClient} from '../client-pkg/superseedr_browser_client.js';
let client, starting, updates, active = 0, stopping = false;
let bridgeSerial = 0, bridgeRequest, retryAt = 0;
const allowed = new Set(['add_magnet', 'add_torrent', 'pause', 'resume', 'remove', 'read_file', 'export_file', 'snapshot', 'shutdown']);
function publish() {
  const snapshot = JSON.parse(client.snapshot());
  self.postMessage({snapshot});
  if (stopping || snapshot.network !== 'reconnecting') return;
  const now = performance.now();
  if (bridgeRequest && !bridgeRequest.applying && now >= bridgeRequest.deadline) bridgeRequest = undefined;
  if (!bridgeRequest && now >= retryAt) {
    bridgeRequest = {id: ++bridgeSerial, deadline: now + 10000, applying: false};
    self.postMessage({rtc_bridge_request: bridgeRequest.id});
  }
}
async function replaceBridge(data) {
  const request = bridgeRequest;
  if (!request || request.id !== data.rtc_bridge_reply || request.applying || stopping || !client) {
    data.port?.close(); return;
  }
  request.applying = true;
  try {
    if (data.error || !data.port) throw new Error(data.error || 'RTC bridge port missing');
    await client.replace_rtc(data.port);
  } catch {
    data.port?.close();
    retryAt = performance.now() + 2000;
  } finally {
    if (bridgeRequest === request) bridgeRequest = undefined;
  }
}
self.onmessage = async ({data}) => {
  if ('rtc_bridge_reply' in data) { await replaceBridge(data); return; }
  const {id, method, args = []} = data;
  try {
    if (active >= 32) throw new Error('Browser command queue is full');
    if (stopping) throw new Error('Browser client is stopping');
    active++;
    let result;
    try {
      if (method === 'start') {
        if (starting) throw new Error('Client already started');
        starting = (async () => { await init(); client = await LiveClient.start(data.port); })();
        await starting;
        updates = setInterval(publish, 250);
        result = true;
      } else {
        if (!allowed.has(method) || !starting) throw new Error('Unknown command or client unavailable');
        // Gate new commands and bridge replies before awaiting the shutdown actor.
        if (method === 'shutdown') stopping = true;
        await starting;
        result = await client[method](...args);
        if (method === 'shutdown') { clearInterval(updates); client.free(); client = undefined; }
      }
    } finally { active--; }
    self.postMessage({id, result}, result instanceof Uint8Array ? [result.buffer] : []);
  } catch (error) { self.postMessage({id, error: String(error)}); }
};
