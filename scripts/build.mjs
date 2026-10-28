#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import fs from 'node:fs';

const scripts = dirname(fileURLToPath(import.meta.url));
const root = dirname(scripts);

function ensureCargoPath() {
  const delim = process.platform === 'win32' ? ';' : ':';
  const paths = (process.env.PATH || '').split(delim);
  const home = process.env.HOME || process.env.USERPROFILE || '';
  const cargoCandidates = [
    join(home, '.cargo', 'bin')
  ];
  if (process.platform === 'darwin' || process.platform === 'linux') {
    const toolchainsDir = join(home, '.rustup', 'toolchains');
    if (fs.existsSync(toolchainsDir)) {
      try {
        const dirs = fs.readdirSync(toolchainsDir);
        for (const d of dirs) {
          cargoCandidates.push(join(toolchainsDir, d, 'bin'));
        }
      } catch (e) {}
    }
  }
  for (const c of cargoCandidates) {
    const binName = process.platform === 'win32' ? 'cargo.exe' : 'cargo';
    if (fs.existsSync(join(c, binName)) && !paths.includes(c)) {
      process.env.PATH = `${c}${delim}${process.env.PATH}`;
      break;
    }
  }
}

function run(command, args) {
  const result = spawnSync(command, args, { stdio: 'inherit', cwd: root, env: process.env });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}

function main() {
  ensureCargoPath();

  const passthroughArgs = process.argv.slice(2);

  if (process.platform === 'win32') {
    run('powershell.exe', [
      '-NoProfile',
      '-ExecutionPolicy',
      'Bypass',
      '-File',
      join(scripts, 'build.ps1'),
      ...passthroughArgs
    ]);
  } else {
    const localTauriCli = join(root, 'node_modules', '@tauri-apps', 'cli', 'tauri.js');
    if (fs.existsSync(localTauriCli)) {
      run('node', [localTauriCli, 'build', ...passthroughArgs]);
    } else {
      run('npx', ['@tauri-apps/cli', 'build', ...passthroughArgs]);
    }
  }
}

main();
