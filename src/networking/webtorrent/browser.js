// SPDX-License-Identifier: GPL-3.0-or-later
// Physical browser adapters only. Tracker and peer policy remain in Rust.
const WINDOW = 256 * 1024;
const CHUNK = 16 * 1024;
const MAX_SDP = 256 * 1024;
let bridge;

function wakeable() {
  const waiters = new Set();
  return {
    wait: () => new Promise(resolve => waiters.add(resolve)),
    wake: () => { for (const resolve of waiters) resolve(); waiters.clear(); },
  };
}
class Channel {
  constructor(ice, initiator) {
    this.pc = new RTCPeerConnection({iceServers: ice});
    this.signal = wakeable(); this.queue = []; this.bytes = 0; this.error = null;
    this.pc.onicegatheringstatechange = () => this.signal.wake();
    this.pc.onconnectionstatechange = () => {
      if (["failed", "closed"].includes(this.pc.connectionState)) this.close("RTC connection closed");
      this.signal.wake();
    };
    this.pc.ondatachannel = ({channel}) => this.attach(channel);
    if (initiator) this.attach(this.pc.createDataChannel("torrent", {ordered: true}));
  }
  attach(channel) {
    if (this.dc || this.error || !channel.ordered || channel.maxRetransmits !== null || channel.maxPacketLifeTime !== null) {
      channel.close(); return;
    }
    this.dc = channel; channel.binaryType = "arraybuffer";
    channel.bufferedAmountLowThreshold = WINDOW / 2;
    channel.onopen = channel.onbufferedamountlow = () => this.signal.wake();
    channel.onclose = channel.onerror = () => this.close("RTC DataChannel closed");
    channel.onmessage = ({data}) => {
      if (!(data instanceof ArrayBuffer) || data.byteLength === 0 || data.byteLength > WINDOW || this.bytes + data.byteLength > WINDOW) {
        this.close("RTC receive capacity exceeded or invalid payload"); return;
      }
      this.queue.push(new Uint8Array(data)); this.bytes += data.byteLength; this.signal.wake();
    };
  }
  check() { if (this.error) throw new Error(this.error); }
  async waitFor(predicate) {
    while (true) { this.check(); if (predicate()) return; await this.signal.wait(); }
  }
  async call(operation, data) {
    this.check();
    if (operation === "offer" || operation === "answer") {
      if (operation === "answer") await this.pc.setRemoteDescription(data);
      this.check();
      const description = operation === "offer" ? await this.pc.createOffer() : await this.pc.createAnswer();
      this.check(); await this.pc.setLocalDescription(description);
      await this.waitFor(() => this.pc.iceGatheringState === "complete");
      const local = this.pc.localDescription;
      if (!local || new TextEncoder().encode(local.sdp).length > MAX_SDP) throw new Error("local SDP exceeds limit");
      return {type: local.type, sdp: local.sdp};
    }
    if (operation === "accept") { await this.pc.setRemoteDescription(data); this.check(); return null; }
    if (operation === "ready") { await this.waitFor(() => this.dc?.readyState === "open"); return null; }
    if (operation === "read") {
      await this.waitFor(() => this.queue.length > 0);
      const first = this.queue[0]; const result = first.slice(0, CHUNK);
      if (first.length <= CHUNK) this.queue.shift(); else this.queue[0] = first.subarray(CHUNK);
      this.bytes -= result.length; return result;
    }
    if (operation === "write") {
      if (!(data instanceof Uint8Array) || !data.length || data.length > CHUNK) throw new Error("invalid RTC write size");
      await this.waitFor(() => this.dc?.readyState === "open" && this.dc.bufferedAmount + data.byteLength <= WINDOW);
      this.dc.send(data); return null;
    }
    throw new Error("unknown RTC operation");
  }
  close(reason = "RTC connection closed") {
    if (this.error) return;
    this.error = reason;
    if (this.dc) {
      this.dc.onopen = this.dc.onmessage = this.dc.onclose = this.dc.onerror = this.dc.onbufferedamountlow = null;
      this.dc.close();
    }
    this.pc.ondatachannel = this.pc.onicegatheringstatechange = this.pc.onconnectionstatechange = null;
    this.pc.close(); this.queue.length = 0; this.bytes = 0; this.signal.wake();
  }
}

// Call in the Window. One port has one owner lifetime and bounded operations.
export function serveRtc(port) {
  const peers = new Map(); let stopped = false, pending = 0, lastSeen = performance.now();
  const stop = () => {
    if (stopped) return; stopped = true; clearInterval(lease);
    for (const peer of peers.values()) peer.close("RTC host lifetime ended");
    peers.clear(); port.close();
  };
  const lease = setInterval(() => { if (performance.now() - lastSeen > 20000) stop(); }, 5000);
  port.onmessageerror = stop;
  port.onmessage = async ({data: message}) => {
    if (stopped || message?.kind !== "rtc") return;
    lastSeen = performance.now();
    const {call, id, operation, data} = message;
    if (operation === "heartbeat") { port.postMessage({heartbeat: true}); return; }
    if (operation === "dispose") { stop(); return; }
    let counted = false;
    try {
      if (!Number.isSafeInteger(call) || !Number.isSafeInteger(id)) throw new Error("invalid bridge identity");
      if (operation === "close") {
        peers.get(id)?.close(); peers.delete(id);
        port.postMessage({call, value: null}); return;
      }
      if (pending >= 512) throw new Error("RTC bridge operation limit");
      pending++; counted = true;
      let value;
      if (operation === "create") {
        if (peers.size >= 128 || peers.has(id)) throw new Error("RTC bridge peer limit or duplicate identity");
        peers.set(id, new Channel(data.ice, data.initiator)); value = null;
      } else {
        const peer = peers.get(id); if (!peer) throw new Error("RTC bridge peer closed");
        value = await peer.call(operation, data);
      }
      if (!stopped) port.postMessage({call, value}, value instanceof Uint8Array ? [value.buffer] : []);
    } catch (error) { if (!stopped) port.postMessage({call, error: String(error)}); }
    finally { if (counted) pending--; }
  };
  port.start(); return stop;
}

// Call in the application worker before constructing any managers.
export function installRtcBridge(port) {
  if (bridge) throw new Error("RTC bridge already installed");
  let serial = 0, peerSerial = 0, stopped = false, lastReply = performance.now();
  const pending = new Map();
  let readyResolve, readyReject;
  const ready = new Promise((resolve, reject) => { readyResolve = resolve; readyReject = reject; });
  const readyDeadline = setTimeout(() => stop(), 10000);
  function stop() {
    if (stopped) return; stopped = true; clearInterval(heartbeat); clearTimeout(readyDeadline);
    readyReject(new Error("RTC bridge did not remain connected"));
    for (const item of pending.values()) { clearTimeout(item.timer); item.reject(new Error("RTC bridge closed")); }
    pending.clear(); port.postMessage({kind: "rtc", operation: "dispose"}); port.close(); bridge = undefined;
  }
  function rpc(id, operation, data) {
    // Reserve cleanup admission even when data/signaling calls fill the normal queue.
    if (stopped || (operation !== "close" && pending.size >= 384) || pending.size >= 512) return Promise.reject(new Error("RTC bridge unavailable or full"));
    const call = ++serial;
    return new Promise((resolve, reject) => {
      const timer = operation === "read" ? null : setTimeout(() => { pending.delete(call); reject(new Error("RTC bridge deadline")); }, 45000);
      pending.set(call, {resolve, reject, timer});
      port.postMessage({kind: "rtc", call, id, operation, data}, data instanceof Uint8Array ? [data.buffer] : []);
    });
  }
  port.onmessageerror = stop;
  port.onmessage = ({data}) => {
    if (stopped) return;
    lastReply = performance.now();
    if (data.heartbeat) { clearTimeout(readyDeadline); readyResolve(); }
    const item = pending.get(data.call); if (!item) return;
    pending.delete(data.call); clearTimeout(item.timer);
    data.error ? item.reject(new Error(data.error)) : item.resolve(data.value);
  };
  const heartbeat = setInterval(() => {
    if (performance.now() - lastReply > 20000) { stop(); return; }
    port.postMessage({kind: "rtc", operation: "heartbeat"});
  }, 5000);
  port.start();
  bridge = {create(ice, initiator) {
    const id = ++peerSerial; const created = rpc(id, "create", {ice, initiator});
    created.catch(() => {});
    let closed = false, closing;
    return {
      async call(operation, data) { await created; if (closed) throw new Error("RTC peer closed"); return rpc(id, operation, data); },
      close() { closed = true; return closing ||= rpc(id, "close", null); },
    };
  }, stop};
  // A fresh activation is published only after the Window acknowledges this port.
  port.postMessage({kind: "rtc", operation: "heartbeat"});
  return ready;
}
export function disposeRtcBridge() { bridge?.stop(); }
export function rtcAvailable() { return !!bridge || typeof RTCPeerConnection === "function"; }
export function createRtc(encodedIce, initiator) {
  const ice = JSON.parse(encodedIce);
  return bridge ? bridge.create(ice, initiator) : new Channel(ice, initiator);
}
export async function rtcCall(peer, operation, encoded, bytes) {
  const value = await peer.call(operation, operation === "write" ? bytes : encoded ? JSON.parse(encoded) : null);
  return operation === "offer" || operation === "answer" ? JSON.stringify(value) : value;
}
export async function closeRtc(peer) { await peer.close(); }

export function createSocket(url, max) {
  const ws = new WebSocket(url); const signal = wakeable();
  const state = {ws, signal, queue: [], bytes: 0, error: null, max};
  ws.onopen = () => signal.wake();
  ws.onclose = ws.onerror = () => { state.error = "tracker socket closed"; signal.wake(); };
  ws.onmessage = ({data}) => {
    if (typeof data !== "string") { closeSocket(state); return; }
    const size = new TextEncoder().encode(data).length;
    if (size > max || state.queue.length >= 16 || state.bytes + size > max * 2) { closeSocket(state); return; }
    state.queue.push({data, size}); state.bytes += size; signal.wake();
  };
  return state;
}
export async function socketCall(state, operation, text) {
  while (true) {
    if (state.error) throw new Error(state.error);
    if (operation === "open" && state.ws.readyState === WebSocket.OPEN) return null;
    if (operation === "read" && state.queue.length) {
      const item = state.queue.shift(); state.bytes -= item.size; return item.data;
    }
    if (operation === "send") {
      if (state.ws.readyState !== WebSocket.OPEN || state.ws.bufferedAmount > state.max * 2 || new TextEncoder().encode(text).length > state.max) throw new Error("tracker send unavailable or full");
      state.ws.send(text); return null;
    }
    await state.signal.wait();
  }
}
export function closeSocket(state) {
  state.error = "tracker socket closed";
  state.ws.onopen = state.ws.onmessage = state.ws.onclose = state.ws.onerror = null;
  state.ws.close(); state.queue.length = 0; state.bytes = 0; state.signal.wake();
}
