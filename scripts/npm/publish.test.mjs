import { test } from 'node:test';
import assert from 'node:assert/strict';
import { lookupIntegrity, publishPackages } from './publish.mjs';

const packages = ['native-a', 'native-b', 'launcher'].map(name => ({ name, version: '1.2.3', integrity: `sha512-${name}` }));

test('retry resumes after partial failure and publishes launcher last', async () => {
  const registry = new Map();
  const uploaded = [];
  let fail = true;
  const options = {
    log: () => {},
    lookup: async name => registry.get(name) ?? null,
    publish: async pkg => {
      if (pkg.name === 'native-b' && fail) throw new Error('Publish rejected');
      assert.ok(!registry.has(pkg.name), 'Never republish an existing version');
      registry.set(pkg.name, pkg.integrity);
      uploaded.push(pkg.name);
    },
  };
  await assert.rejects(publishPackages(packages, options), /Publish rejected/);
  assert.deepEqual(uploaded, ['native-a']);
  fail = false;
  await publishPackages(packages, options);
  assert.deepEqual(uploaded, ['native-a', 'native-b', 'launcher']);
  await publishPackages(packages, options);
  assert.equal(uploaded.length, 3, 'A fully published release is a no-op');
});

test('conflicting immutable version aborts before any uploads', async () => {
  const uploaded = [];
  await assert.rejects(publishPackages(packages, {
    log: () => {},
    lookup: async name => name === 'native-b' ? 'sha512-other' : null,
    publish: async pkg => uploaded.push(pkg),
  }), /Integrity mismatch/);
  assert.deepEqual(uploaded, []);
});

test('registry lookup treats only 404 as missing and rejects ambiguous failures', async () => {
  assert.equal(await lookupIntegrity('native-a', '1.2.3', async () => ({ status: 404 })), null);
  assert.equal(await lookupIntegrity('native-a', '1.2.3', async () => ({ status: 200, ok: true, json: async () => ({ dist: { integrity: 'sha512-match' } }) })), 'sha512-match');
  for (const status of [401, 403, 429, 500]) {
    await assert.rejects(lookupIntegrity('native-a', '1.2.3', async () => ({ status, ok: false })), /registry HTTP/);
  }
  await assert.rejects(lookupIntegrity('native-a', '1.2.3', async () => ({ status: 200, ok: true, json: async () => ({}) })), /integrity is missing/);
  await assert.rejects(lookupIntegrity('native-a', '1.2.3', async () => { throw new Error('Network unavailable'); }), /Network unavailable/);
});

test('registry errors prevent all uploads, even with earlier missing packages', async () => {
  const uploaded = [];
  await assert.rejects(publishPackages(packages, {
    lookup: async name => {
      if (name === 'native-b') throw new Error('Registry unavailable');
      return null;
    },
    publish: async pkg => uploaded.push(pkg),
  }), /Registry unavailable/);
  assert.deepEqual(uploaded, []);
});
