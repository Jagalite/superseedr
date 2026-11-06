// Packs and installs real binaries without accessing the npm registry.
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { resolve } from 'node:path';
import { tmpdir } from 'node:os';
import assert from 'node:assert/strict';

const output = resolve(process.argv[2]);
const { version } = JSON.parse(readFileSync(resolve(output, 'superseedr/package.json')));
const npm = process.platform === 'win32' ? 'npm.cmd' : 'npm';
const prefix = mkdtempSync(resolve(tmpdir(), 'native-smoke-'));
function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: 'utf8', timeout: 60000, ...options });
  if (result.status !== 0) throw new Error(`${command}: ${result.error || ''}\n${result.stdout}\n${result.stderr}`);
  return result.stdout;
}
try {
  const tarballs = readdirSync(output).map(name => {
    const cwd = resolve(output, name);
    const packed = JSON.parse(run(npm, ['pack', '--json', '--ignore-scripts'], { cwd, shell: process.platform === 'win32' }));
    return resolve(cwd, packed[0].filename);
  });
  run(npm, ['install', '--global', '--prefix', prefix, '--offline', '--ignore-scripts', '--no-audit', '--no-fund', ...tarballs], { shell: process.platform === 'win32' });
  const command = process.platform === 'win32' ? resolve(prefix, 'superseedr.cmd') : resolve(prefix, 'bin/superseedr');
  assert.equal(run(command, ['--version'], { shell: process.platform === 'win32' }).trim(), `superseedr ${version}`);
  assert.match(run(command, ['--help'], { shell: process.platform === 'win32' }), /Usage: superseedr/);
  console.log(`Packed install, --version (${version}), and --help passed.`);
} finally { rmSync(prefix, { recursive: true, force: true }); }
