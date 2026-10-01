#!/usr/bin/env node
// 本地隔离开发种子：向「已启动的本地」molan-server 写入技能库与确定性 mock 渠道，并建一本演示作品。
// 只用于 127.0.0.1 上的隔离数据目录；绝不要对生产实例运行。
//   node tools/dev-seed.mjs http://127.0.0.1:17482
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

const base = process.argv[2] ?? 'http://127.0.0.1:17482';
if (!/^http:\/\/(127\.0\.0\.1|localhost)(:\d+)?$/.test(base)) {
  console.error('拒绝：dev-seed 只允许本机地址');
  process.exit(2);
}
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');

async function ipc(cmd, args = {}) {
  const r = await fetch(`${base}/ipc/${cmd}`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ args }) });
  const text = await r.text();
  let out;
  for (const line of text.split('\n')) {
    if (!line.trim()) continue;
    const j = JSON.parse(line);
    if (j.err) throw new Error(`${cmd}: ${j.err.message}`);
    if ('r' in j) out = j.r;
  }
  return out;
}

const existing = await ipc('list_skills');
if (existing.length === 0) {
  const dump = JSON.parse(readFileSync(resolve(root, 'skills-export/skills-export.json'), 'utf8'));
  for (const s of dump.skills) {
    let targets = [];
    try {
      targets = JSON.parse(s.targets_json || '[]');
    } catch {}
    await ipc('create_skill', {
      name: s.name,
      description: s.description ?? '',
      promptTemplate: s.prompt_template ?? '',
      kind: s.kind,
      usageMode: s.usage_mode,
      origin: s.origin,
      builtinKey: s.builtin_key ?? '',
      targets,
      enabled: s.enabled !== 0,
    });
  }
  console.log(`技能：导入 ${dump.skills.length} 条`);
} else {
  console.log(`技能：已有 ${existing.length} 条，跳过`);
}

await ipc('set_settings', {
  entries: {
    channels: JSON.stringify([{ id: 'mock', label: 'Mock（确定性测试渠道）', baseUrl: 'mock://', model: 'mock-model', models: ['mock-model'] }]),
    active_channel: 'mock',
  },
});
console.log('渠道：mock:// 已设为默认（协议/流程验证用，不代表真实模型质量）');

const books = await ipc('list_books');
if (!books.some((b) => b.title === '青岚纪（演示）')) {
  const b = await ipc('create_book', { title: '青岚纪（演示）', genre: '玄幻', pov: '第三人称' });
  const w = (group, name, content) => ipc('doc_write', { bookId: b.id, group, name, op: 'create', content, idempotencyKey: `seed-${group}-${name}` });
  await w('设定', '世界观.md', '# 世界观\n\n青岚宗坐落于云雾群山之间，修行分为炼气、筑基、金丹三境。\n\n宗门规矩：外门弟子三年内未入筑基者，须下山历练。\n');
  await w('设定', '人物.md', '# 人物\n\n- 林照：主角，山村少年，持半块玉佩，性格倔强。\n- 白发长老：外门执事，表面严苛，暗中关照林照。\n');
  await w('正文', '第1章.md', '# 第1章 初入山门\n\n山风掠过石阶，少年背着一个旧行囊，站在了青岚宗的山门前。\n\n他抬头望着云雾深处的连绵殿宇，攥紧了手里的荐书。😀 这是他第一次离家这么远。\n');
  await w('细纲', '细纲_第2章.md', '# 第2章 细纲\n\n**本章目标**：林照通过三试中的第一试。\n**冲突与对手**：同批弟子赵衡刁难。\n**看点与爽点**：林照以玉佩之力化解险境。\n**章末钩子**：长老发现玉佩纹路与宗门禁地相同。\n');
  console.log(`作品：已建《青岚纪（演示）》 ${b.id}`);
} else {
  console.log('作品：演示作品已存在');
}
