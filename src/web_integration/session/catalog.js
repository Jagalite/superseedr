// SPDX-License-Identifier: GPL-3.0-or-later
// One application catalog owner per origin, held until orderly host shutdown.
export async function openCatalog() {
  if (!navigator.locks) throw new Error("Browser ownership locks are unavailable");
  let release;
  const held = new Promise(resolve => { release = resolve; });
  let acquired, failed;
  const ready = new Promise((resolve, reject) => { acquired = resolve; failed = reject; });
  const lock = navigator.locks.request("superseedr-browser-catalog-v1", {ifAvailable: true}, async token => {
    if (!token) { failed(new Error("Another tab owns this browser client")); return; }
    acquired(); await held;
  });
  lock.catch(failed);
  await ready;
  try {
    const db = await new Promise((resolve, reject) => {
      const request = indexedDB.open("superseedr-browser-v1", 1);
      request.onupgradeneeded = () => request.result.createObjectStore("catalog");
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
      request.onblocked = () => reject(new Error("Catalog upgrade blocked"));
    });
    return {db, release, lock};
  } catch (error) { release(); throw error; }
}
// Keep the small settings snapshot and binary metadata in the same transaction.
// Version-one inline metadata is still readable and migrates on the next write.
const metadataPrefix = "metadata:";
function toHex(bytes) {
  const alphabet = Array.from({length: 256}, (_, i) => i.toString(16).padStart(2, "0"));
  const chunks = [];
  for (let start = 0; start < bytes.length; start += 16384) {
    let chunk = "";
    for (const byte of bytes.subarray(start, start + 16384)) chunk += alphabet[byte];
    chunks.push(chunk);
  }
  return chunks.join("");
}
export async function readCatalog(owner) {
  const {snapshot, metadata} = await new Promise((resolve, reject) => {
    const tx = owner.db.transaction("catalog", "readonly");
    let snapshot = "";
    const metadata = {};
    const request = tx.objectStore("catalog").openCursor();
    request.onsuccess = () => {
      const cursor = request.result;
      if (!cursor) return;
      if (cursor.key === "snapshot") snapshot = cursor.value;
      else if (typeof cursor.key === "string" && cursor.key.startsWith(metadataPrefix))
        metadata[cursor.key.slice(metadataPrefix.length)] = cursor.value;
      cursor.continue();
    };
    tx.oncomplete = () => resolve({snapshot, metadata});
    tx.onerror = tx.onabort = () => reject(tx.error || new Error("Catalog read aborted"));
  });
  if (!snapshot) return "";
  const catalog = JSON.parse(snapshot);
  catalog.metadata ||= {};
  for (const [hash, bytes] of Object.entries(metadata)) catalog.metadata[hash] = toHex(bytes);
  return JSON.stringify(catalog);
}
export async function writeCatalog(owner, snapshot) {
  const {metadata = {}, ...catalog} = JSON.parse(snapshot);
  const settings = JSON.stringify(catalog);
  if (settings.length > 32 * 1024 * 1024) throw new Error("Catalog settings exceed limit");
  const records = new Map();
  for (const [hash, hex] of Object.entries(metadata)) {
    if (!/^[0-9a-f]{40}$/.test(hash) || typeof hex !== "string" || hex.length % 2 || /[^0-9a-f]/.test(hex))
      throw new Error("Invalid catalog metadata");
    const bytes = new Uint8Array(hex.length / 2);
    for (let i = 0; i < bytes.length; i++) bytes[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
    records.set(metadataPrefix + hash, bytes);
  }
  await new Promise((resolve, reject) => {
    const tx = owner.db.transaction("catalog", "readwrite");
    const store = tx.objectStore("catalog");
    store.put(settings, "snapshot");
    for (const [key, bytes] of records) store.put(bytes, key);
    const cursor = store.openKeyCursor();
    cursor.onsuccess = () => {
      const entry = cursor.result;
      if (!entry) return;
      if (typeof entry.key === "string" && entry.key.startsWith(metadataPrefix) && !records.has(entry.key)) store.delete(entry.key);
      entry.continue();
    };
    tx.oncomplete = resolve;
    tx.onerror = tx.onabort = () => reject(tx.error || new Error("Catalog write aborted"));
  });
}
export function closeCatalog(owner) { owner.db.close(); owner.release(); }
