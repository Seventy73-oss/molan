#!/usr/bin/env node
// 旧数据兼容与回退检查：同一个数据目录依次由「基线版本 → 当前版本 → 基线版本」打开。
//
//   node tools/compat-check.mjs <基线 molan-server 可执行文件> [当前 molan-server 可执行文件]
//
// 1) 基线版本建数据：技能、作品、设定 / 正文 / 细纲、细纲确认、第 1 章记忆、聊天会话消息、第 2 章待审稿；
// 2) 当前版本打开同一目录：旧数据全部可读（作品、文件 hash、会话消息、旧消息的产物投影、待审队列、技能版本、
//    记忆），并能用新服务定稿基线生成的待审稿；
// 3) 回退：基线版本再次打开（库里已有新表 / 新列）：仍能读取作品、正文、会话与技能，并照常写入。
// 只连 127.0.0.1，使用全新临时目录与本地桩模型；不触碰任何已有数据。
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = resolve(fileURLToPath(new URL('.', import.meta.url)), '..');
const baselineExe = process.argv[2];
const currentExe = process.argv[3] ?? resolve(repo, 'target/debug', process.platform === 'win32' ? 'molan-server.exe' : 'molan-server');
if (!baselineExe) throw new Error('用法：node tools/compat-check.mjs <基线可执行文件> [当前可执行文件]');

// ---------- 桩模型（规则同 mock://） ----------
function reply(sys, user) {
  if (sys.includes('审读一章正文')) return '{"ok":true,"issues":[],"fix":""}';
  if (sys.includes('小说连续性复核员')) return '{"passed":true,"errors":[]}';
  if (sys.includes('小说连续性记录员')) {
    const ev = user.split('\n').map((l) => l.trim()).find((l) => [...l].length >= 8 && !l.startsWith('【') && !l.startsWith('#')) ?? '';
    return JSON.stringify({ summary: '本章为测试章节。', facts: [], threads: [], events: [{ description: '开篇', evidence: [...ev].slice(0, 14).join('') }] });
  }
  if (sys.includes('只输出 JSON') || sys.includes('只输出JSON') || sys.includes('输出人物状态变更 JSON')) return '{"updates":[],"newCharacters":[]}';
  let t = '第二章 初试\n\n山门之内，少年站在演武场边，看着同批弟子依次上前。';
  for (let i = 1; i <= 8; i++) t += `\n\n第${i}位弟子上前时，场边一阵低语。少年握紧玉佩，默念老人的叮嘱：修行一途，如逆水行舟，不进则退。他深吸一口气，等着自己的名字。`;
  return t;
}
const text = (c) => (typeof c === 'string' ? c : Array.isArray(c) ? c.map((x) => x.text ?? '').join('') : '');
const stub = createServer((req, res) => {
  let body = '';
  req.on('data', (d) => (body += d));
  req.on('end', () => {
    const j = JSON.parse(body || '{}');
    const msgs = j.messages ?? [];
    const sys = msgs.filter((m) => m.role === 'system').map((m) => text(m.content)).join('\n');
    const user = [...msgs].reverse().find((m) => m.role === 'user');
    const content = reply(sys, text(user?.content ?? ''));
    const usage = { prompt_tokens: 10, completion_tokens: 10, total_tokens: 20 };
    if (j.stream) {
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      for (const p of content.match(/[\s\S]{1,200}/g) ?? ['']) res.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: { content: p } }] })}\n\n`);
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

const root = mkdtempSync(join(tmpdir(), 'molan-compat-'));
async function withServer(exe, fn) {
  const port = 18900 + Math.floor(Math.random() * 90);
  const proc = spawn(exe, [], { env: { ...process.env, MOLAN_ROOT: root, PORT: String(port), BIND: '127.0.0.1', MOLAN_AUTH_TOKEN: '' }, stdio: 'ignore' });
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
    const ipc = async (cmd, args = {}) => {
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
    return await fn(ipc);
  } finally {
    proc.kill();
    await new Promise((r) => setTimeout(r, 800)); // 等进程释放数据库文件
  }
}

const results = [];
const check = (stage, name, ok, detail = '') => {
  results.push(ok);
  console.log(`${ok ? 'PASS' : 'FAIL'}  [${stage}] ${name}${detail ? `  — ${detail}` : ''}`);
};
const waitMemory = async (ipc, bookId, ch) => {
  for (let i = 0; i < 80; i++) {
    const ms = await ipc('memory_status', { bookId });
    if ((ms.memories ?? []).some((m) => m.ch === ch && m.status === 'valid')) return true;
    await new Promise((r) => setTimeout(r, 250));
  }
  return false;
};

const ch1 = '# 第1章 初入山门\n\n山风掠过石阶，少年背着一个旧行囊，站在了青岚宗的山门前。\n\n他抬头望着云雾深处的连绵殿宇，攥紧了手里的荐书。这是他第一次离家这么远。\n';
let fixture;

// ---------- 1) 基线版本建数据 ----------
await withServer(baselineExe, async (ipc) => {
  const dump = JSON.parse(readFileSync(resolve(repo, 'skills-export/skills-export.json'), 'utf8'));
  for (const s of dump.skills.slice(0, 12)) {
    let targets = [];
    try {
      targets = JSON.parse(s.targets_json || '[]');
    } catch {
      /* 旧导出 */
    }
    await ipc('create_skill', { name: s.name, description: s.description ?? '', promptTemplate: s.prompt_template ?? '', kind: s.kind, usageMode: s.usage_mode, origin: s.origin, builtinKey: s.builtin_key ?? '', targets, enabled: s.enabled !== 0 });
  }
  await ipc('set_settings', { entries: { channels: JSON.stringify([{ id: 'stub', label: 'stub', baseUrl: stubUrl, model: 'stub-model', models: ['stub-model'] }]), active_channel: 'stub' } });
  const b = await ipc('create_book', { title: '旧版作品', genre: '玄幻', pov: '第三人称' });
  await ipc('write_file', { bookId: b.id, group: '设定', name: '人物.md', content: '# 人物\n\n- 林照：主角。\n' });
  await ipc('write_file', { bookId: b.id, group: '正文', name: '第1章.md', content: ch1 });
  await ipc('write_file', { bookId: b.id, group: '细纲', name: '细纲_第2章.md', content: '# 第2章 细纲\n\n**本章目标**：通过第一试。\n' });
  await ipc('confirm_outline', { bookId: b.id, ch: 2 });
  await ipc('rebuild_memory', { bookId: b.id, ch: 1 });
  check('基线', '第1章记忆同步', await waitMemory(ipc, b.id, 1));
  const s = await ipc('create_session', { bookId: b.id, title: '旧会话' });
  await ipc('chat_stream', { onEvent: '__CHANNEL__:1', bookId: b.id, sessionId: s.id, message: '介绍一下主角', requestId: 'old-chat' });
  await ipc('draft_chapter', { onEvent: '__CHANNEL__:1', bookId: b.id, ch: 2, requestId: 'old-draft' });
  const pending = await ipc('list_pending_chapters', { bookId: b.id });
  check('基线', '第2章进入待审', pending.some((p) => p.ch === 2));
  const skills = await ipc('list_skills');
  fixture = { bookId: b.id, sessionId: s.id, skillCount: skills.length, pendingHash: pending.find((p) => p.ch === 2)?.contentHash };
});

// ---------- 2) 当前版本打开同一目录 ----------
await withServer(currentExe, async (ipc) => {
  const stage = '当前';
  const books = await ipc('list_books');
  check(stage, '旧作品可读', books.some((x) => x.id === fixture.bookId));
  const doc = await ipc('doc_read', { bookId: fixture.bookId, group: '正文', name: '第1章.md' });
  check(stage, '旧正文可读且带 hash（doc_read）', doc.exists && doc.content === ch1 && doc.hash?.length === 64);
  const msgs = await ipc('list_messages', { bookId: fixture.bookId, sessionId: fixture.sessionId });
  check(stage, '旧会话消息可读', Array.isArray(msgs) && msgs.length >= 2, `${msgs?.length} 条`);
  const arts = await ipc('artifact_list', { bookId: fixture.bookId, sessionId: fixture.sessionId });
  check(stage, '旧会话产物投影不报错', Array.isArray(arts), `${arts.length} 张卡`);
  const skills = await ipc('list_skills');
  check(stage, '旧技能全部可读并带版本', skills.length === fixture.skillCount && skills.every((x) => x.rev >= 1), `${skills.length} 条`);
  const pending = await ipc('list_pending_chapters', { bookId: fixture.bookId });
  const p2 = pending.find((p) => p.ch === 2);
  check(stage, '旧待审稿在队列中且 hash 一致', !!p2 && p2.contentHash === fixture.pendingHash);
  check(stage, '旧待审稿的审稿状态如实为「尚未审稿」（旧版未入账）', p2?.review?.state === 'none', JSON.stringify(p2?.review));
  const ms = await ipc('memory_status', { bookId: fixture.bookId });
  check(stage, '旧记忆可读', (ms.memories ?? []).some((m) => m.ch === 1 && m.status === 'valid'));
  const preview = await ipc('task_preview', { bookId: fixture.bookId, task: 'body', target: { ch: 3 } });
  check(stage, '对旧作品做计划预览', typeof preview.plan?.planHash === 'string');
  const ok = await ipc('approve_chapter', { bookId: fixture.bookId, name: p2.name, expectedHash: p2.contentHash });
  check(stage, '用新服务定稿旧待审稿（按 hash 绑定）', ok.ok === true && typeof ok.writeId === 'string');
  const formal = await ipc('doc_read', { bookId: fixture.bookId, group: '正文', name: '第2章.md' });
  check(stage, '定稿后正式稿存在', formal.exists && formal.hash === p2.contentHash);
  const hist = await ipc('doc_history', { bookId: fixture.bookId, group: '正文', name: '第2章.md' });
  check(stage, '新写入进入统一账本', hist.some((h) => h.source?.service === 'chapter_commit.approve'));
  fixture.ch2Hash = formal.hash;
});

// ---------- 3) 回退：基线版本再次打开 ----------
await withServer(baselineExe, async (ipc) => {
  const stage = '回退';
  const books = await ipc('list_books');
  check(stage, '旧版本仍能启动并列出作品', books.some((x) => x.id === fixture.bookId));
  const c1 = await ipc('read_file', { bookId: fixture.bookId, group: '正文', name: '第1章.md' });
  check(stage, '旧版本读取正文', c1 === ch1);
  const c2 = await ipc('read_file', { bookId: fixture.bookId, group: '正文', name: '第2章.md' });
  check(stage, '旧版本读取新版本定稿的正文', typeof c2 === 'string' && c2.length > 100);
  const msgs = await ipc('list_messages', { bookId: fixture.bookId, sessionId: fixture.sessionId });
  check(stage, '旧版本读取会话', Array.isArray(msgs) && msgs.length >= 2);
  const skills = await ipc('list_skills');
  check(stage, '旧版本读取技能', skills.length === fixture.skillCount);
  await ipc('write_file', { bookId: fixture.bookId, group: '设定', name: '回退后新增.md', content: '回退后写入' });
  check(stage, '旧版本照常写入', (await ipc('read_file', { bookId: fixture.bookId, group: '设定', name: '回退后新增.md' })) === '回退后写入');
});

stub.close();
const failed = results.filter((x) => !x).length;
console.log(`\n${results.length - failed}/${results.length} 通过 · 数据目录 ${root}`);
process.exit(failed ? 1 : 0);
