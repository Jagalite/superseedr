import init, { Store } from "./pkg/superseedr_storage_contract.js";
await init();
let store, restore;
onmessage = async ({ data: { id, method, args = [] } }) => {
  try {
    let result;
    if (method === "open") store = await Store.open(...args);
    else if (method === "open_file") store = await Store.open_file(...args);
    else if (method === "fault") {
      const fallback = args[0];
      const prototype = fallback
        ? FileSystemFileHandle.prototype
        : FileSystemSyncAccessHandle.prototype;
      const key = fallback ? "createWritable" : "write";
      const old = prototype[key];
      prototype[key] = function (...values) {
        if (!fallback || this.name.startsWith("file-"))
          throw new DOMException(
            "contract quota exhaustion",
            "QuotaExceededError",
          );
        return old.apply(this, values);
      };
      restore = () => {
        prototype[key] = old;
      };
    } else if (method === "count_fault") {
      const [key, count] = args;
      const prototype = FileSystemSyncAccessHandle.prototype;
      const old = prototype[key];
      prototype[key] = function (bytes, options) {
        // A short operation must really transfer the prefix it reports.
        if (count === "partial") return old.call(this, bytes.subarray(0, 7), options);
        return count;
      };
      restore = () => { prototype[key] = old; };
    } else if (method === "restore") {
      restore?.();
      restore = null;
    } else if (method === "burst") {
      for (let i = 0; i < 64; i++)
        store.cancel_write(0, new Uint8Array(4096).fill(i));
      result = store.stats();
    } else result = await store[method](...args);
    postMessage({ id, result });
  } catch (error) {
    postMessage({ id, error: String(error) });
  }
};
postMessage({ ready: true });
