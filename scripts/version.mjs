#!/usr/bin/env node
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const workspacePackages = ['fdm-core', 'fdm-engine', 'fdm-server', 'fdm-platform', 'fdm-desktop'];

function read(path) {
  return readFileSync(join(root, path), 'utf8');
}

function write(path, text) {
  writeFileSync(join(root, path), text, 'utf8');
}

function readJson(path) {
  return JSON.parse(read(path));
}

function writeJson(path, value) {
  write(path, JSON.stringify(value, null, 2) + '\n');
}

function normalizeVersion(input) {
  const value = String(input || '').trim().replace(/^v/i, '');
  if (!/^\d+\.\d+\.\d+$/.test(value)) {
    throw new Error(`invalid semantic version: ${input}`);
  }
  return value;
}

function bumpPatch(version) {
  const [major, minor, patch] = normalizeVersion(version).split('.').map(Number);
  return `${major}.${minor}.${patch + 1}`;
}

function setVersion(version) {
  const next = normalizeVersion(version);

  const pkg = readJson('package.json');
  pkg.version = next;
  writeJson('package.json', pkg);

  const lock = readJson('package-lock.json');
  lock.version = next;
  if (lock.packages && lock.packages['']) {
    lock.packages[''].version = next;
  }
  writeJson('package-lock.json', lock);

  let cargoToml = read('Cargo.toml');
  cargoToml = cargoToml.replace(
    /(\[workspace\.package\][\s\S]*?\nversion\s*=\s*")[^"]+(")/,
    `$1${next}$2`,
  );
  write('Cargo.toml', cargoToml);

  let cargoLock = read('Cargo.lock');
  for (const name of workspacePackages) {
    const pattern = new RegExp(
      `(\\[\\[package\\]\\]\\s*\\nname = "${name}"\\s*\\nversion = ")[^"]+(")`,
    );
    cargoLock = cargoLock.replace(pattern, `$1${next}$2`);
  }
  write('Cargo.lock', cargoLock);

  const tauri = readJson('src-tauri/tauri.conf.json');
  tauri.version = next;
  writeJson('src-tauri/tauri.conf.json', tauri);

  return next;
}

function versionSnapshot() {
  const pkg = readJson('package.json');
  const lock = readJson('package-lock.json');
  const tauri = readJson('src-tauri/tauri.conf.json');
  const cargoToml = read('Cargo.toml');
  const cargoLock = read('Cargo.lock');
  const workspaceVersion = cargoToml.match(/\[workspace\.package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/)?.[1];
  const lockVersions = {};
  for (const name of workspacePackages) {
    const pattern = new RegExp(
      `\\[\\[package\\]\\]\\s*\\nname = "${name}"\\s*\\nversion = "([^"]+)"`,
    );
    lockVersions[name] = cargoLock.match(pattern)?.[1];
  }
  return {
    packageJson: pkg.version,
    packageLock: lock.version,
    packageLockRoot: lock.packages?.['']?.version,
    cargoWorkspace: workspaceVersion,
    cargoLock: lockVersions,
    tauri: tauri.version,
  };
}

function assertVersion(expected) {
  const want = normalizeVersion(expected);
  const snapshot = versionSnapshot();
  const failures = [];
  for (const [field, value] of Object.entries(snapshot)) {
    if (field === 'cargoLock') {
      for (const [name, actual] of Object.entries(value)) {
        if (actual !== want) failures.push(`${name} Cargo.lock=${actual}`);
      }
    } else if (value !== want) {
      failures.push(`${field}=${value}`);
    }
  }
  if (failures.length) {
    throw new Error(`version mismatch, expected ${want}: ${failures.join(', ')}`);
  }
  return want;
}

function option(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

const command = process.argv[2];

try {
  if (command === 'next') {
    const base = normalizeVersion(option('--base') || readJson('package.json').version);
    console.log(bumpPatch(base));
  } else if (command === 'bump') {
    const base = normalizeVersion(option('--base') || readJson('package.json').version);
    console.log(setVersion(bumpPatch(base)));
  } else if (command === 'set') {
    const value = process.argv[3];
    if (!value) throw new Error('usage: version.mjs set <version>');
    console.log(setVersion(value));
  } else if (command === 'check') {
    const value = process.argv[3];
    if (!value) throw new Error('usage: version.mjs check <version>');
    console.log(assertVersion(value));
  } else if (command === 'show') {
    console.log(JSON.stringify(versionSnapshot(), null, 2));
  } else {
    throw new Error('usage: version.mjs <next|bump|set|check|show> [version] [--base version]');
  }
} catch (error) {
  console.error(`version: ${error.message}`);
  process.exit(1);
}
