// SPDX-License-Identifier: GPL-3.0-or-later
// Fetch only the independent browser distribution; no peer-client dependencies
// or install scripts run in our application dependency tree.
import {createHash} from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {mkdir, readFile, writeFile} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';

const integrity = 'PFgLphma0dsUWmbWrZ016Cja+j+3D3DXuhk09A5u9qVAzH1T4Vj1VZd8j+zb8sDwPv7NA7+qgPtDEN4iFd6wdw==';
const url = 'https://registry.npmjs.org/webtorrent/-/webtorrent-3.0.21.tgz';
export async function preparePeerClient() {
  const directory = new URL('../../target/browser-contract-peer/', import.meta.url);
  await mkdir(directory, {recursive: true});
  const archive = new URL('webtorrent-3.0.21.tgz', directory);
  let bytes;
  try { bytes = await readFile(archive); }
  catch (error) {
    if (error.code !== 'ENOENT') throw error;
    const response = await fetch(url, {signal: AbortSignal.timeout(60000)});
    if (!response.ok) throw Error(`Independent peer download failed: ${response.status}`);
    bytes = Buffer.from(await response.arrayBuffer());
  }
  if (createHash('sha512').update(bytes).digest('base64') !== integrity) {
    throw Error('Independent peer archive integrity mismatch');
  }
  await writeFile(archive, bytes);
  const bundle = execFileSync('tar', ['-xOf', fileURLToPath(archive), 'package/dist/webtorrent.min.js'], {maxBuffer: 16 * 1024 * 1024});
  const output = new URL('webtorrent.min.js', directory);
  await writeFile(output, bundle);
  return fileURLToPath(output);
}
