import { gzipSync } from "node:zlib";
import { readdir, readFile, stat } from "node:fs/promises";
import { extname, join, relative } from "node:path";

const DIST_DIR = new URL("../dist/", import.meta.url);
const BUDGETS = {
  wasmRaw: 2_550_000, // Includes Show's 30-scene renderer.
  wasmGzip: 900_000,
  javascriptRaw: 700_000,
  javascriptGzip: 220_000,
  totalGzip: 1_150_000,
};
const SERVER_EXTENSIONS = new Set([".php", ".py", ".rb", ".rs", ".toml"]);

async function filesUnder(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = [];
  for (const entry of entries) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) files.push(...(await filesUnder(path)));
    else if (entry.isFile()) files.push(path);
    else throw new Error(`dist contains a non-static entry: ${path}`);
  }
  return files;
}

function assertBudget(label, actual, maximum) {
  if (actual > maximum) {
    throw new Error(`${label} is ${actual} bytes; budget is ${maximum} bytes`);
  }
}

const distPath = DIST_DIR.pathname;
const files = await filesUnder(distPath);
if (!files.some((path) => relative(distPath, path) === "index.html")) {
  throw new Error("dist/index.html is missing");
}

const wasmFiles = files.filter((path) => extname(path) === ".wasm");
if (wasmFiles.length !== 1) {
  throw new Error(`expected exactly one WASM asset, found ${wasmFiles.length}`);
}
for (const path of files) {
  if (SERVER_EXTENSIONS.has(extname(path))) {
    throw new Error(`dist contains a server-side source file: ${relative(distPath, path)}`);
  }
}

const measurements = await Promise.all(
  files.map(async (path) => {
    const bytes = await readFile(path);
    return {
      path: relative(distPath, path),
      raw: (await stat(path)).size,
      gzip: gzipSync(bytes, { level: 9 }).length,
    };
  }),
);
const wasm = measurements.find((entry) => entry.path.endsWith(".wasm"));
const javascript = measurements.filter((entry) => entry.path.endsWith(".js"));
const javascriptRaw = javascript.reduce((total, entry) => total + entry.raw, 0);
const javascriptGzip = javascript.reduce((total, entry) => total + entry.gzip, 0);
const totalGzip = measurements.reduce((total, entry) => total + entry.gzip, 0);

assertBudget("WASM raw size", wasm.raw, BUDGETS.wasmRaw);
assertBudget("WASM gzip size", wasm.gzip, BUDGETS.wasmGzip);
assertBudget("JavaScript raw size", javascriptRaw, BUDGETS.javascriptRaw);
assertBudget("JavaScript gzip size", javascriptGzip, BUDGETS.javascriptGzip);
assertBudget("Total static gzip size", totalGzip, BUDGETS.totalGzip);

console.log(
  JSON.stringify(
    {
      files: measurements.length,
      wasmRaw: wasm.raw,
      wasmGzip: wasm.gzip,
      javascriptRaw,
      javascriptGzip,
      totalGzip,
      budgets: BUDGETS,
    },
    null,
    2,
  ),
);
