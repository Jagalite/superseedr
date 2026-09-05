// SPDX-License-Identifier: GPL-3.0-or-later
import {serveRtc} from '../../src/networking/webtorrent/browser.js';

// The Window owns physical browser RTC objects. The worker owns recovery and
// publishes network activations only after the new port answers its heartbeat.
export function createRtcHost(worker) {
  let stopped = false, closePort, lastRequest = 0;
  function disconnect() { closePort?.(); closePort = undefined; }
  function connect() {
    if (stopped) throw new Error('RTC host is closed');
    disconnect();
    const channel = new MessageChannel();
    closePort = serveRtc(channel.port1);
    return channel.port2;
  }
  function receive({data}) {
    const id = data.rtc_bridge_request;
    if (stopped || !Number.isSafeInteger(id) || id <= lastRequest) return;
    lastRequest = id;
    let port;
    try {
      port = connect();
      worker.postMessage({rtc_bridge_reply: id, port}, [port]);
    } catch (error) {
      port?.close(); disconnect();
      worker.postMessage({rtc_bridge_reply: id, error: String(error)});
    }
  }
  worker.addEventListener('message', receive);
  return {connect, disconnect, close() {
    stopped = true; worker.removeEventListener('message', receive); disconnect();
  }};
}
