#!/usr/bin/env node
// 一键本地浏览器验证：全新临时数据目录 → 启动已构建的 molan-server（仅 127.0.0.1）→ 种子 → run.mjs → 停服。
//   cargo build -p molan-server && (cd frontend && npm run build)
//   node frontend/e2e/local.mjs [截图目录]
// 不触碰任何已有数据目录；结束后临时目录保留以便排查（路径会打印）。
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, openSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = fileURLToPath(new URL('.', import.meta.url));
const repo = resolve(here, '../..');
const exe = resolve(repo, 'target/debug', process.platform === 'win32' ? 'molan-server.exe' : 'molan-server');
const dist = resolve(repo, 'frontend/dist');
if (!existsSync(exe)) throw new Error(`未找到 ${exe}，请先 cargo build -p molan-server`);
if (!existsSync(join(dist, 'index.html'))) throw new Error('未找到 frontend/dist，请先 npm run build');

const port = 17000 + Math.floor(Math.random() * 900);
const root = mkdtempSync(join(tmpdir(), 'molan-e2e-'));
const base = `http://127.0.0.1:${port}`;
console.log(`[e2e] 数据目录 ${root} · 服务 ${base}`);
const server = spawn(exe, [], {
  env: { ...process.env, MOLAN_ROOT: root, PORT: String(port), BIND: '127.0.0.1', MOLAN_WEB_DIR: dist, MOLAN_AUTH_TOKEN: '' },
  // 服务端日志（tracing 写 stdout）保留在临时数据目录，便于事后排查耗时与错误
  stdio: ['ignore', openSync(join(root, 'server.log'), 'w'), 'inherit'],
});

let code = 1;
try {
  for (let i = 0; i < 60; i++) {
    try {
      const r = await fetch(`${base}/health`);
      if (r.ok) break;
    } catch {
      /* 等待启动 */
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  const seed = spawnSync(process.execPath, [resolve(repo, 'tools/dev-seed.mjs'), base], { stdio: 'inherit' });
  if (seed.status !== 0) throw new Error('种子失败');
  const run = spawnSync(process.execPath, [resolve(here, 'run.mjs'), base, process.argv[2] ?? resolve(repo, 'docs/refactor/screenshots')], { stdio: 'inherit', cwd: resolve(repo, 'frontend') });
  code = run.status ?? 1;
} finally {
  server.kill();
  const timing = readFileSync(join(root, 'server.log'), 'utf8').split('\n').filter((l) => l.includes('审批通过'));
  for (const l of timing) console.log(`[e2e] ${l.replace(/\x1b\[[0-9;]*m/g, '').trim()}`);
}
process.exit(code);
