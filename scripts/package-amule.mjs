#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const scripts = dirname(fileURLToPath(import.meta.url));

function run(command, args) {
  const result = spawnSync(command, args, { stdio: 'inherit', cwd: dirname(scripts) });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}

if (process.platform === 'win32') {
  run('powershell.exe', [
    '-NoProfile',
    '-ExecutionPolicy',
    'Bypass',
    '-File',
    join(scripts, 'package-amule.ps1'),
  ]);
} else if (process.platform === 'darwin') {
  run('bash', [join(scripts, 'package-amule-macos.sh')]);
} else {
  console.error(`package-amule: unsupported platform ${process.platform}`);
  process.exit(1);
}
