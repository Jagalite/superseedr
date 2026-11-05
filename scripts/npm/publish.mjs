import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';

const registry = 'https://registry.npmjs.org';

export async function lookupIntegrity(name, version, fetchPackage = fetch) {
  const response = await fetchPackage(`${registry}/${encodeURIComponent(name)}/${encodeURIComponent(version)}`);
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(`Cannot check ${name}@${version}: registry HTTP ${response.status}`);
  const manifest = await response.json();
  if (typeof manifest.dist?.integrity !== 'string' || !manifest.dist.integrity) {
    throw new Error(`Cannot check ${name}@${version}: registry integrity is missing`);
  }
  return manifest.dist.integrity;
}

export async function publishPackages(packages, { lookup = lookupIntegrity, publish, log = console.log }) {
  // Check the entire set before making changes, including already-published versions.
  const pending = [];
  for (const pkg of packages) {
    const existing = await lookup(pkg.name, pkg.version);
    if (existing === null) pending.push(pkg);
    else if (existing === pkg.integrity) log(`Already published, integrity verified: ${pkg.name}@${pkg.version}`);
    else throw new Error(`Integrity mismatch for ${pkg.name}@${pkg.version}; refusing to continue`);
  }
  // Callers supply platform packages first and the launcher last.
  for (const pkg of pending) await publish(pkg);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const files = process.argv.slice(2);
  if (!files.length) throw new Error('Usage: node scripts/npm/publish.mjs TARBALL... (launcher last)');
  const packages = files.map(file => {
    const tarball = resolve(file);
    const [metadata] = JSON.parse(execFileSync('npm', ['pack', tarball, '--dry-run', '--json', '--ignore-scripts'], { encoding: 'utf8' }));
    const integrity = `sha512-${createHash('sha512').update(readFileSync(tarball)).digest('base64')}`;
    if (metadata.integrity !== integrity) throw new Error(`Tarball integrity mismatch: ${tarball}`);
    return { name: metadata.name, version: metadata.version, integrity, tarball };
  });
  await publishPackages(packages, {
    publish: pkg => execFileSync('npm', ['publish', pkg.tarball, '--access', 'public', '--registry', registry], { stdio: 'inherit' }),
  });
}
