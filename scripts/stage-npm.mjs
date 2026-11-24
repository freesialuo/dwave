import { chmodSync, copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const [stagePath, source, mode] = process.argv.slice(2);
if (!stagePath || !source || (mode && mode !== '--host-only') || process.argv.length > 5) {
  throw new Error('Usage: node scripts/stage-npm.mjs STAGE ARTIFACTS_DIR | STAGE BINARY --host-only');
}

const root = new URL('../', import.meta.url);
const stage = resolve(stagePath);
const targets = JSON.parse(readFileSync(new URL('npm/targets.json', root), 'utf8'));
const manifest = JSON.parse(readFileSync(new URL('npm/package.json', root), 'utf8'));
const repository = process.env.GITHUB_REPOSITORY;

if (repository) {
  if (!/^[\w.-]+\/[\w.-]+$/.test(repository)) {
    throw new Error(`Invalid GITHUB_REPOSITORY: ${repository}`);
  }
  manifest.repository = { type: 'git', url: `git+https://github.com/${repository}.git` };
}

const selected = mode === '--host-only' ? [`${process.platform}-${process.arch}`] : Object.keys(targets);

mkdirSync(stage);
mkdirSync(join(stage, 'bin'));
writeFileSync(join(stage, 'package.json'), `${JSON.stringify(manifest, null, 2)}\n`);
for (const file of ['npm/bin/dwave.cjs', 'npm/targets.json', 'README.md', 'LICENSE-MIT', 'LICENSE-APACHE']) {
  copyFileSync(new URL(file, root), join(stage, file.replace(/^npm\//, '')));
}
chmodSync(join(stage, 'bin', 'dwave.cjs'), 0o755);

for (const target of selected) {
  const executable = targets[target];
  if (!executable) throw new Error(`Unknown host target: ${target}`);

  const directory = join(stage, 'native', target);
  const destination = join(directory, executable);
  const binary = mode === '--host-only' ? source : join(source, `binary-${target}`, executable);
  mkdirSync(directory, { recursive: true });
  copyFileSync(binary, destination);
  chmodSync(destination, 0o755);
}
