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
export async function readCatalog(owner) {
  return new Promise((resolve, reject) => {
    const tx = owner.db.transaction("catalog", "readonly");
    const request = tx.objectStore("catalog").get("snapshot");
    let result = "";
    request.onsuccess = () => { result = request.result || ""; };
    tx.oncomplete = () => resolve(result);
    tx.onerror = tx.onabort = () => reject(tx.error || new Error("Catalog read aborted"));
  });
}
export async function writeCatalog(owner, snapshot) {
  if (typeof snapshot !== "string" || snapshot.length > 32 * 1024 * 1024) throw new Error("Catalog snapshot exceeds limit");
  await new Promise((resolve, reject) => {
    const tx = owner.db.transaction("catalog", "readwrite");
    tx.objectStore("catalog").put(snapshot, "snapshot");
    tx.oncomplete = resolve;
    tx.onerror = tx.onabort = () => reject(tx.error || new Error("Catalog write aborted"));
  });
}
export function closeCatalog(owner) { owner.db.close(); owner.release(); }
