#!/usr/bin/env node
// 重构前后对比（同一固定数据 + 同一桩模型）：模型调用次数、提供的工具数、系统提示与总提示字数、服务端耗时。
//
//   node tools/compare-baseline.mjs <基线 molan-server 可执行文件> [当前 molan-server 可执行文件]
//
// 基线可执行文件需另行构建（例如 `git archive <基线提交> | tar -x -C 某目录` 后在该目录 cargo build）。
// 只连 127.0.0.1；每个版本使用全新临时数据目录；桩模型是本地 OpenAI 兼容服务，按固定规则回复，
// 记录每一次请求。结果是「协议/流程层」对比，不代表真实模型的质量、速度或费用。
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = resolve(fileURLToPath(new URL('.', import.meta.url)), '..');
const baselineExe = process.argv[2];
const currentExe = process.argv[3] ?? resolve(repo, 'target/debug', process.platform === 'win32' ? 'molan-server.exe' : 'molan-server');
if (!baselineExe) throw new Error('用法：node tools/compare-baseline.mjs <基线可执行文件> [当前可执行文件]');

// ---------- 桩模型（规则同 molan-llm 的 mock://，保证两个版本拿到相同的回复） ----------
function reply(sys, user) {
  if (sys.includes('审读一章正文')) return '{"ok":true,"issues":[],"fix":""}';
  if (sys.includes('小说连续性复核员')) return '{"passed":true,"errors":[]}';
  if (sys.includes('小说连续性记录员')) {
    const ev = (user.split('\n').map((l) => l.trim()).find((l) => [...l].length >= 8 && !l.startsWith('【') && !l.startsWith('#')) ?? '');
    return JSON.stringify({ summary: '本章为测试章节，情节以原文为准。', facts: [], threads: [], events: [{ description: '本章开篇情节', evidence: [...ev].slice(0, 14).join('') }] });
  }
  if (sys.includes('输出人物状态变更 JSON') || sys.includes('只输出 JSON') || sys.includes('只输出JSON')) return '{"updates":[],"newCharacters":[]}';
  if (sys.includes('压缩成「前情摘要」')) return '（覆盖至第N章）主角获得入门名额。';
  if (sys.includes('走向级细纲') || user.includes('细纲')) return '# 第3章 细纲\n\n**本章目标**：主角通过第二试。\n**冲突与对手**：赵衡再度刁难。\n**章末钩子**：禁地异动。';
  let t = '第一章 初入山门\n\n山风掠过石阶，少年背着一个旧行囊，站在了青岚宗的山门前。';
  for (let i = 1; i <= 8; i++) t += `\n\n这段路他走得并不轻松。第${i}次停下歇脚的时候，他想起临行前村里的老人说过的话：修行一途，如逆水行舟，不进则退。少年咬了咬牙，继续向上走去。石阶尽头，一名灰袍弟子拦住了他，问他可有名录在册。`;
  return t;
}

let tag = 'setup';
const calls = [];
const text = (c) => (typeof c === 'string' ? c : Array.isArray(c) ? c.map((x) => x.text ?? '').join('') : '');
const stub = createServer((req, res) => {
  let body = '';
  req.on('data', (d) => (body += d));
  req.on('end', () => {
    const j = JSON.parse(body || '{}');
    const msgs = j.messages ?? [];
    const sys = msgs.filter((m) => m.role === 'system').map((m) => text(m.content)).join('\n');
    const user = [...msgs].reverse().find((m) => m.role === 'user');
    const promptChars = msgs.reduce((n, m) => n + [...text(m.content)].length, 0);
    calls.push({ tag, stream: !!j.stream, tools: (j.tools ?? []).length, messages: msgs.length, sysChars: [...sys].length, promptChars });
    const content = reply(sys, text(user?.content ?? ''));
    const usage = { prompt_tokens: promptChars, completion_tokens: [...content].length, total_tokens: promptChars + [...content].length };
    if (j.stream) {
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      const parts = content.match(/[\s\S]{1,200}/g) ?? [''];
      for (const p of parts) res.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: { content: p } }] })}\n\n`);
      res.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: {}, finish_reason: 'stop' }], usage })}\n\n`);
      res.end('data: [DONE]\n\n');
    } else {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ choices: [{ index: 0, message: { role: 'assistant', content }, finish_reason: 'stop' }], usage }));
    }
  });
});
await new Promise((r) => stub.listen(0, '127.0.0.1', r));
const stubUrl = `http://127.0.0.1:${stub.address().port}/v1`;

// ---------- 被测服务 ----------
async function withServer(exe, fn) {
  const port = 18000 + Math.floor(Math.random() * 900);
  const root = mkdtempSync(join(tmpdir(), 'molan-cmp-'));
  const proc = spawn(exe, [], { env: { ...process.env, MOLAN_ROOT: root, PORT: String(port), BIND: '127.0.0.1', MOLAN_AUTH_TOKEN: '' }, stdio: ['ignore', 'ignore', 'ignore'] });
  const base = `http://127.0.0.1:${port}`;
  try {
    for (let i = 0; i < 80; i++) {
      try {
        if ((await fetch(`${base}/health`)).ok) break;
      } catch {
        /* 启动中 */
      }
      await new Promise((r) => setTimeout(r, 250));
    }
    return await fn(base);
  } finally {
    proc.kill();
  }
}

function ipcOf(base) {
  return async (cmd, args = {}) => {
    const r = await fetch(`${base}/ipc/${cmd}`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ args }) });
    let out;
    let err;
    for (const line of (await r.text()).split('\n')) {
      if (!line.trim()) continue;
      const j = JSON.parse(line);
      if (j.err) err = j.err.message;
      if ('r' in j) out = j.r;
    }
    if (err) throw new Error(`${cmd}: ${err}`);
    return out;
  };
}

async function seed(ipc) {
  const dump = JSON.parse(readFileSync(resolve(repo, 'skills-export/skills-export.json'), 'utf8'));
  for (const s of dump.skills) {
    let targets = [];
    try {
      targets = JSON.parse(s.targets_json || '[]');
    } catch {
      /* 旧导出 */
    }
    await ipc('create_skill', { name: s.name, description: s.description ?? '', promptTemplate: s.prompt_template ?? '', kind: s.kind, usageMode: s.usage_mode, origin: s.origin, builtinKey: s.builtin_key ?? '', targets, enabled: s.enabled !== 0 });
  }
  await ipc('set_settings', { entries: { channels: JSON.stringify([{ id: 'stub', label: 'stub', baseUrl: stubUrl, model: 'stub-model', models: ['stub-model'] }]), active_channel: 'stub' } });
  const b = await ipc('create_book', { title: '青岚纪（对比）', genre: '玄幻', pov: '第三人称' });
  const w = (group, name, content) => ipc('write_file', { bookId: b.id, group, name, content });
  await w('设定', '世界观.md', '# 世界观\n\n青岚宗坐落于云雾群山之间，修行分为炼气、筑基、金丹三境。\n');
  await w('设定', '人物.md', '# 人物\n\n- 林照：主角，山村少年，持半块玉佩，性格倔强。\n- 白发长老：外门执事。\n');
  await w('正文', '第1章.md', '# 第1章 初入山门\n\n山风掠过石阶，少年背着一个旧行囊，站在了青岚宗的山门前。\n\n他抬头望着云雾深处的连绵殿宇，攥紧了手里的荐书。这是他第一次离家这么远。\n');
  await w('细纲', '细纲_第2章.md', '# 第2章 细纲\n\n**本章目标**：林照通过三试中的第一试。\n**冲突与对手**：同批弟子赵衡刁难。\n**章末钩子**：长老发现玉佩纹路。\n');
  await ipc('confirm_outline', { bookId: b.id, ch: 2 });
  await ipc('rebuild_memory', { bookId: b.id, ch: 1 });
  for (let i = 0; i < 60; i++) {
    const ms = await ipc('memory_status', { bookId: b.id });
    if ((ms.memories ?? []).some((m) => m.ch === 1 && m.status === 'valid')) break;
    await new Promise((r) => setTimeout(r, 250));
  }
  const s = await ipc('create_session', { bookId: b.id, title: '对比' });
  return { bookId: b.id, sessionId: s.id };
}

const SCENARIOS = [
  { id: 'S1 助手聊天', run: (ipc, f, v) => ipc('agent_turn', { onEvent: '__CHANNEL__:1', bookId: f.bookId, sessionId: f.sessionId, message: '主角林照现在处于什么处境？', requestId: `s1-${v}`, ...(v === 'now' ? { task: 'chat' } : {}) }) },
  { id: 'S2 助手起草第3章细纲', run: (ipc, f, v) => ipc('agent_turn', { onEvent: '__CHANNEL__:1', bookId: f.bookId, sessionId: f.sessionId, message: '起草第3章细纲', requestId: `s2-${v}`, ...(v === 'now' ? { task: 'outline', target: { ch: 3 } } : {}) }) },
  { id: 'S3 单章起草（第2章）', run: (ipc, f, v) => ipc('draft_chapter', { onEvent: '__CHANNEL__:1', bookId: f.bookId, ch: 2, requestId: `s3-${v}` }) },
  { id: 'S4 旧聊天入口 chat_stream', run: (ipc, f, v) => ipc('chat_stream', { onEvent: '__CHANNEL__:1', bookId: f.bookId, sessionId: f.sessionId, message: '介绍一下主角林照', requestId: `s4-${v}` }) },
];

const rows = [];
for (const [version, exe] of [['baseline', baselineExe], ['now', currentExe]]) {
  await withServer(exe, async (base) => {
    const ipc = ipcOf(base);
    tag = `${version}:setup`;
    const f = await seed(ipc);
    for (const sc of SCENARIOS) {
      tag = `${version}:${sc.id}`;
      const t0 = performance.now();
      let status = 'ok';
      try {
        const r = await sc.run(ipc, f, version);
        status = r?.status ?? (r?.ok === false ? 'not-ok' : 'ok');
      } catch (e) {
        status = `error: ${String(e.message).slice(0, 60)}`;
      }
      const ms = Math.round(performance.now() - t0);
      const cs = calls.filter((c) => c.tag === tag);
      rows.push({
        scenario: sc.id,
        version,
        status,
        modelCalls: cs.length,
        toolsOffered: Math.max(0, ...cs.map((c) => c.tools)),
        maxSystemChars: Math.max(0, ...cs.map((c) => c.sysChars)),
        totalPromptChars: cs.reduce((n, c) => n + c.promptChars, 0),
        serverMs: ms,
      });
    }
  });
}
stub.close();

console.log('| 场景 | 版本 | 结果 | 模型调用 | 提供工具数 | 最大系统提示（字） | 提示总字数 | 服务端耗时（ms，桩模型） |');
console.log('|---|---|---|---|---|---|---|---|');
for (const r of rows) console.log(`| ${r.scenario} | ${r.version} | ${r.status} | ${r.modelCalls} | ${r.toolsOffered} | ${r.maxSystemChars} | ${r.totalPromptChars} | ${r.serverMs} |`);
