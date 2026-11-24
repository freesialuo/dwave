import { execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { appendFileSync, createReadStream, readFileSync } from 'node:fs';
import { basename } from 'node:path';

const root = new URL('../', import.meta.url);
const manifest = JSON.parse(readFileSync(new URL('npm/package.json', root), 'utf8'));
const targets = JSON.parse(readFileSync(new URL('npm/targets.json', root), 'utf8'));
const registry = 'https://registry.npmjs.org';

function parseRelease(tag) {
  const match = /^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$/.exec(tag);
  if (!match || match[4]?.split('.').some(identifier => /^0\d+$/.test(identifier))) {
    throw new Error(`Expected a v-prefixed SemVer release tag without build metadata, got ${tag}`);
  }

  const version = tag.slice(1);
  if (version !== manifest.version) {
    throw new Error(`Tag ${tag} does not match npm package version ${manifest.version}`);
  }
  return { version, dist_tag: match[4] ? 'next' : 'latest' };
}

function validateCargoVersion(version) {
  const metadata = JSON.parse(execFileSync('cargo', [
    'metadata', '--locked', '--offline', '--no-deps', '--format-version', '1',
  ], { cwd: root, encoding: 'utf8' }));
  for (const pkg of metadata.packages) {
    if (pkg.version !== version) {
      throw new Error(`Cargo package ${pkg.name} is ${pkg.version}, expected ${version}`);
    }
  }
  if (!metadata.packages.some(pkg => pkg.targets.some(target => target.name === 'dwave' && target.kind.includes('bin')))) {
    throw new Error('Cargo metadata is missing the dwave binary target');
  }
}

function output(values) {
  console.log(JSON.stringify(values));
  if (process.env.GITHUB_OUTPUT) {
    for (const [key, value] of Object.entries(values)) {
      appendFileSync(process.env.GITHUB_OUTPUT, `${key}=${value}\n`);
    }
  }
}

async function verifyPackage(archive, version) {
  if (basename(archive) !== `${manifest.name}-${version}.tgz`) {
    throw new Error(`Unexpected release archive filename: ${archive}`);
  }

  const files = execFileSync('tar', ['-tf', archive], { encoding: 'utf8' }).trim().split('\n');
  const expected = [
    'package/package.json', 'package/README.md', 'package/LICENSE-MIT', 'package/LICENSE-APACHE',
    'package/bin/dwave.cjs', 'package/targets.json',
    ...Object.entries(targets).map(([target, executable]) => `package/native/${target}/${executable}`),
  ];
  if (JSON.stringify(files.sort()) !== JSON.stringify(expected.sort())) {
    throw new Error(`Unexpected files in release package: ${files.join(', ')}`);
  }

  const packed = JSON.parse(execFileSync('tar', ['-xOf', archive, 'package/package.json'], { encoding: 'utf8' }));
  if (packed.name !== manifest.name || packed.version !== version || packed.bin?.dwave !== 'bin/dwave.cjs') {
    throw new Error('Packed name, version, or executable do not match the release');
  }
  const repository = process.env.GITHUB_REPOSITORY;
  if (repository && packed.repository?.url !== `git+https://github.com/${repository}.git`) {
    throw new Error(`Packed repository URL must match ${repository} for npm provenance`);
  }

  const hash = createHash('sha512');
  for await (const chunk of createReadStream(archive)) hash.update(chunk);
  return `sha512-${hash.digest('base64')}`;
}

async function publishedIntegrity(version) {
  const response = await fetch(`${registry}/${manifest.name}/${version}`, {
    headers: { 'Cache-Control': 'no-cache' }, signal: AbortSignal.timeout(15_000),
  });
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(`npm registry returned HTTP ${response.status} loading ${manifest.name}@${version}`);

  const { dist } = await response.json();
  if (!dist?.integrity) throw new Error(`Missing npm integrity for published ${manifest.name}@${version}`);
  return dist.integrity;
}

async function publish(archive, release, integrity) {
  const version = `${manifest.name}@${release.version}`;
  const prior = await publishedIntegrity(release.version);
  if (prior) {
    if (prior !== integrity) throw new Error(`${version} already exists with a different tarball`);
    console.log(`${version} already published with the same integrity.`);
    return;
  }

  const result = spawnSync('npm', ['publish', archive, '--registry', registry, '--access', 'public', '--tag', release.dist_tag], {
    stdio: 'inherit',
  });
  if (result.error) throw result.error;
  if (result.status !== 0 && await publishedIntegrity(release.version) !== integrity) {
    throw new Error(`npm publish failed for ${version} (exit ${result.status ?? result.signal})`);
  }
}

const [operation, tag, archive, expectedIntegrity] = process.argv.slice(2);
const release = parseRelease(tag);

if (operation === 'validate') {
  validateCargoVersion(release.version);
  output(release);
} else if (operation === 'verify' && archive) {
  const integrity = await verifyPackage(archive, release.version);
  output({ integrity });
} else if (operation === 'publish' && archive && expectedIntegrity) {
  if (!process.env.GITHUB_REPOSITORY) throw new Error('Publishing requires the GitHub repository context');
  const integrity = await verifyPackage(archive, release.version);
  if (integrity !== expectedIntegrity) throw new Error('Release tarball differs from the verified tarball');
  await publish(archive, release, integrity);
} else {
  throw new Error('Usage: node scripts/npm-release.mjs validate TAG | verify TAG TARBALL | publish TAG TARBALL INTEGRITY');
}
