// SPDX-License-Identifier: GPL-3.0-or-later
import { chromium } from "@playwright/test";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { resolve, sep } from "node:path";
import assert from "node:assert/strict";
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
        : await readFile(path);
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
const browser = await chromium.launch({ headless: true });
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
            ? call.reject(new Error(data.error))
            : call.resolve(data.result);
        }
      };
      return {
        target,
        call: (method, ...args) =>
          new Promise((resolve, reject) => {
            const id = ++serial;
            pending.set(id, { resolve, reject });
            target.postMessage({ id, method, args });
          }),
      };
    };
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
        "cross-file exact bytes",
      );
      await rejects(
        owner.call("write", 16380, new Uint8Array(16)),
        "extends past",
      );
      await rejects(owner.call("read", 16385, 0), "extends past");
      await owner.call("fault", fallback);
      await rejects(owner.call("write", 0, new Uint8Array([3])), "StorageFull");
      await owner.call("restore");
      const stats = await owner.call("stats");
      check(
        stats.mode === (fallback ? "writable" : "sync"),
        "actual backend mode",
      );
      check(stats.peakHandles <= 2, "handle ceiling");
      await owner.call("cancel_write", 100, new Uint8Array([11, 22, 33, 44]));
      expected.set([11, 22, 33, 44], 100);
      await owner.call("close");
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
      await owner.call("remove");
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
} finally {
  await browser.close();
  await new Promise((resolve) => server.close(resolve));
}
