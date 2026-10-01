#!/usr/bin/env node
// Paper Studio 浏览器验证（真实 Rust 服务 + 隔离数据目录 + mock:// 确定性渠道）。
// 这是协议/流程/界面验证，不代表真实模型的写作质量。
//   1) 启动隔离服务：MOLAN_ROOT=<临时目录> PORT=17482 MOLAN_WEB_DIR=frontend/dist molan-server
//   2) node tools/dev-seed.mjs http://127.0.0.1:17482
//   3) node frontend/e2e/run.mjs http://127.0.0.1:17482 [截图目录]
// 使用本机已安装的 Microsoft Edge（playwright-core，channel=msedge），不下载浏览器。
import { mkdirSync } from 'node:fs';
import { resolve } from 'node:path';
import { chromium } from 'playwright-core';

const base = process.argv[2] ?? 'http://127.0.0.1:17482';
const shots = resolve(process.argv[3] ?? '../docs/refactor/screenshots');
if (!/^http:\/\/(127\.0\.0\.1|localhost)(:\d+)?$/.test(base)) throw new Error('只允许本机隔离服务');
mkdirSync(shots, { recursive: true });

const results = [];
const pageErrors = [];
function check(name, ok, detail = '') {
  results.push({ name, ok: !!ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? `  — ${detail}` : ''}`);
}

async function ipc(cmd, args = {}) {
  const r = await fetch(`${base}/ipc/${cmd}`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ args }) });
  let out;
  for (const line of (await r.text()).split('\n')) {
    if (!line.trim()) continue;
    const j = JSON.parse(line);
    if (j.err) throw new Error(`${cmd}: ${j.err.message}`);
    if ('r' in j) out = j.r;
  }
  return out;
}

/** 轮询服务端事实直到满足（不靠固定等待）；返回 {ok, value, ms}。 */
async function until(read, ok, timeoutMs = 10000) {
  const t0 = Date.now();
  let value;
  while (Date.now() - t0 < timeoutMs) {
    value = await read();
    if (ok(value)) return { ok: true, value, ms: Date.now() - t0 };
    await new Promise((r) => setTimeout(r, 200));
  }
  return { ok: false, value, ms: Date.now() - t0 };
}

const books = await ipc('list_books');
const demo = books.find((b) => b.title === '青岚纪（演示）');
if (!demo) throw new Error('请先运行 tools/dev-seed.mjs');
const bookUrl = `${base}/#/book/${demo.id}`;

const browser = await chromium.launch({ channel: 'msedge', headless: true });

async function newPage(viewport = { width: 1440, height: 900 }, colorScheme = 'light') {
  const ctx = await browser.newContext({ viewport, colorScheme, deviceScaleFactor: 1, locale: 'zh-CN' });
  const page = await ctx.newPage();
  page.on('pageerror', (e) => pageErrors.push(`${page.url()} :: ${e.message}`));
  page.on('console', (m) => {
    if (m.type() === 'error' && !/favicon/.test(m.text())) pageErrors.push(`console: ${m.text()}`);
  });
  return { ctx, page };
}

async function noOverflow(page, label) {
  const o = await page.evaluate(() => ({ sw: document.documentElement.scrollWidth, cw: document.documentElement.clientWidth }));
  check(`无横向溢出 · ${label}`, o.sw <= o.cw + 1, `scrollWidth=${o.sw} clientWidth=${o.cw}`);
}

async function shot(page, name) {
  await page.waitForTimeout(250);
  await page.screenshot({ path: resolve(shots, `${name}.png`), fullPage: false });
}

async function openDoc(page, group, name) {
  await page.locator('.tree__name', { hasText: name.replace(/\.md$/, '') }).first().click();
  await page.locator('textarea.manuscript').waitFor();
  await page.waitForFunction((g) => document.querySelector('.ws__crumb')?.textContent?.includes(g), `${group} / ${name}`);
}

// ---------- 1. 书库与工作台布局（多视口 / 浅色暗色） ----------
for (const [w, h] of [
  [1440, 900],
  [1280, 800],
  [1024, 768],
  [768, 1024],
  [390, 844],
]) {
  for (const scheme of ['light', 'dark']) {
    const { ctx, page } = await newPage({ width: w, height: h }, scheme);
    await page.goto(`${base}/#/`);
    await page.locator('.book-card').first().waitFor();
    await noOverflow(page, `书库 ${w}x${h} ${scheme}`);
    if (scheme === 'light' || w === 1440 || w === 390) await shot(page, `library-${w}-${scheme}`);
    await page.goto(bookUrl);
    await page.locator('.tree__name').first().waitFor({ state: 'attached' });
    if (w >= 1080) await openDoc(page, '正文', '第1章.md');
    await page.waitForTimeout(400);
    await noOverflow(page, `工作台 ${w}x${h} ${scheme}`);
    if (scheme === 'light' || w === 1440 || w === 390) await shot(page, `workspace-${w}-${scheme}`);
    await ctx.close();
  }
}

// ---------- 2. 编辑 → 保存 → 刷新仍在 ----------
{
  const { ctx, page } = await newPage();
  await page.goto(bookUrl);
  await openDoc(page, '设定', '人物.md');
  const ta = page.locator('textarea.manuscript');
  await ta.click();
  await page.keyboard.press('Control+End');
  const marker = `\n- 新角色·苏晚：药堂弟子 ${Date.now()}`;
  await page.keyboard.insertText(marker);
  await page.locator('.ws__bar .status', { hasText: '未保存' }).waitFor();
  check('编辑后显示「未保存」', true);
  await page.keyboard.press('Control+s');
  await page.locator('.ws__bar .status', { hasText: '已保存' }).waitFor({ timeout: 10000 });
  check('Ctrl+S 后显示「已保存」', true);
  const disk = await ipc('doc_read', { bookId: demo.id, group: '设定', name: '人物.md' });
  check('磁盘内容含新编辑（服务端回读）', disk.content.includes(marker.trim()));
  await page.reload();
  await openDoc(page, '设定', '人物.md');
  check('刷新后内容仍在', (await ta.inputValue()).includes(marker.trim()));
  await shot(page, 'editor-saved');
  await ctx.close();
}

// ---------- 3. 双标签页冲突：不丢新稿 ----------
{
  const a = await newPage();
  const b = await newPage();
  for (const p of [a.page, b.page]) {
    await p.goto(bookUrl);
    await openDoc(p, '设定', '世界观.md');
  }
  await b.page.locator('textarea.manuscript').click();
  await b.page.keyboard.press('Control+End');
  await b.page.keyboard.insertText('\nB 标签页先写入的一行。');
  await b.page.keyboard.press('Control+s');
  await b.page.locator('.ws__bar .status', { hasText: '已保存' }).waitFor();
  await a.page.locator('textarea.manuscript').click();
  await a.page.keyboard.press('Control+End');
  await a.page.keyboard.insertText('\nA 标签页基于旧版本的修改。');
  await a.page.keyboard.press('Control+s');
  await a.page.locator('.conflict').waitFor({ timeout: 10000 });
  check('旧基线保存 → 冲突面板（不覆盖）', true);
  const disk = await ipc('doc_read', { bookId: demo.id, group: '设定', name: '世界观.md' });
  check('B 的新稿仍在磁盘上', disk.content.includes('B 标签页先写入的一行') && !disk.content.includes('A 标签页'));
  await shot(a.page, 'conflict');
  await a.page.getByRole('button', { name: '另存我的版本为副本' }).click();
  const copyNames = async () => (await ipc('scan_tree', { bookId: demo.id })).find((g) => g.dir === '设定').files.filter((f) => f.name.includes('副本')).map((f) => f.name);
  const copies = await until(copyNames, (v) => v.length >= 1);
  check('A 的版本另存为副本（不丢稿）', copies.ok, copies.value.join(','));
  await a.ctx.close();
  await b.ctx.close();
}

// ---------- 4. 选区改写 → 产物卡 → 替换选区（只改片段） ----------
{
  const { ctx, page } = await newPage();
  await page.goto(bookUrl);
  await openDoc(page, '正文', '第1章.md');
  const before = (await ipc('doc_read', { bookId: demo.id, group: '正文', name: '第1章.md' })).content;
  const sel = '攥紧了手里的荐书。';
  const start = before.indexOf(sel);
  await page.locator('textarea.manuscript').evaluate((el, [s, e]) => {
    el.focus();
    el.setSelectionRange(s, e);
    el.dispatchEvent(new Event('select', { bubbles: true }));
  }, [start, start + sel.length]);
  await page.locator('textarea.manuscript').dispatchEvent('mouseup');
  await page.getByRole('button', { name: '改写选区' }).click();
  await page.locator('.composer__target', { hasText: '选区' }).waitFor();
  check('选区任务带上目标（基线 + UTF-16 偏移）', true);
  await page.locator('.composer__input').fill('让这一句更有画面感');
  await page.getByRole('button', { name: '发送' }).click();
  const card = page.locator('.acard').filter({ hasText: '修改稿' }).last();
  await card.waitFor({ timeout: 30000 });
  await card.getByRole('button', { name: '替换选区' }).waitFor();
  check('生成修改稿卡片（片段，非全文）', (await card.textContent()).includes('片段'));
  await shot(page, 'artifact-fragment');
  await card.getByRole('button', { name: '替换选区' }).click();
  await card.locator('.status', { hasText: '已保存' }).waitFor({ timeout: 10000 });
  const after = (await ipc('doc_read', { bookId: demo.id, group: '正文', name: '第1章.md' })).content;
  check('磁盘：选区前文不变', after.startsWith(before.slice(0, start)));
  check('磁盘：选区后文不变（含 emoji）', after.endsWith(before.slice(start + sel.length)));
  const arts = await ipc('artifact_list', { bookId: demo.id });
  const frag = arts.find((x) => x.kind === 'revision' && x.state === 'saved');
  const full = frag ? (await ipc('artifact_get', { bookId: demo.id, artifactId: frag.id })).content : '';
  check('磁盘 = 前文 + 产物内容 + 后文（精确替换选区）', !!full && after === before.slice(0, start) + full + before.slice(start + sel.length));
  await shot(page, 'artifact-fragment-saved');
  await ctx.close();
}

// ---------- 5. 细纲任务 → 保存到细纲 → 确认 → 刷新后状态一致 ----------
{
  const { ctx, page } = await newPage();
  await page.goto(bookUrl);
  await page.getByRole('radio', { name: '细纲' }).click();
  await page.locator('.composer__ch input').fill('3');
  await page.getByRole('button', { name: /生效计划/ }).click();
  await page.locator('.plan').waitFor();
  check('发起前显示生效计划（主技能/文风/上下文）', (await page.locator('.plan').textContent()).includes('主技能'));
  {
    const vh = page.viewportSize().height;
    const send = await page.locator('.composer__actions .btn--primary').boundingBox();
    check('展开计划时发送按钮仍完整可见', !!send && send.y >= 0 && send.y + send.height <= vh, send ? `bottom=${Math.round(send.y + send.height)} vh=${vh}` : '未找到');
  }
  await shot(page, 'composer-plan');
  await page.getByRole('button', { name: '发送' }).click();
  const card = page.locator('.acard').filter({ hasText: '第3章细纲' }).last();
  await card.waitFor({ timeout: 30000 });
  check('细纲产物卡：已生成，尚未保存', (await card.textContent()).includes('已生成，尚未保存'));
  await card.getByRole('button', { name: '保存到细纲' }).click();
  await card.locator('.status', { hasText: '已保存' }).waitFor({ timeout: 10000 });
  check('保存后「已保存」且未宣称已确认', !(await card.textContent()).includes('细纲已确认'));
  await card.getByRole('button', { name: '确认细纲' }).click();
  await card.locator('.status', { hasText: '细纲已确认' }).waitFor({ timeout: 10000 });
  await shot(page, 'artifact-outline-confirmed');
  await page.reload();
  const again = page.locator('.acard').filter({ hasText: '第3章细纲' }).last();
  await again.waitFor({ timeout: 15000 });
  check('刷新后卡片状态仍为「细纲已确认」', (await again.textContent()).includes('细纲已确认'));
  await ctx.close();
}

// ---------- 6. 生产线：前序记忆 → 确认第2章细纲 → 起草正文 → 待审 → 定稿 ----------
{
  // 前序章节记忆未同步时章节服务会拒绝起草（正确行为）；先显式重建第1章记忆
  await ipc('rebuild_memory', { bookId: demo.id, ch: 1 });
  for (let i = 0; i < 40; i++) {
    const ms = await ipc('memory_status', { bookId: demo.id });
    if ((ms.memories ?? []).some((m) => m.ch === 1 && m.status === 'valid')) break;
    await new Promise((r) => setTimeout(r, 500));
  }
  const ms = await ipc('memory_status', { bookId: demo.id });
  check('第1章记忆已同步（起草前置条件）', (ms.memories ?? []).some((m) => m.ch === 1 && m.status === 'valid'), JSON.stringify(ms.jobs ?? []).slice(0, 160));
  const { ctx, page } = await newPage();
  await page.goto(bookUrl);
  await page.getByRole('tab', { name: /生产线/ }).click();
  const row = page.locator('.pipe-row').filter({ hasText: '第2章' });
  await row.waitFor();
  if (await row.getByRole('button', { name: '确认细纲' }).count()) {
    await row.getByRole('button', { name: '确认细纲' }).click();
    await row.getByRole('button', { name: '确认细纲' }).waitFor({ state: 'detached', timeout: 10000 });
  }
  await row.getByRole('button', { name: '起草正文' }).click();
  const pending = await until(() => ipc('list_pending_chapters', { bookId: demo.id }), (v) => v.some((p) => p.ch === 2), 60000);
  const formalBefore = await ipc('doc_read', { bookId: demo.id, group: '正文', name: '第2章.md' });
  check('起草后进入「正文待审」队列（不是定稿）', pending.ok && !formalBefore.exists, `${pending.ms}ms`);
  await shot(page, 'pipeline-after-draft');
  await page.getByRole('tab', { name: /待审/ }).click();
  check('待审列表显示审稿结论徽标', ((await page.locator('.pend-item').filter({ hasText: '第 2 章' }).textContent()) ?? '').includes('审稿通过'));
  await page.locator('.pend-item').filter({ hasText: '第 2 章' }).getByRole('button', { name: '阅读并处理' }).click();
  await page.getByRole('button', { name: /定稿（按此版本）/ }).waitFor();
  await shot(page, 'pending-reader');
  check('待审阅读器显示针对当前版本的审稿结论', await page.getByText('审稿通过（针对当前版本）').isVisible());
  const tClick = Date.now();
  await page.getByRole('button', { name: /定稿（按此版本）/ }).click();
  // 界面完成信号：服务端返回定稿结果后阅读器关闭（服务端耗时见 local.mjs 打印的「审批通过（服务端 Nms）」）
  await page.getByRole('dialog', { name: /第 2 章/ }).waitFor({ state: 'detached', timeout: 15000 });
  const uiMs = Date.now() - tClick;
  const formal = await ipc('doc_read', { bookId: demo.id, group: '正文', name: '第2章.md' });
  check('定稿后正式稿存在（界面完成时磁盘已写入）', formal.exists && formal.content.length > 100, `点击→界面完成 ${uiMs}ms`);
  await ctx.close();
}

// ---------- 8. 文件回收站：删除 → 按真实 trashId 恢复 ----------
{
  const { ctx, page } = await newPage();
  await page.goto(bookUrl);
  const item = page.locator('.tree__file').filter({ hasText: '世界观（副本' }).first();
  await item.waitFor();
  const fname = (await item.locator('.tree__name').getAttribute('title')) ?? '';
  await item.getByRole('button', { name: /操作/ }).click();
  await page.getByRole('menuitem', { name: '移到回收站' }).click();
  await page.getByRole('dialog').getByRole('button', { name: '移到回收站' }).click();
  // 轮询服务端目录（不靠固定等待）：present=期望文件在「设定」中
  const inTree = async (present) => {
    for (let i = 0; i < 25; i++) {
      const tree = await ipc('scan_tree', { bookId: demo.id });
      if (tree.find((g) => g.dir === '设定').files.some((f) => f.name === fname) === present) return true;
      await page.waitForTimeout(200);
    }
    return false;
  };
  check('删除后文件离开目录', await inTree(false), fname);
  await page.locator('.tree__tools').getByRole('button', { name: '目录操作' }).click();
  await page.getByRole('menuitem', { name: '文件回收站' }).click();
  // 回收站里还有定稿时移走的待审稿：按文件名定位要恢复的那一行
  await page.getByRole('dialog').locator('li', { hasText: `设定/${fname}` }).getByRole('button', { name: '恢复' }).click();
  check('按 trashId 恢复后文件回到目录', await inTree(true), fname);
  await ctx.close();
}

// ---------- 9. 技能库 / 设置 / 暗色产物卡 / 手机助手抽屉 ----------
{
  const { ctx, page } = await newPage();
  await page.goto(`${base}/#/skills`);
  await page.locator('.skill-item').first().waitFor();
  await page.locator('.skill-item', { hasText: '展开正文写作' }).click();
  await page.locator('.template').waitFor();
  await noOverflow(page, '技能库 1440');
  await shot(page, 'skills');
  await page.goto(`${base}/#/settings/channels`);
  await page.locator('.channel').first().waitFor();
  await shot(page, 'settings-channels');
  await page.goto(`${base}/#/settings/book`);
  await page.waitForTimeout(500);
  await shot(page, 'settings-book');
  await ctx.close();
}
{
  const { ctx, page } = await newPage({ width: 1440, height: 900 }, 'dark');
  await page.goto(bookUrl);
  await page.locator('.acard').first().waitFor({ timeout: 15000 });
  await shot(page, 'artifacts-dark');
  await ctx.close();
}
{
  const { ctx, page } = await newPage({ width: 390, height: 844 }, 'light');
  await page.goto(bookUrl);
  await page.getByRole('button', { name: '展开辅助区' }).click();
  await page.locator('.composer').waitFor();
  await noOverflow(page, '手机 · 助手抽屉');
  await shot(page, 'mobile-assistant');
  await page.getByRole('button', { name: '关闭辅助区' }).click();
  await page.getByRole('button', { name: '展开资料目录' }).click();
  await page.locator('.tree__name').first().click();
  await page.locator('textarea.manuscript').waitFor();
  await noOverflow(page, '手机 · 编辑');
  await shot(page, 'mobile-editor');
  await ctx.close();
}

// ---------- 10. 书库：新建（长书名）/ 切书隔离 / 窄屏 / 空结果 / 作品回收站 ----------
{
  const longTitle = '长书名测试：一部名字非常非常长的长篇玄幻小说用来检查标题换行与窄屏布局';
  const { ctx, page } = await newPage();
  await page.goto(`${base}/#/`);
  await page.locator('.book-card').first().waitFor();
  await page.getByRole('button', { name: '新建作品' }).first().click();
  const dlg = page.getByRole('dialog', { name: '新建作品' });
  await dlg.locator('input').first().fill(longTitle);
  await dlg.getByRole('button', { name: '创建' }).click();
  const created = await until(() => ipc('list_books'), (v) => v.some((b) => b.title === longTitle));
  check('书库新建作品（真实创建）', created.ok);
  const other = created.value.find((b) => b.title === longTitle);
  // 切书隔离：在演示书发起任务后立即切到新书，迟到的响应不得出现在新书
  // （新建后已进入新书；hash 导航返回时界面可能仍是新书，须等演示书真正显示后再操作）
  await page.goto(bookUrl);
  await page.locator('.ws__title', { hasText: demo.title }).waitFor();
  await page.locator('.composer__input').waitFor();
  await page.getByRole('radio', { name: '聊天' }).click();
  await page.locator('.composer__input').fill('切书隔离测试消息');
  await page.getByRole('button', { name: '发送' }).click();
  await page.goto(`${base}/#/book/${other.id}`);
  await page.locator('.ws__title', { hasText: longTitle.slice(0, 6) }).waitFor();
  await page.waitForTimeout(2000);
  const stream = (await page.locator('.assistant__stream').textContent()) ?? '';
  const leakedCards = await page.locator('.assistant__stream .acard').count();
  check('切换作品后旧作品的运行与消息不串到新作品', !stream.includes('切书隔离测试消息') && leakedCards === 0, `stream=${stream.slice(0, 80)} cards=${leakedCards}`);
  const runs = await until(() => ipc('list_sessions', { bookId: demo.id }), (v) => Array.isArray(v) && v.length > 0);
  check('旧作品的运行仍在原作品会话中完成', runs.ok);
  // 窄屏长书名
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${base}/#/`);
  await page.locator('.book-card').first().waitFor();
  await noOverflow(page, '书库 · 长书名 390');
  await shot(page, 'library-long-title-390');
  await page.setViewportSize({ width: 1440, height: 900 });
  // 搜索无结果 → 空状态
  await page.locator('input[aria-label="搜索作品"]').fill('不存在的书名zzz');
  check('搜索无结果时显示空状态', await page.getByText('没有匹配的作品').isVisible());
  await page.locator('input[aria-label="搜索作品"]').fill('');
  // 移到作品回收站（可恢复）
  const card = page.locator('.book-card').filter({ hasText: longTitle.slice(0, 6) });
  await card.getByRole('button', { name: /更多操作/ }).click();
  await page.getByRole('menuitem', { name: '移到回收站' }).click();
  await page.getByRole('dialog').getByRole('button', { name: '移到回收站' }).click();
  const gone = await until(() => ipc('list_books'), (v) => !v.some((b) => b.title === longTitle));
  check('移到作品回收站后书库不再显示', gone.ok);
  await page.getByRole('button', { name: '作品回收站' }).click();
  const inTrash = page.getByRole('dialog', { name: '作品回收站' }).getByText(longTitle.slice(0, 6)).first();
  check('作品回收站里可见被删作品', await inTrash.waitFor({ timeout: 10000 }).then(() => true, () => false));
  await ctx.close();
}

// ---------- 11. 生成后原文被改：卡片标「原文已变化」，迟到的旧基线替换被拒、磁盘不变 ----------
{
  const { ctx, page } = await newPage();
  await page.goto(bookUrl);
  await openDoc(page, '设定', '人物.md');
  const before = await ipc('doc_read', { bookId: demo.id, group: '设定', name: '人物.md' });
  const line = before.content.split('\n').find((l) => l.trim().length >= 8).trim();
  const sel = line.slice(0, 8);
  const start = before.content.indexOf(sel);
  await page.locator('textarea.manuscript').evaluate((el, [s, e]) => {
    el.focus();
    el.setSelectionRange(s, e);
    el.dispatchEvent(new Event('select', { bubbles: true }));
  }, [start, start + sel.length]);
  await page.locator('textarea.manuscript').dispatchEvent('mouseup');
  await page.getByRole('button', { name: '改写选区' }).click();
  await page.locator('.composer__input').fill('换个说法');
  await page.getByRole('button', { name: '发送' }).click();
  const fresh = page.locator('.acard').filter({ hasText: '人物.md' }).last();
  await fresh.getByRole('button', { name: '替换选区' }).waitFor({ timeout: 30000 });
  // 生成后、应用前：另一个标签页追加了一行（作者的新内容）
  const appended = '\n另一个标签页追加的一行。';
  const w = await ipc('doc_write', { bookId: demo.id, group: '设定', name: '人物.md', op: 'append', baseHash: before.hash, content: appended, idempotencyKey: `e2e-append-${Date.now()}` });
  check('并发写入：另一处追加成功', w.commit === 'committed', w.commit);
  await page.reload();
  const stale = page.locator('.acard').filter({ hasText: '人物.md' }).last();
  await stale.waitFor({ timeout: 15000 });
  check('原文变化后卡片显示「原文已变化」', ((await stale.textContent()) ?? '').includes('原文已变化'));
  check('原文变化后主动作改为「与当前原文比较」', await stale.getByRole('button', { name: '与当前原文比较' }).isVisible());
  await stale.scrollIntoViewIfNeeded();
  await shot(page, 'artifact-base-changed');
  const arts = await ipc('artifact_list', { bookId: demo.id });
  const a = arts.find((x) => x.kind === 'revision' && x.target?.name === '人物.md');
  const r = await ipc('artifact_deliver', { bookId: demo.id, artifactId: a.id, action: 'save', op: 'replace_range', group: '设定', name: '人物.md', baseHash: before.hash, start: a.target.start, end: a.target.end, expected: sel, idempotencyKey: `late-${Date.now()}` });
  const disk = await ipc('doc_read', { bookId: demo.id, group: '设定', name: '人物.md' });
  check('迟到的旧基线替换被拒，磁盘保留新内容', r.delivery.ok === false && disk.content === before.content + appended, r.delivery.receipt?.error?.code ?? r.delivery.status);
  await ctx.close();
}

// ---------- 12. 冲突状态：同章细纲已存在时「保存到细纲」不覆盖，转为选择去向 ----------
{
  const { ctx, page } = await newPage();
  await page.goto(bookUrl);
  await page.getByRole('radio', { name: '细纲' }).click();
  await page.locator('.composer__ch input').fill('3');
  await page.locator('.composer__input').fill('再出一版第3章细纲');
  await page.getByRole('button', { name: '发送' }).click();
  const card = page.locator('.acard').filter({ hasText: '第3章细纲' }).filter({ hasText: '已生成，尚未保存' }).last();
  await card.waitFor({ timeout: 30000 });
  const original = await ipc('doc_read', { bookId: demo.id, group: '细纲', name: '细纲_第3章.md' });
  // 作者先在卡片里改写 AI 文本 → 形成新修订（原生成内容保留为修订 1）
  await card.getByRole('button', { name: '编辑后保存为新修订' }).click();
  const ed = page.getByRole('dialog', { name: /编辑「/ });
  await ed.locator('textarea').fill('# 第3章细纲\n作者改过的另一版：考核改在夜里进行。');
  await ed.getByRole('button', { name: '保存为新修订' }).click();
  const revised = page.locator('.acard').filter({ hasText: '修订 2' }).last();
  await revised.waitFor({ timeout: 10000 });
  check('卡片内编辑形成新修订（仍未保存到书稿）', ((await revised.textContent()) ?? '').includes('已生成，尚未保存'));
  await revised.getByRole('button', { name: '保存到细纲' }).click();
  const dlg = page.getByRole('dialog', { name: '选择保存位置' });
  await dlg.waitFor({ timeout: 10000 });
  check('目标已存在：不覆盖，卡片如实报冲突并弹出保存去向', ((await revised.textContent()) ?? '').includes('目标已存在'));
  const after = await ipc('doc_read', { bookId: demo.id, group: '细纲', name: '细纲_第3章.md' });
  check('已确认的细纲内容未被覆盖', after.hash === original.hash);
  await shot(page, 'artifact-conflict-destination');
  await dlg.getByRole('button', { name: '取消' }).click();
  await ctx.close();
}

// ---------- 13. 技能工坊：AI 起草 → 技能草稿卡 → 保存为技能 ----------
{
  const { ctx, page } = await newPage();
  await page.goto(`${base}/#/skills`);
  await page.locator('.skill-item').first().waitFor();
  await page.getByRole('button', { name: 'AI 起草' }).click();
  await page.getByLabel('技能名').fill('E2E对白法');
  await page.getByRole('button', { name: '生成草稿' }).click();
  const card = page.locator('.acard').filter({ hasText: 'E2E对白法' }).first();
  await card.waitFor({ timeout: 30000 });
  check('技能草稿卡：已生成，尚未保存（未自动进技能库）', ((await card.textContent()) ?? '').includes('已生成，尚未保存') && !(await ipc('list_skills')).some((s) => s.name === 'E2E对白法'));
  await shot(page, 'skill-draft');
  await card.getByRole('button', { name: '保存为技能' }).click();
  const saved = await until(() => ipc('list_skills'), (v) => v.some((s) => s.name === 'E2E对白法'));
  check('保存为技能：技能库出现且只出现一次', saved.ok && saved.value.filter((s) => s.name === 'E2E对白法').length === 1);
  await page.locator('.template').waitFor({ timeout: 10000 });
  check('保存后跳转到该技能详情', ((await page.locator('.split__detail').textContent()) ?? '').includes('E2E对白法'));
  await ctx.close();
}

// ---------- 14. 批量自动写作（单章模式）：复用章节服务，只写到待审，不越界 ----------
{
  const mem = await until(() => ipc('memory_status', { bookId: demo.id }), (v) => (v.memories ?? []).some((m) => m.ch === 2 && m.status === 'valid'), 30000);
  check('第2章定稿后记忆已同步（自动写作前置）', mem.ok, `${mem.ms}ms`);
  const { ctx, page } = await newPage();
  await page.goto(bookUrl);
  await page.getByRole('tab', { name: /生产线/ }).click();
  await page.getByRole('button', { name: /批量自动写作/ }).click();
  await page.locator('.autowrite input[type=number]').first().fill('3');
  await page.locator('.autowrite').getByRole('button', { name: '开始' }).click();
  await page.getByRole('dialog').getByRole('button', { name: '开始' }).click();
  const p3 = await until(() => ipc('list_pending_chapters', { bookId: demo.id }), (v) => v.some((p) => p.ch === 3), 90000);
  const formal3 = await ipc('doc_read', { bookId: demo.id, group: '正文', name: '第3章.md' });
  check('自动写作：第3章进入待审（未直接定稿、未写第4章）', p3.ok && !formal3.exists && !p3.value.some((p) => p.ch === 4), `${p3.ms}ms`);
  await shot(page, 'autowrite');
  // 单章模式不自动审稿：待审区如实显示「尚未审稿」，作者可手动审当前版本
  await page.getByRole('tab', { name: /待审/ }).click();
  const item3 = page.locator('.pend-item').filter({ hasText: '第 3 章' });
  await item3.waitFor({ timeout: 15000 });
  check('自动写作待审稿如实标「尚未审稿」', ((await item3.textContent()) ?? '').includes('尚未审稿'));
  await item3.getByRole('button', { name: '阅读并处理' }).click();
  await page.getByRole('button', { name: '审稿当前版本' }).click();
  await page.getByText('审稿通过（针对当前版本）').waitFor({ timeout: 30000 });
  const r3 = (await ipc('list_pending_chapters', { bookId: demo.id })).find((p) => p.ch === 3)?.review;
  check('手动审稿后结论绑定当前待审稿（current）', r3?.state === 'current', JSON.stringify(r3 ?? null).slice(0, 80));
  await shot(page, 'pending-review-on-demand');
  await ctx.close();
}

// ---------- 7. 键盘可达性抽查 ----------
{
  const { ctx, page } = await newPage();
  await page.goto(`${base}/#/`);
  await page.locator('.book-card').first().waitFor();
  await page.keyboard.press('Tab');
  await page.keyboard.press('Tab');
  const focused = await page.evaluate(() => document.activeElement?.getAttribute('aria-label') || document.activeElement?.textContent?.slice(0, 10));
  check('Tab 可聚焦主导航', !!focused, String(focused));
  await ctx.close();
}

await browser.close();
check('无页面错误（pageerror / console.error）', pageErrors.length === 0, pageErrors.slice(0, 5).join(' | '));
const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} 通过`);
process.exit(failed.length ? 1 : 0);
