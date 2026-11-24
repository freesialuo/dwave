import assert from 'node:assert/strict';
import { test } from 'node:test';
import { parseRelease, publishOnce } from './npm-release-decisions.mjs';

test('rejects malformed tags, leading zeroes, and build metadata', () => {
  for (const tag of ['1.2.3', 'v1.2', 'v01.2.3', 'v1.2.3-rc.01', 'v1.2.3+local', 'v1.2.3\n']) {
    assert.throws(() => parseRelease(tag, tag.slice(1)), /Expected a v-prefixed SemVer/);
  }
});

test('rejects a tag that differs from the package version', () => {
  assert.throws(() => parseRelease('v1.2.3', '1.2.4'), /does not match npm package version 1\.2\.4/);
});

test('publishes stable releases to latest and prereleases to next', () => {
  assert.deepEqual(parseRelease('v1.2.3', '1.2.3'), { version: '1.2.3', dist_tag: 'latest' });
  assert.deepEqual(parseRelease('v1.2.3-rc.1', '1.2.3-rc.1'), { version: '1.2.3-rc.1', dist_tag: 'next' });
});

const packageVersion = 'dwave@1.2.3';
const integrity = 'sha512-expected';

test('rejects an existing version with different integrity before publishing', async t => {
  const lookup = t.mock.fn(() => 'sha512-conflict');
  const publish = t.mock.fn();
  await assert.rejects(publishOnce({ packageVersion, integrity, lookup, publish }), /already exists with a different tarball/);
  assert.equal(publish.mock.callCount(), 0);
});

test('treats an existing version with the same integrity as a no-op', async t => {
  const lookup = t.mock.fn(() => integrity);
  const publish = t.mock.fn();
  assert.equal(await publishOnce({ packageVersion, integrity, lookup, publish }), 'already-published');
  assert.equal(publish.mock.callCount(), 0);
});

test('publishes an absent version once', async t => {
  const lookup = t.mock.fn(() => null);
  const publish = t.mock.fn(() => ({ status: 0 }));
  assert.equal(await publishOnce({ packageVersion, integrity, lookup, publish }), 'published');
  assert.equal(lookup.mock.callCount(), 1);
  assert.equal(publish.mock.callCount(), 1);
});

test('accepts a publish race only if the same bytes won', async t => {
  const publish = t.mock.fn(() => ({ status: 1 }));
  const lookup = t.mock.fn(() => integrity);
  lookup.mock.mockImplementationOnce(() => null);
  assert.equal(await publishOnce({ packageVersion, integrity, lookup, publish }), 'already-published');
  assert.equal(lookup.mock.callCount(), 2);
  assert.equal(publish.mock.callCount(), 1);

  const conflict = t.mock.fn(() => 'sha512-conflict');
  conflict.mock.mockImplementationOnce(() => null);
  await assert.rejects(publishOnce({ packageVersion, integrity, lookup: conflict, publish }), /already exists with a different tarball/);
});

test('surfaces a failed publish when the version remains absent', async t => {
  const lookup = t.mock.fn(() => null);
  const publish = t.mock.fn(() => ({ status: 42 }));
  await assert.rejects(publishOnce({ packageVersion, integrity, lookup, publish }), /npm publish failed for dwave@1\.2\.3 \(exit 42\)/);
  assert.equal(lookup.mock.callCount(), 2);
  assert.equal(publish.mock.callCount(), 1);
});
