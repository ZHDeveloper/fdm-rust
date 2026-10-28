#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import fs from 'node:fs';
import net from 'node:net';

const scripts = dirname(fileURLToPath(import.meta.url));
const root = dirname(scripts);

// 自动检测并补齐 Rust/Cargo 路径
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

// 检查 25432 端口是否已有 FDM 实例监听
function checkPortInUse(port) {
  return new Promise((resolve) => {
    const socket = new net.Socket();
    socket.setTimeout(400);
    socket.on('connect', () => {
      socket.destroy();
      resolve(true);
    });
    socket.on('timeout', () => {
      socket.destroy();
      resolve(false);
    });
    socket.on('error', () => {
      resolve(false);
    });
    socket.connect(port, '127.0.0.1');
  });
}

function run(command, args) {
  const result = spawnSync(command, args, { stdio: 'inherit', cwd: root, env: process.env });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}

async function main() {
  ensureCargoPath();

  const isRunning = await checkPortInUse(25432);
  if (isRunning) {
    console.warn('\n⚠️ [警告] 检测到端口 25432 已被监听，可能已有正在运行的 FDM 实例。');
    console.warn('   请先完全退出已运行的 FDM，以避免单例锁和端口占用冲突。\n');
  }

  const passthroughArgs = process.argv.slice(2);

  if (process.platform === 'win32') {
    run('powershell.exe', [
      '-NoProfile',
      '-ExecutionPolicy',
      'Bypass',
      '-File',
      join(scripts, 'dev.ps1'),
      ...passthroughArgs
    ]);
  } else {
    // 优先使用本地 node_modules 里的 tauri，避免 npx 因网络问题尝试去 npm 官方源拉取
    const localTauriCli = join(root, 'node_modules', '@tauri-apps', 'cli', 'tauri.js');
    if (fs.existsSync(localTauriCli)) {
      run('node', [localTauriCli, 'dev', ...passthroughArgs]);
    } else {
      run('npx', ['@tauri-apps/cli', 'dev', ...passthroughArgs]);
    }
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
