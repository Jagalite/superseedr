// Independently written peer-wire fixture; generated payload only.
import { chromium } from '@playwright/test';
import { createServer } from 'node:http';
const config = JSON.parse(process.argv[2]);
const server = createServer((_request, response) => response.end('<!doctype html><title>Orbital transport contract</title>'));
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const browser = await chromium.launch({ headless: true });
try {
  const page = await browser.newPage();
  page.on('console', message => console.log(message.text()));
  page.on('pageerror', error => console.error(error));
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  await page.evaluate(config => {
    const hex = text => Uint8Array.from(text.match(/../g).map(pair => parseInt(pair, 16)));
    const binary = data => Array.from(data, byte => String.fromCharCode(byte)).join('');
    const encoder = new TextEncoder(), decoder = new TextDecoder();
    const hash = hex(config.hash), identity = hex(config.peer), metadata = hex(config.metadata);
    const payload = Uint8Array.from({ length: config.length }, (_, i) => (i * 13 + (i >>> 7)) & 255);
    const combine = (...arrays) => { const bytes = new Uint8Array(arrays.reduce((n, a) => n + a.length, 0)); let at = 0; for (const a of arrays) { bytes.set(a, at); at += a.length; } return bytes; };
    const integer = value => { const bytes = new Uint8Array(4); new DataView(bytes.buffer).setUint32(0, value); return bytes; };
    const uint = (bytes, offset = 0) => new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(offset);
    function encode(value) {
      if (typeof value === 'number') return encoder.encode(`i${value}e`);
      if (typeof value === 'string') return encoder.encode(`${value.length}:${value}`);
      return combine(encoder.encode('d'), ...Object.keys(value).sort().flatMap(key => [encode(key), encode(value[key])]), encoder.encode('e'));
    }
    function decode(bytes, index = 0) {
      if (bytes[index] === 100) { const result = {}; index++; while (bytes[index] !== 101) { const key = decode(bytes, index), value = decode(bytes, key[1]); result[key[0]] = value[0]; index = value[1]; } return [result, index + 1]; }
      if (bytes[index] === 105) { const end = bytes.indexOf(101, index); return [Number(decoder.decode(bytes.slice(index + 1, end))), end + 1]; }
      const separator = bytes.indexOf(58, index), length = Number(decoder.decode(bytes.slice(index, separator)));
      return [decoder.decode(bytes.slice(separator + 1, separator + 1 + length)), separator + 1 + length];
    }
    const socket = new WebSocket(config.tracker);
    let peer, announced = false;
    socket.onopen = () => socket.send(JSON.stringify({ action: 'announce', info_hash: binary(hash), peer_id: binary(identity), event: 'started', offers: [], numwant: 0, left: config.mode === 'seed' ? 0 : config.length, uploaded: 0, downloaded: 0 }));
    socket.onmessage = async event => {
      const message = JSON.parse(event.data);
      if (message.interval && !announced) { announced = true; console.log('READY'); }
      if (!message.offer || peer) return;
      peer = new RTCPeerConnection({ iceServers: [] });
      peer.ondatachannel = event => attach(event.channel);
      await peer.setRemoteDescription(message.offer);
      await peer.setLocalDescription(await peer.createAnswer());
      if (peer.iceGatheringState !== 'complete') await new Promise(resolve => peer.addEventListener('icegatheringstatechange', () => { if (peer.iceGatheringState === 'complete') resolve(); }));
      socket.send(JSON.stringify({ action: 'announce', info_hash: binary(hash), peer_id: binary(identity), to_peer_id: message.peer_id, offer_id: message.offer_id, answer: peer.localDescription }));
    };
    function attach(channel) {
      channel.binaryType = 'arraybuffer';
      let input = new Uint8Array(), handshaken = false, metadataId = 0, requested = false, received = 0, sentMetadata = false, askedMetadataBack = false;
      const downloaded = new Uint8Array(config.length);
      const send = bytes => { for (let i = 0; i < bytes.length; i += 16384) channel.send(bytes.slice(i, i + 16384)); };
      const frame = (id, bytes = new Uint8Array()) => send(combine(integer(bytes.length + 1), Uint8Array.of(id), bytes));
      const extension = (id, bytes) => frame(20, combine(Uint8Array.of(id), bytes));
      channel.onopen = () => console.log('CONNECTED');
      channel.onmessage = event => {
        input = combine(input, new Uint8Array(event.data));
        if (!handshaken) {
          if (input.length < 68) return;
          if (!input.slice(28, 48).every((byte, i) => byte === hash[i])) throw new Error('wrong handshake swarm');
          input = input.slice(68); handshaken = true;
          const greeting = new Uint8Array(68); greeting[0] = 19; greeting.set(encoder.encode('BitTorrent protocol'), 1); greeting[25] = 16; greeting.set(hash, 28); greeting.set(identity, 48); send(greeting);
          extension(0, encode({ m: { ut_metadata: 3 }, ...(config.mode === 'seed' ? { metadata_size: metadata.length } : {}) }));
          if (config.mode === 'seed') {
            const count = Math.ceil(config.length / config.pieceLength), bits = new Uint8Array(Math.ceil(count / 8));
            for (let p = 0; p < count; p++) bits[p >>> 3] |= 128 >>> (p & 7);
            frame(5, bits); frame(1);
          } else frame(2);
        }
        while (input.length >= 4) {
          const length = uint(input);
          if (length > 2 * 1024 * 1024) throw new Error('oversized frame');
          if (input.length < length + 4) return;
          const packet = input.slice(4, 4 + length); input = input.slice(4 + length);
          if (!length) continue;
          if (packet[0] === 1 && config.mode === 'sink' && !requested) {
            requested = true;
            for (let offset = 0; offset < config.length; offset += 16384) frame(6, combine(integer(Math.floor(offset / config.pieceLength)), integer(offset % config.pieceLength), integer(Math.min(16384, config.length - offset))));
          }
          if (packet[0] === 6 && config.mode === 'seed') {
            const piece = uint(packet, 1), begin = uint(packet, 5), count = uint(packet, 9), offset = piece * config.pieceLength + begin;
            if (count > 16384 || offset + count > payload.length) throw new Error('invalid block request');
            frame(7, combine(integer(piece), integer(begin), payload.slice(offset, offset + count)));
          }
          if (packet[0] === 7 && config.mode === 'sink') {
            const offset = uint(packet, 1) * config.pieceLength + uint(packet, 5), bytes = packet.slice(9);
            downloaded.set(bytes, offset); received += bytes.length;
            if (received === config.length) { if (!downloaded.every((byte, i) => byte === payload[i])) throw new Error('upload differs from generated payload'); console.log('VERIFIED'); }
          }
          if (packet[0] === 20 && packet[1] === 0) {
            const [greeting] = decode(packet.slice(2)); metadataId = greeting.m?.ut_metadata || 0;
            if (config.mode === 'seed' && sentMetadata && greeting.metadata_size && !askedMetadataBack) {
              askedMetadataBack = true; extension(metadataId, encode({msg_type: 0, piece: 0}));
            }
          }
          if (packet[0] === 20 && packet[1] === 3) {
            const [header, consumed] = decode(packet.slice(2));
            if (header.msg_type === 0 && config.mode === 'seed') { sentMetadata = true; extension(metadataId, encode({msg_type: 987})); const offset = header.piece * 16384; extension(metadataId, combine(encode({ msg_type: 1, piece: header.piece, total_size: metadata.length }), metadata.slice(offset, offset + 16384))); console.log('METADATA_SENT'); }
            if (header.msg_type === 1 && config.mode === 'seed') {
              const data = packet.slice(2 + consumed);
              if (data.length !== metadata.length || !data.every((byte, i) => byte === metadata[i])) throw new Error('returned metadata mismatch');
              console.log('METADATA_RETURNED');
            }
          }
        }
      };
    }
  }, config);
  await new Promise(resolve => process.stdin.once('data', resolve));
} finally { await browser.close(); server.close(); }
