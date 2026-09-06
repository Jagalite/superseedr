// SPDX-License-Identifier: GPL-3.0-or-later
// Physical OPFS execution only; the torrent manager owns validity and scheduling.
const fail = (name, message) => {
  throw new DOMException(message, name);
};
const identity = (layout) =>
  JSON.stringify(
    layout.files.map((f) => [
      f.path,
      f.length,
      f.global_start_offset,
      f.is_padding,
    ]),
  );
export async function openPayload(namespace, serialized, fallback) {
  if (!/^(v1-[0-9a-f]{40}|v2-[0-9a-f]{64})$/.test(namespace))
    fail("DataError", "invalid torrent namespace");
  if (!navigator.locks || !navigator.storage?.getDirectory)
    fail("NotSupportedError", "OPFS and Web Locks required");
  const layout = JSON.parse(serialized);
  let end = 0;
  const paths = new Set();
  for (const file of layout.files) {
    if (
      !Number.isSafeInteger(file.length) ||
      file.length < 0 ||
      file.global_start_offset !== end ||
      paths.has(file.path)
    )
      fail("DataError", "invalid payload layout");
    paths.add(file.path);
    end += file.length;
    if (!Number.isSafeInteger(end))
      fail("DataError", "payload exceeds precise browser offsets");
  }
  if (end !== layout.total_size) fail("DataError", "invalid total length");
  let release, resolveLock, rejectLock;
  const acquired = new Promise((resolve, reject) => {
    resolveLock = resolve;
    rejectLock = reject;
  });
  const held = new Promise((resolve) => {
    release = resolve;
  });
  const lockTask = navigator.locks.request(
    `superseedr:payload:${namespace}`,
    { mode: "exclusive", ifAvailable: true },
    async (lock) => {
      if (!lock) {
        rejectLock(
          new DOMException(
            "torrent payload already owned",
            "InvalidStateError",
          ),
        );
        return;
      }
      resolveLock();
      await held;
    },
  );
  lockTask.catch(rejectLock);
  await acquired;
  try {
    const root = await (
      await navigator.storage.getDirectory()
    ).getDirectoryHandle("superseedr-payload-v1", { create: true });
    const directory = await root.getDirectoryHandle(namespace, {
      create: true,
    });
    const manifest = await directory.getFileHandle("layout.json", {
      create: true,
    });
    const old = await (await manifest.getFile()).text(),
      key = identity(layout);
    if (old && old !== key) fail("DataError", "namespace layout mismatch");
    if (!old) {
      for await (const name of directory.keys())
        if (name !== "layout.json")
          fail("DataError", "unidentified payload requires recovery");
      const writer = await manifest.createWritable();
      try {
        await writer.write(key);
        await writer.close();
      } catch (error) {
        await writer.abort().catch(() => {});
        throw error;
      }
    }
    return new Store(root, directory, namespace, layout, fallback, async () => {
      release();
      await lockTask;
    });
  } catch (error) {
    release();
    await lockTask;
    throw error;
  }
}
class Store {
  constructor(root, directory, namespace, layout, fallback, unlock) {
    Object.assign(this, {
      root,
      directory,
      namespace,
      layout,
      fallback,
      unlock,
    });
    this.handles = new Map();
    this.tail = Promise.resolve();
    this.count = 0;
    this.bytes = 0;
    this.sealed = false;
    this.terminal = null;
    this.peakHandles = 0;
    this.mode = null;
  }
  submit(serialized, input) {
    const op = JSON.parse(serialized),
      terminal = op.kind === "close" || op.kind === "remove";
    if (this.sealed)
      return op.kind === "close"
        ? this.terminal || Promise.resolve(null)
        : Promise.reject(
            new DOMException("payload closed", "InvalidStateError"),
          );
    const size = op.kind === "read" ? op.length : input.byteLength;
    if (!Number.isSafeInteger(size) || size < 0 || size > 32 * 1024 * 1024)
      return Promise.reject(
        new DOMException("invalid operation size", "DataError"),
      );
    if (!terminal && (this.count >= 32 || this.bytes + size > 64 * 1024 * 1024))
      return Promise.reject(
        Object.assign(new Error("payload admission full"), {
          name: "BusyError",
        }),
      );
    if (terminal) this.sealed = true;
    const bytes = new Uint8Array(input);
    this.count++;
    this.bytes += size;
    const complete = this.tail
      .then(() => this.execute(op, bytes))
      .finally(() => {
        this.count--;
        this.bytes -= size;
      });
    this.tail = complete.catch(() => {});
    if (terminal) this.terminal = complete;
    return complete;
  }
  async handle(index, create) {
    let entry = this.handles.get(index);
    if (entry) {
      this.handles.delete(index);
      this.handles.set(index, entry);
      return entry;
    }
    if (this.handles.size === 2) {
      const [old, item] = this.handles.entries().next().value;
      item.sync?.close();
      this.handles.delete(old);
    }
    const file = await this.directory.getFileHandle(`file-${index}`, {
      create,
    });
    const sync =
      !this.fallback && typeof file.createSyncAccessHandle === "function"
        ? await file.createSyncAccessHandle()
        : null;
    this.mode = sync ? "sync" : "writable";
    entry = { file, sync };
    this.handles.set(index, entry);
    this.peakHandles = Math.max(this.peakHandles, this.handles.size);
    return entry;
  }
  async length(index) {
    const entry = await this.handle(index, false);
    return entry.sync
      ? entry.sync.getSize()
      : (await entry.file.getFile()).size;
  }
  async write(index, offset, bytes) {
    const entry = await this.handle(index, true);
    if (entry.sync) {
      let done = 0;
      while (done < bytes.length) {
        const count = entry.sync.write(bytes.subarray(done), {
          at: offset + done,
        });
        if (!Number.isSafeInteger(count) || count <= 0 || count > bytes.length - done)
          fail("UnknownError", "invalid OPFS write count");
        done += count;
      }
      entry.sync.flush();
    } else {
      const writer = await entry.file.createWritable({
        keepExistingData: true,
      });
      try {
        // WebKit's writable stream can ignore a typed-array view's byteOffset.
        // Give it exactly this bounded span, starting at zero in its own buffer.
        const data = bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength
          ? bytes : bytes.slice();
        await writer.write({ type: "write", position: offset, data });
        await writer.close();
      } catch (error) {
        await writer.abort().catch(() => {});
        throw error;
      }
    }
  }
  async resize(index, length) {
    const entry = await this.handle(index, true);
    if (entry.sync) {
      entry.sync.truncate(length);
      entry.sync.flush();
    } else {
      const writer = await entry.file.createWritable({
        keepExistingData: true,
      });
      try {
        await writer.truncate(length);
        await writer.close();
      } catch (error) {
        await writer.abort().catch(() => {});
        throw error;
      }
    }
  }
  async execute(op, bytes) {
    switch (op.kind) {
      case "allocate": {
        let fresh = true;
        for (let i = 0; i < this.layout.files.length; i++) {
          if (this.layout.files[i].is_padding) continue;
          try {
            if ((await this.length(i)) > 0) fresh = false;
          } catch (error) {
            if (error.name !== "NotFoundError") throw error;
          }
        }
        for (let i = 0; i < op.layout.files.length; i++) {
          const file = op.layout.files[i];
          if (file.is_padding || file.is_skipped) continue;
          await this.handle(i, true);
          const length = await this.length(i);
          if (length !== file.length && (!fresh || length > 0))
            await this.resize(i, file.length);
        }
        return fresh;
      }
      case "browser_file": {
        const index = op.file_index, file = op.layout.files[index];
        if (!Number.isSafeInteger(index) || index < 0 || !file || file.is_padding || file.is_skipped)
          fail("DataError", "file is not exportable");
        // Serialize behind physical writes, flush and release only this pooled handle.
        // Subsequent upload reads can reopen it without owning the download's File.
        const entry = await this.handle(index, false);
        entry.sync?.flush();
        entry.sync?.close();
        this.handles.delete(index);
        const snapshot = await entry.file.getFile();
        if (snapshot.size !== file.length) fail("DataError", "export file length mismatch");
        return snapshot;
      }
      case "inspect": {
        const i = this.layout.files.findIndex(
          (f) => f.path === op.path && !f.is_padding,
        );
        if (i < 0) fail("NotFoundError", "file outside payload manifest");
        return { is_file: true, length: await this.length(i) };
      }
      case "read":
      case "write": {
        const output = op.kind === "read" ? new Uint8Array(op.length) : null;
        for (const span of op.spans) {
          if (span.padding) continue;
          if (op.kind === "write")
            await this.write(
              span.index,
              span.local,
              bytes.subarray(span.position, span.position + span.length),
            );
          else {
            let entry;
            try {
              entry = await this.handle(span.index, false);
            } catch (error) {
              if (span.skipped && error.name === "NotFoundError") continue;
              throw error;
            }
            const target = output.subarray(
              span.position,
              span.position + span.length,
            );
            if (entry.sync) {
              const available = Math.min(
                target.length,
                Math.max(0, entry.sync.getSize() - span.local),
              );
              let done = 0;
              while (done < available) {
                const count = entry.sync.read(
                  target.subarray(done, available),
                  { at: span.local + done },
                );
                if (!Number.isSafeInteger(count) || count <= 0 || count > available - done)
                  fail("UnknownError", "invalid OPFS read count");
                done += count;
              }
            } else
              target.set(
                new Uint8Array(
                  await (await entry.file.getFile())
                    .slice(span.local, span.local + target.length)
                    .arrayBuffer(),
                ),
              );
          }
        }
        return output;
      }
      case "close":
      case "remove": {
        try {
          if (op.kind === "remove") {
            const allowed = new Set(
              this.layout.files.filter((f) => !f.is_padding).map((f) => f.path),
            );
            if (
              op.files.length !== allowed.size ||
              op.files.some((path) => !allowed.delete(path))
            )
              fail(
                "DataError",
                "removal must name the complete torrent payload",
              );
          }
          for (const entry of this.handles.values()) entry.sync?.close();
          this.handles.clear();
          if (op.kind === "remove")
            await this.root.removeEntry(this.namespace, { recursive: true });
          return null;
        } finally {
          for (const entry of this.handles.values()) {
            try {
              entry.sync?.close();
            } catch {}
          }
          this.handles.clear();
          await this.unlock();
        }
      }
      default:
        fail("DataError", "unknown payload operation");
    }
  }
}
export function submitPayload(store, serialized, data) {
  return store.submit(serialized, data);
}
export function payloadStats(store) {
  return {
    mode: store.mode,
    handles: store.handles.size,
    peakHandles: store.peakHandles,
    count: store.count,
    bytes: store.bytes,
  };
}

// Recovery after a manager has terminated: acquire the same ownership lock before
// deleting its namespace, including when metadata never became available.
export async function removeClosedPayload(namespace) {
  if (!/^(v1-[0-9a-f]{40}|v2-[0-9a-f]{64})$/.test(namespace))
    fail("DataError", "invalid torrent namespace");
  await navigator.locks.request(`superseedr:payload:${namespace}`, {mode: "exclusive", ifAvailable: true}, async lock => {
    if (!lock) fail("InvalidStateError", "torrent payload already owned");
    try {
      const root = await (await navigator.storage.getDirectory()).getDirectoryHandle("superseedr-payload-v1");
      await root.removeEntry(namespace, {recursive: true});
    } catch (error) {
      if (error.name !== "NotFoundError") throw error;
    }
  });
}
