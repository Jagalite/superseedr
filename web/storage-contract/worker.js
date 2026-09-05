// SPDX-License-Identifier: GPL-3.0-or-later
import init, {run_contract} from './pkg/superseedr_storage_contract.js';
await init();
const prototype = FileSystemFileHandle.prototype;
const originalCreate = prototype.createSyncAccessHandle;
if (globalThis.FileSystemSyncAccessHandle) {
  const sync = FileSystemSyncAccessHandle.prototype;
  const originalFlush = sync.flush;
  const originalWrite = sync.write;
  const originalRead = sync.read;
  sync.read = function(buffer, options) {
    if (globalThis.storageFault === "partial") buffer = buffer.subarray(0, 4096);
    return originalRead.call(this, buffer, options);
  };
  sync.flush = function(...args) {
    if (globalThis.storageFault === 'flush') throw new DOMException('Injected flush failure', 'QuotaExceededError');
    return originalFlush.apply(this,args);
  };
  sync.write = function(...args) {
    if (globalThis.storageFault === 'quota') throw new DOMException('Injected quota exhaustion', 'QuotaExceededError');
    if (globalThis.storageFault === 'zero') return 0;
    if (globalThis.storageFault === "partial") args[0] = args[0].subarray(0, 4096);
    return originalWrite.apply(this,args);
  };
}
self.onmessage = async ({data}) => {
  prototype.createSyncAccessHandle = data.fallback ? null : originalCreate;
  try { postMessage({ok:true,phase:data.phase,result:JSON.parse(await run_contract(data.phase,data.namespace,data.fallback))}); }
  catch(error) { postMessage({ok:false,phase:data.phase,error:String(error)}); }
};
postMessage({ready:true});
