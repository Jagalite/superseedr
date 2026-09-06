// SPDX-License-Identifier: GPL-3.0-or-later
import { chromium, firefox, webkit } from "@playwright/test";
import { createServer } from "node:http";
import { readFile, mkdtemp, rm } from "node:fs/promises";
import {tmpdir} from "node:os";
import { fileURLToPath } from "node:url";
import { resolve, sep } from "node:path";
import assert from "node:assert/strict";
import {createHash} from "node:crypto";
const root = resolve(
  fileURLToPath(new URL("../storage-contract/", import.meta.url)),
);
const server = createServer(async (req, res) => {
  try {
    const path = resolve(
      root,
      "." + new URL(req.url, "http://localhost").pathname,
    );
    if (!path.startsWith(root + sep) && path !== root) {
      res.writeHead(403).end();
      return;
    }
    const bytes =
      req.url === "/"
        ? Buffer.from("<!doctype html><title>Payload contract</title>")
        : await readFile(req.url === "/save-file.js" ? resolve(root, "../src/save-file.js") : path);
    res.setHeader(
      "Content-Type",
      path.endsWith(".wasm")
        ? "application/wasm"
        : path.endsWith(".mjs") || path.endsWith(".js")
          ? "text/javascript"
          : "text/html",
    );
    res.end(bytes);
  } catch {
    res.writeHead(404).end();
  }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const engine = process.env.SUPERSEEDR_TEST_BROWSER || "chromium";
const profile = await mkdtemp(resolve(tmpdir(), "ss-storage-contract-"));
const browser = await ({chromium, firefox, webkit})[engine].launchPersistentContext(profile, { headless: true });
console.log("STORAGE_BROWSER", engine, browser.browser().version());
try {
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  const result = await page.evaluate(async () => {
    const check = (value, message) => {
      if (!value) throw new Error(message);
    };
    const worker = async () => {
      const target = new Worker("/worker.mjs", { type: "module" });
      let serial = 0;
      const pending = new Map();
      await new Promise((resolve, reject) => {
        target.onerror = (event) => reject(new Error(event.message));
        target.onmessage = ({ data }) => {
          if (data.ready) resolve();
        };
      });
      target.onmessage = ({ data }) => {
        const call = pending.get(data.id);
        if (call) {
          pending.delete(data.id);
          data.error
            ? call.reject(new Error(`${call.method}: ${data.error}`))
            : call.resolve(data.result);
        }
      };
      return {
        target,
        call: (method, ...args) =>
          new Promise((resolve, reject) => {
            const id = ++serial;
            pending.set(id, { resolve, reject, method });
            target.postMessage({ id, method, args });
          }),
      };
    };
    window.makeWorker = worker;
    const rejects = async (promise, pattern) => {
      try {
        await promise;
      } catch (error) {
        check(String(error).includes(pattern), `unexpected failure: ${error}`);
        return;
      }
      throw new Error("expected rejection");
    };
    const outcomes = [];
    for (const fallback of [false, true]) {
      const namespace = "v1-" + (fallback ? "73" : "41").repeat(20);
      let owner = await worker();
      await owner.call("open", namespace, fallback, 0);
      check((await owner.call("allocate")) === true, "fresh allocation");
      const other = await worker();
      await rejects(
        other.call("open", namespace, fallback, 0),
        "already owned",
      );
      await rejects(
        other.call("open", "../outside", fallback, 0),
        "invalid torrent namespace",
      );
      await other.call(
        "open",
        "v2-" + (fallback ? "89" : "67").repeat(32),
        fallback,
        0,
      );
      await other.call("allocate");
      await other.call("write", 0, new Uint8Array([55, 66]));
      const sparse = await owner.call("read", 0, 16384);
      check(
        sparse.every((x) => x === 0),
        "sparse/padding/skipped reads",
      );
      const bytes = Uint8Array.from(
        { length: 16384 },
        (_, i) => (i * 17 + (i >>> 5)) & 255,
      );
      await owner.call("write", 0, bytes);
      const expected = bytes.slice();
      expected.fill(0, 4096, 7168);
      let actual = await owner.call("read", 0, 16384);
      check(
        actual.every((x, i) => x === expected[i]),
        `cross-file exact bytes (fallback=${fallback}, first mismatch=${actual.findIndex((x, i) => x !== expected[i])}, first=${Array.from(actual.slice(0, 24))}, around boundary=${Array.from(actual.slice(7168, 7192))})`,
      );
      await rejects(
        owner.call("write", 16380, new Uint8Array(16)),
        "extends past",
      );
      await rejects(owner.call("read", 16385, 0), "extends past");
      await owner.call("fault", fallback);
      await rejects(owner.call("write", 0, new Uint8Array([3])), "StorageFull");
      await owner.call("restore");
      if (!fallback) {
        for (const key of ["read", "write"]) {
          for (const count of [0, -1, 0.5, NaN, Infinity, 17, 4294967288]) {
            await owner.call("count_fault", key, count);
            await rejects(owner.call(key, 0, key === "read" ? 16 : new Uint8Array(16)), `invalid OPFS ${key} count`);
            await owner.call("restore");
            const idle = await owner.call("stats");
            check(idle.count === 0 && idle.bytes === 0, "failed operation releases admission");
          }
          await owner.call("count_fault", key, "partial");
          if (key === "write") await owner.call("write", 0, expected.subarray(0, 128));
          else {
            const partial = await owner.call("read", 0, 128);
            check(partial.every((x, i) => x === expected[i]), "partial read advances exactly");
          }
          await owner.call("restore");
        }
        const partial = await owner.call("read", 0, 128);
        check(partial.every((x, i) => x === expected[i]), "partial write advances exactly");
      }
      const exported = await owner.call("export_file", 0);
      check(exported instanceof File && exported.size === 4096, "file-backed structured clone");
      check(new Uint8Array(await exported.arrayBuffer()).every((x, i) => x === expected[i]), "export bytes");
      const empty = await owner.call("export_file", 1);
      check(empty instanceof File && empty.size === 0, "empty file export");
      for (const index of [2, 4, 99]) await rejects(owner.call("export_file", index), "not exportable");
      const stats = await owner.call("stats");
      check(
        stats.mode === (fallback ? "writable" : "sync"),
        "actual backend mode",
      );
      check(stats.peakHandles <= 2, "handle ceiling");
      await owner.call("cancel_write", 100, new Uint8Array([11, 22, 33, 44]));
      expected.set([11, 22, 33, 44], 100);
      const closingExport = owner.call("export_file", 0);
      await owner.call("close");
      check(new Uint8Array(await (await closingExport).arrayBuffer())[100] === 11, "queued export drains before close and remains readable");
      await rejects(owner.call("export_file", 0), "closed");
      await owner.call("close");
      owner.target.terminate();
      owner = await worker();
      await owner.call("open", namespace, fallback, 0);
      actual = await owner.call("read", 0, 16384);
      check(
        actual.every((x, i) => x === expected[i]),
        "cancelled write survives close/reopen",
      );
      await owner.call("close");
      await rejects(
        owner.call("open", namespace, fallback, 1),
        "layout mismatch",
      );
      await owner.call("open", namespace, fallback, 0);
      const burst = await owner.call("burst");
      check(
        burst.count <= 32 && burst.bytes <= 64 * 1024 * 1024,
        "queue ceiling",
      );
      await owner.call("close");
      owner.target.terminate();
      owner = await worker();
      await owner.call("open", namespace, fallback, 0);
      actual = await owner.call("read", 0, 4096);
      check(
        actual.every((x) => x === 31),
        "admitted cancelled writes finish in order",
      );
      // Abrupt worker loss releases browser locks/handles; flushed data remains recheckable.
      owner.target.terminate();
      await new Promise((resolve) => setTimeout(resolve, 100));
      owner = await worker();
      await owner.call("open", namespace, fallback, 0);
      actual = await owner.call("read", 0, 4096);
      check(
        actual.every((x) => x === 31),
        "retained bytes after worker loss",
      );
      await owner.call("cancel_write", 0, new Uint8Array([9]));
      const deletingExport = owner.call("export_file", 0);
      await owner.call("remove");
      check((await deletingExport) instanceof File, "admitted export settles before removal");
      await rejects(owner.call("export_file", 0), "closed");
      owner.target.terminate();
      owner = await worker();
      await owner.call("open", namespace, fallback, 0);
      check((await owner.call("allocate")) === true, "scoped deletion");
      await owner.call("remove");
      owner.target.terminate();
      check(
        (await other.call("read", 0, 2)).join(",") === "55,66",
        "deletion preserves other torrent namespace",
      );
      await other.call("remove");
      other.target.terminate();
      outcomes.push({
        mode: stats.mode,
        peakHandles: stats.peakHandles,
        contracts: "passed",
      });
    }
    return outcomes;
  });
  assert.equal(result.length, 2);
  console.log(JSON.stringify(result, null, 2));
  // Real >64 MiB download through the production backend and the page's save
  // helper. Generate and write bounded chunks; hash the saved stream in Node.
  const length = Number(process.env.SUPERSEEDR_TEST_EXPORT_BYTES || 65 * 1024 * 1024 + 37);
  const expected = createHash('sha256');
  const chunkSize = 1024 * 1024;
  for (let offset = 0; offset < length; offset += chunkSize) {
    const chunk = Buffer.alloc(Math.min(chunkSize, length - offset));
    for (let i = 0; i < chunk.length; i++) chunk[i] = (i * 17 + (i >>> 5) + offset / chunkSize) & 255;
    expected.update(chunk);
  }
  await page.evaluate(async ({length, chunkSize}) => {
    const owner = window.exportOwner = await window.makeWorker();
    await owner.call('open_file', 'v1-' + 'a5'.repeat(20), length);
    await owner.call('allocate');
    // A sparse, unfilled physical file cannot masquerade as a complete export.
    try { await owner.call('export_file', 0); throw Error('short file accepted'); }
    catch (error) { if (!String(error).includes('length mismatch')) throw error; }
    for (let offset = 0; offset < length; offset += chunkSize) {
      const chunk = new Uint8Array(Math.min(chunkSize, length - offset));
      for (let i = 0; i < chunk.length; i++) chunk[i] = (i * 17 + (i >>> 5) + offset / chunkSize) & 255;
      await owner.call('write', offset, chunk);
    }
    const {saveFile} = await import('/save-file.js');
    window.showSaveFilePicker = undefined;
    const button = document.createElement('button'); button.textContent = 'Save generated file';
    button.onclick = () => { window.saveOutcome = saveFile({path: 'generated-payload.bin', length}, {
      read: () => { throw Error('fallback must not assemble bytes'); },
      exportFile: async () => {
        const file = await owner.call('export_file', 0);
        await owner.call('read', 0, 16); // reopen pooled sync handle, as seeding does
        return file;
      },
    }); };
    document.body.append(button);
  }, {length, chunkSize});
  const completed = page.waitForEvent('download', {timeout: 120000});
  await page.getByRole('button', {name: 'Save generated file'}).click();
  const download = await completed;
  const actual = createHash('sha256');
  for await (const chunk of await download.createReadStream()) actual.update(chunk);
  assert.equal(actual.digest('hex'), expected.digest('hex'));
  assert.equal(await page.evaluate(() => window.saveOutcome), 'download_started');
  assert.equal(download.suggestedFilename(), 'generated-payload.bin');
  await page.evaluate(async ({length, chunkSize}) => {
    const owner = window.exportOwner;
    for (const offset of [0, Math.min(32 * chunkSize, Math.floor((length - 1) / chunkSize) * chunkSize), Math.floor((length - 1) / chunkSize) * chunkSize]) {
      const chunk = await owner.call('read', offset, Math.min(37, length - offset));
      if (!chunk.every((x, i) => x === ((i * 17 + (i >>> 5) + offset / chunkSize) & 255)))
        throw Error('retained source mismatch');
    }
    await owner.call('remove'); owner.target.terminate();
  }, {length, chunkSize});
  await download.delete();
  console.log('FILE_BACKED_DOWNLOAD_AND_RETAINED_READS_PASSED', {engine, length});
} finally {
  await browser.close();
  await rm(profile, {recursive: true, force: true});
  await new Promise((resolve) => server.close(resolve));
}
