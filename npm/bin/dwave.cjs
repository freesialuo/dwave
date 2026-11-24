#!/usr/bin/env node

const { spawnSync } = require('node:child_process');
const { constants } = require('node:os');
const { join } = require('node:path');
const targets = require('../targets.json');

const target = `${process.platform}-${process.arch}`;
const executable = targets[target];

if (!executable) {
  console.error(`dwave has no bundled binary for ${target}. Supported targets: ${Object.keys(targets).join(', ')}.`);
  process.exitCode = 1;
} else {
  const binary = join(__dirname, '..', 'native', target, executable);
  const result = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' });

  if (result.error) {
    console.error(`dwave could not start its ${target} binary: ${result.error.message}`);
    process.exitCode = 1;
  } else if (result.signal) {
    process.exitCode = 128 + (constants.signals[result.signal] ?? 0);
  } else {
    process.exitCode = result.status ?? 1;
  }
}
