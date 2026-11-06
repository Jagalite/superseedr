import { test } from 'node:test';
import assert from 'node:assert/strict';
import { chmodSync, mkdtempSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { packDirectory, platforms, version } from './package.mjs';

const npm = process.platform === 'win32' ? 'npm.cmd' : 'npm';
function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: 'utf8', timeout: 60000, ...options });
  assert.equal(result.status, 0, `${result.error || ''}\n${result.stdout}\n${result.stderr}`);
  return result.stdout;
}

test('packed global install forwards arguments, streams, exit codes and termination', { timeout: 60000 }, async () => {
  const temp = mkdtempSync(resolve(tmpdir(), 'native-cli-'));
  try {
    const platform = `${process.platform}-${process.arch}`;
    assert.ok(platforms.includes(platform));
    // Use the runtime as a portable executable fixture, including on Windows.
    const native = packDirectory(temp, platform, process.execPath);
    const launcher = packDirectory(temp, 'launcher');
    const fixture = resolve(temp, 'fixture.cjs');
    writeFileSync(fixture, `if (process.argv[2] === 'wait') { console.log('ready'); setInterval(() => {}, 1000); } else { console.log(JSON.stringify(process.argv.slice(2))); console.error('fixture stderr'); process.exit(7); }`);
    const tarballs = [native, launcher].map(cwd => {
      const result = JSON.parse(run(npm, ['pack', '--json', '--ignore-scripts'], { cwd, shell: process.platform === 'win32' }));
      return resolve(cwd, result[0].filename);
    });
    const prefix = resolve(temp, 'prefix');
    run(npm, ['install', '--global', '--prefix', prefix, '--offline', '--ignore-scripts', '--no-audit', '--no-fund', ...tarballs], { shell: process.platform === 'win32' });
    const modules = process.platform === 'win32' ? resolve(prefix, 'node_modules') : resolve(prefix, 'lib/node_modules');
    const cli = resolve(modules, 'superseedr/cli.cjs');
    const args = ['a b', '--flag', 'λ', '$(literal)'];
    const result = spawnSync(process.execPath, [cli, fixture, ...args], { encoding: 'utf8' });
    assert.equal(result.status, 7);
    assert.deepEqual(JSON.parse(result.stdout), args);
    assert.match(result.stderr, /fixture stderr/);
    if (process.platform !== 'win32') {
      const child = spawn(process.execPath, [cli, fixture, 'wait'], { timeout: 10000 });
      await new Promise((resolveReady, reject) => {
        child.once('error', reject);
        child.stdout.once('data', resolveReady);
        child.once('exit', code => reject(new Error(`Premature exit: ${code}`)));
      });
      const exited = new Promise(resolveExit => child.once('exit', (code, signal) => resolveExit({ code, signal })));
      child.kill('SIGTERM');
      assert.deepEqual(await exited, { code: null, signal: 'SIGTERM' });
    }
    if (process.platform !== 'win32') {
      chmodSync(resolve(modules, `superseedr-${platform}/bin/superseedr`), 0o644);
      const denied = spawnSync(process.execPath, [cli], { encoding: 'utf8', timeout: 5000 });
      assert.equal(denied.status, 1);
      assert.match(denied.stderr, /could not start native binary/);
    }
    rmSync(resolve(modules, `superseedr-${platform}`), { recursive: true });
    const missing = spawnSync(process.execPath, [cli], { encoding: 'utf8' });
    assert.equal(missing.status, 1);
    assert.match(missing.stderr, /optional dependencies enabled/);
  } finally { rmSync(temp, { recursive: true, force: true }); }
});

test('all target manifests pin matching versions and reject unknown targets', () => {
  const temp = mkdtempSync(resolve(tmpdir(), 'native-manifest-'));
  try {
    assert.match(version, /^\d+\.\d+\.\d+/);
    for (const platform of platforms) {
      const directory = packDirectory(temp, platform, process.execPath);
      const manifest = JSON.parse(readFileSync(resolve(directory, 'package.json')));
      assert.equal(manifest.version, version);
      assert.deepEqual(manifest.os, [platform.split('-')[0]]);
      assert.deepEqual(manifest.cpu, [platform.split('-')[1]]);
      if (platform.startsWith('linux-')) assert.deepEqual(manifest.libc, ['glibc']);
    }
    const launcher = packDirectory(temp, 'launcher');
    const manifest = JSON.parse(readFileSync(resolve(launcher, 'package.json')));
    assert.deepEqual(manifest.optionalDependencies, Object.fromEntries(platforms.map(p => [`superseedr-${p}`, version])));
    const historical = packDirectory(temp, 'launcher', undefined, '1.0.16');
    const oldManifest = JSON.parse(readFileSync(resolve(historical, 'package.json')));
    assert.equal(oldManifest.version, '1.0.16');
    assert.ok(Object.values(oldManifest.optionalDependencies).every(v => v === '1.0.16'));
    assert.throws(() => packDirectory(temp, 'launcher', undefined, 'invalid'), /Invalid npm release version/);
    assert.throws(() => packDirectory(temp, 'unsupported', process.execPath), /Unsupported platform/);
  } finally { rmSync(temp, { recursive: true, force: true }); }
});
