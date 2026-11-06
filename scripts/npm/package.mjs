import { chmodSync, copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
export const platforms = ['darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64', 'win32-x64'];
export const version = readFileSync(resolve(root, 'Cargo.toml'), 'utf8').match(/^version = "([^"]+)"/m)[1];

export function packDirectory(output, platform, binary, releaseVersion = version) {
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(releaseVersion)) throw new Error(`Invalid npm release version: ${releaseVersion}`);
  if (platform !== 'launcher' && !platforms.includes(platform)) throw new Error(`Unsupported platform: ${platform}`);
  if (platform !== 'launcher' && !binary) throw new Error('A native binary path is required');
  const name = platform === 'launcher' ? 'superseedr' : `superseedr-${platform}`;
  const directory = resolve(output, name);
  mkdirSync(directory, { recursive: true });
  const manifest = {
    name, version: releaseVersion, description: 'A BitTorrent client in your terminal.',
    license: 'GPL-3.0-or-later', repository: { type: 'git', url: 'git+https://github.com/Jagalite/superseedr.git' },
    engines: { node: '>=22' }, publishConfig: { access: 'public' },
  };
  if (platform === 'launcher') {
    manifest.bin = { superseedr: 'cli.cjs' };
    manifest.files = ['cli.cjs'];
    manifest.optionalDependencies = Object.fromEntries(platforms.map(p => [`superseedr-${p}`, releaseVersion]));
    copyFileSync(resolve(root, 'npm/cli.cjs'), resolve(directory, 'cli.cjs'));
    chmodSync(resolve(directory, 'cli.cjs'), 0o755);
  } else {
    const [os, cpu] = platform.split('-');
    Object.assign(manifest, { os: [os], cpu: [cpu], files: ['bin/'] });
    if (os === 'linux') manifest.libc = ['glibc'];
    mkdirSync(resolve(directory, 'bin'), { recursive: true });
    const destination = resolve(directory, `bin/superseedr${os === 'win32' ? '.exe' : ''}`);
    copyFileSync(binary, destination);
    chmodSync(destination, 0o755);
  }
  copyFileSync(resolve(root, 'LICENSE'), resolve(directory, 'LICENSE'));
  copyFileSync(resolve(root, platform === 'launcher' ? 'README.md' : 'npm/README.md'), resolve(directory, 'README.md'));
  writeFileSync(resolve(directory, 'package.json'), JSON.stringify(manifest, null, 2) + '\n');
  return directory;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  const versionFlag = args.find(arg => arg.startsWith('--version='));
  const [output, platform, binary] = args.filter(arg => arg !== versionFlag);
  if (!output || !platform) throw new Error('Usage: node scripts/npm/package.mjs OUTPUT launcher|PLATFORM [BINARY] [--version=VERSION]');
  console.log(packDirectory(output, platform, binary, versionFlag?.slice('--version='.length)));
}
