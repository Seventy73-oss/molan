#!/usr/bin/env node
'use strict';
/**
 * booksetup-patch.cjs —— 「建书档案保存」链路修复补丁（可审计 · 幂等 · 只读原 bundle）
 *
 * 从同目录 bundle.js（原始产物，整体 sha256 / 字节数已钉死）中，仅替换 3 个限定片段，
 * 输出同目录 patched-bundle.js。不修改 CSS，不修改任何其它文件；重复执行结果逐字节一致。
 *
 * 修复契约（逐条对应）：
 *   1. store.applyBookSetup 明确返回 boolean：成功 true / 失败 false。
 *      原实现 catch 吞掉错误后隐式 return undefined，UI 误判为成功。
 *   2. A2 组件「保存」按钮仅在返回值 === true 时才把该档案标记为「已存」。
 *   3. 所有按钮处理函数内部消化异常（try/catch），不产生 unhandledrejection。
 *   4. uN 对每个 save_chat_output 回包检查 ok===true，否则抛错。
 *      原实现完全不看回包，后端返回失败也会静默跳过。
 *   5. 无 bridge（Z 为假）时走本地模拟分支，绝不调用 X()/invoke，杜绝模拟写入。
 *   6. 「全部保存并创建」使用后端 apply_book_setup 返回的 result 作为确认结果。
 *   7. 同名且内容完全一致视为幂等重试（read_file 校验通过 → 放行）；
 *      内容不一致 / 读取失败 → 抛错，绝不静默覆盖。
 *   8. 刷新目录 / 书列表失败只提示「已保存，但目录刷新失败」，
 *      绝不把已完成的保存当失败（原实现把 Nt/ba 放进同一 try，失败会走「保存失败」）。
 *   9. 保存成功回写 UI 前校验消息仍属于当前会话消息列表；会话已切走则跳过覆盖。
 *  10. 不改动全局 qu()（isTauri 当前已为 true）。
 *
 * 可审计性：
 *   - 原 bundle 整体 sha256 与字节数、3 个被替换片段的 sha256 全部钉死，不匹配即中止。
 *   - 每个锚点在原文中必须唯一命中，否则中止。
 *   - 输出先做契约 token 断言（17 正 / 4 负），再落盘，最后用 node --check 做 ESM 语法校验。
 *   - 脚本只写 patched-bundle.js 一个文件；校验失败会删除该文件并以非零码退出。
 *   - --check 模式只读：不写盘，仅核对现有 patched-bundle.js 是否为期望产物。
 *
 * 用法：
 *   node booksetup-patch.cjs            # 生成 patched-bundle.js（并自检）
 *   node booksetup-patch.cjs --check    # 只校验现有 patched-bundle.js 是否为期望产物
 */

const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const { execFileSync } = require('child_process');

const DIR = __dirname;
const SRC = path.join(DIR, 'bundle.js');
const OUT = path.join(DIR, 'patched-bundle.js');
const CHECK_ONLY = process.argv.includes('--check');

const sha256 = (s) => crypto.createHash('sha256').update(s, 'utf8').digest('hex');
const countOcc = (hay, needle) => hay.split(needle).length - 1;
const die = (msg) => { console.error('[patch] FAIL: ' + msg); process.exit(1); };

// ── 原始产物指纹 ────────────────────────────────────────────────────────────
const ORIGINAL_SHA256 = 'f53fa2048e89a0a195880fc9e95aa8276c705955d1a23b5f27a7429dd324737e';
const ORIGINAL_BYTES = 754779;

// ── 新片段（JS 源文本） ─────────────────────────────────────────────────────
const STORE_NEW = `applyBookSetup:async(o,c,d)=>{const{bookId:m}=l(),f=o.result?.bookSetup;if(!f)return!1;const h=!d,_=(d?f.files.filter(S=>S.name===d):f.files).map(S=>({group:S.group,name:S.name,content:S.content,draft:S.draft})),g=h?c?.trim()||f.titles[0]||"未命名新书":"";let E=null;try{E=await uN({bookId:m,title:g,genre:f.genre??"",files:_,messageId:o.id})}catch(S){const D=S instanceof Error?S.message:String(S);l().showToast(\`保存失败：\${D}\`);return!1}if(!E||E.ok!==!0){l().showToast("保存失败：服务端未确认保存成功");return!1}const S=h?E.result??{...o.result,bookSetup:{...f,saved:!0}}:o.result;if(!l().messages.some(D=>D.id===o.id))return!0;a(D=>({messages:D.messages.map(A=>A.id===o.id?{...A,result:S}:A)}));try{const[D,A]=await Promise.all([Nt(m),ba()]);a({tree:D,books:A,treeOpen:!0})}catch(D){l().showToast(\`已保存，但目录刷新失败：\${D instanceof Error?D.message:String(D)}\`)}if(h){try{await l().loadBookStyle();const D=l().onboardingSkill==="设定整理";a({onboardingActive:!1,onboardingSkill:""}),D&&l().showToast("设定已整理入库，接着让 AI 帮你查缺补漏、补强卖点？",{label:"查缺补漏",run:()=>{l().runChat({text:"帮我给这本书的设定查缺补漏——金手指、主角、反派、世界观、主线，哪里薄弱或有洞，逐项跟我商量怎么补。",skills:["设定共建"],files:[],clearComposer:!1})}})}catch{}}else{const D=_.find(A=>A.group==="设定");if(D)try{await l().openFile(D.group,D.name)}catch{}}return!0}`;

const UN_NEW = `uN=async a=>{if(Z){for(const l of a.files){let o=null,c=!1;try{o=await X("save_chat_output",{bookId:a.bookId,group:l.group,name:l.name,content:l.content})}catch(d){let m=null;try{m=await X("read_file",{bookId:a.bookId,group:l.group,name:l.name})}catch{}if(typeof m!=="string"||m!==l.content){const f=d instanceof Error?d.message:String(d);throw new Error(\`「\${l.name}」保存失败：\${f}\`)}c=!0}if(!c&&(!o||o.ok!==!0)){const d=o&&typeof o=="object"?(o.error??o.message??o.detail):o;throw new Error(\`「\${l.name}」保存未确认：\${d?String(d):"服务端未返回 ok"}\`)}}if(!a.title)return{ok:!0};const E=await X("apply_book_setup",{bookId:a.bookId,title:a.title,genre:a.genre,files:[],messageId:a.messageId,confirmed:!0});if(!E||E.ok!==!0)throw new Error(\`创建书籍未确认：\${E&&typeof E=="object"&&(E.error??E.message)?String(E.error??E.message):"服务端未返回 ok"}\`);return E}for(const l of a.files){const o=St().find(c=>c.dir===l.group);o&&!o.files.some(c=>c.name===l.name)&&(o.files=[...o.files,{name:l.name,size:"1 千字",draft:!!l.draft}])}for(const l of Object.values(Mt)){const o=l.find(c=>c.id===a.messageId);if(o?.result?.bookSetup)return o.result.bookSetup.saved=!0,Promise.resolve(JSON.stringify(o.result))}return Promise.resolve("{}")}`;

const A2_BUTTON_OLD = `const z=async()=>{if(!(D||!y.trim())){A(!0);try{await o(a,y)}finally{A(!1)}}},$=async H=>{await o(a,void 0,H),S(te=>new Set(te).add(H))};`;

const A2_BUTTON_NEW = `const z=async()=>{if(D||!y.trim())return;A(!0);try{await o(a,y)}catch{}finally{A(!1)}},$=async H=>{try{const te=await o(a,void 0,H);te===!0&&S(de=>new Set(de).add(H))}catch{}};`;

// ── 被替换片段：起点锚点 / 终点锚点 / 原文片段 sha256 ───────────────────────
const SPANS = [
  {
    key: 'store.applyBookSetup',
    start: 'applyBookSetup:async(o,c,d)=>{',
    end: ',generateFirstOutline:async()=>{',
    pin: 'e6f24a360653da755d2ceb0180fbc873ce299feba8393a5f21d50f09a2a5f4b2',
    kind: 'full',
    next: STORE_NEW,
  },
  {
    key: 'uN (save_chat_output bridge)',
    start: 'uN=async a=>',
    end: ',z_=a=>{',
    pin: '5729a43491d25fdc468b1636ce684fb428d047ee77d4587a14b26f66e1ab6dea',
    kind: 'full',
    next: UN_NEW,
  },
  {
    key: 'A2 component (button handlers)',
    start: 'function A2({msg:a}){',
    end: 'const O2={standalone:',
    pin: '00b9c91c08bacb7938073b7f653caf2c736ba7f813bf4a8e125b3c4a10d916ec',
    kind: 'inner',
    innerOld: A2_BUTTON_OLD,
    innerNew: A2_BUTTON_NEW,
  },
];

// ── 期望输出上的契约断言（token → 最少出现次数） ─────────────────────────────
const CONTRACT = [
  ['applyBookSetup:async(o,c,d)=>{', 1],
  ['if(!f)return!1;', 1],
  ['E=await uN({bookId:m,title:g,genre:f.genre??"",files:_,messageId:o.id})', 1],
  ['if(!E||E.ok!==!0){l().showToast("保存失败：服务端未确认保存成功");return!1}', 1],
  ['const S=h?E.result??{...o.result,bookSetup:{...f,saved:!0}}:o.result;', 1],
  ['if(!l().messages.some(D=>D.id===o.id))return!0;', 1],
  ['l().showToast(`已保存，但目录刷新失败：${D instanceof Error?D.message:String(D)}`)', 1],
  ['if(D)try{await l().openFile(D.group,D.name)}catch{}}return!0}', 1],
  ['const z=async()=>{if(D||!y.trim())return;A(!0);try{await o(a,y)}catch{}finally{A(!1)}}', 1],
  [',$=async H=>{try{const te=await o(a,void 0,H);te===!0&&S(de=>new Set(de).add(H))}catch{}};', 1],
  ['uN=async a=>{if(Z){', 1],
  ['if(!c&&(!o||o.ok!==!0)){', 1],
  ['throw new Error(`「${l.name}」保存失败：${f}`)', 1],
  ['throw new Error(`「${l.name}」保存未确认：${d?String(d):"服务端未返回 ok"}`)', 1],
  ['const E=await X("apply_book_setup",{bookId:a.bookId,title:a.title,genre:a.genre,files:[],messageId:a.messageId,confirmed:!0});', 1],
  ['throw new Error(`创建书籍未确认：${E&&typeof E=="object"&&(E.error??E.message)?String(E.error??E.message):"服务端未返回 ok"}`)', 1],
  ['if(typeof m!=="string"||m!==l.content)', 1],
];
// 负向断言：这些「坏味道」不得再出现在输出中
const FORBIDDEN = [
  ['if(!E||E.ok!==!0)throw new Error("服务端未确认保存成功")', '旧 store 内联 throw 仍在'],
  ['applyBookSetup:async(o,c,d)=>{const{bookId:m}=l(),f=o.result?.bookSetup;if(!f)return;', '旧 store 签名仍在'],
  ['$=async H=>{await o(a,void 0,H),S(te=>new Set(te).add(H))};', '旧保存回调仍在'],
  ['uN=async a=>{if(Z){for(const l of a.files)try{await X("save_chat_output"', '旧 uN 仍在'],
];

function extract(src) {
  const found = [];
  for (const spec of SPANS) {
    const i = src.indexOf(spec.start);
    if (i < 0) die('找不到起点锚点：' + spec.key + ' / ' + spec.start);
    if (countOcc(src, spec.start) !== 1) die('起点锚点非唯一：' + spec.key);
    const j = src.indexOf(spec.end, i + spec.start.length);
    if (j < 0) die('找不到终点锚点：' + spec.key + ' / ' + spec.end);
    if (countOcc(src, spec.end) !== 1) die('终点锚点非唯一：' + spec.key);
    found.push({ spec: spec, i: i, j: j, body: src.slice(i, j) });
  }
  return found;
}

function build(src) {
  const bytes = Buffer.byteLength(src, 'utf8');
  if (bytes !== ORIGINAL_BYTES) {
    die('原 bundle 字节数不符：期望 ' + ORIGINAL_BYTES + '，实际 ' + bytes);
  }
  const digest = sha256(src);
  if (digest !== ORIGINAL_SHA256) {
    die('原 bundle sha256 不符：期望 ' + ORIGINAL_SHA256 + '，实际 ' + digest);
  }
  console.log('[patch] 原 bundle 指纹 OK  sha256=' + digest + '  bytes=' + bytes);

  const spans = extract(src);
  for (const s of spans) {
    const d = sha256(s.body);
    if (d !== s.spec.pin) {
      die('片段哈希不符：' + s.spec.key + '\n  期望 ' + s.spec.pin + '\n  实际 ' + d);
    }
    console.log('[patch] 片段 OK  ' + s.spec.key + '  len=' + s.body.length + '  sha256=' + d.slice(0, 16) + '…');
  }

  // 必须按起点降序替换：较早的片段长度变化会平移其后的偏移。
  // （uN 位于 store 之前，若先替换 uN 会让 store 的偏移失效。）
  const ordered = spans.slice().sort(function (x, y) { return y.i - x.i; });
  let out = src;
  for (let k = 0; k < ordered.length; k++) {
    const sp = ordered[k];
    let replacement;
    if (sp.spec.kind === 'full') {
      replacement = sp.spec.next;
    } else {
      const n = countOcc(sp.body, sp.spec.innerOld);
      if (n !== 1) die('片段内待替换文本出现 ' + n + ' 次（期望 1）：' + sp.spec.key);
      replacement = sp.body.replace(sp.spec.innerOld, sp.spec.innerNew);
    }
    out = out.slice(0, sp.i) + replacement + out.slice(sp.j);
  }

  for (const pair of CONTRACT) {
    const n = countOcc(out, pair[0]);
    if (n < pair[1]) die('契约断言缺失（出现 ' + n + ' 次，期望 ≥' + pair[1] + '）：' + pair[0]);
  }
  for (const pair of FORBIDDEN) {
    if (countOcc(out, pair[0]) > 0) die('负向断言命中（' + pair[1] + '）：' + pair[0]);
  }
  console.log('[patch] 契约断言 OK（' + CONTRACT.length + ' 正 / ' + FORBIDDEN.length + ' 负）');
  return out;
}

// 用 node --check 做 ESM 语法校验（bundle 含顶层 import）。
// Node 的 --check 对 .js 会按模块语法探测，原 bundle 已验证可通过。
function syntaxCheck(file, label) {
  try {
    execFileSync(process.execPath, ['--check', file], { stdio: 'pipe' });
  } catch (e) {
    const msg = String((e && e.stderr) || (e && e.message) || e).trim();
    die('语法校验失败：' + label + ' → ' + msg.split('\n').slice(0, 6).join('\n'));
  }
  console.log('[patch] 语法校验 OK（node --check）：' + label);
}

function main() {
  const src = fs.readFileSync(SRC, 'utf8');
  const expected = build(src);
  const expectedSha = sha256(expected);
  console.log('[patch] 期望产物 sha256=' + expectedSha + '  bytes=' + Buffer.byteLength(expected, 'utf8'));

  if (CHECK_ONLY) {
    if (!fs.existsSync(OUT)) die('--check 模式下 patched-bundle.js 不存在');
    const cur = fs.readFileSync(OUT, 'utf8');
    const curSha = sha256(cur);
    if (curSha !== expectedSha) die('patched-bundle.js 与期望产物不一致：' + curSha);
    syntaxCheck(OUT, 'patched-bundle.js');
    console.log('[patch] --check OK：patched-bundle.js 与期望产物逐字节一致');
    return;
  }

  fs.writeFileSync(OUT, expected, 'utf8');
  try {
    const written = fs.readFileSync(OUT, 'utf8');
    if (sha256(written) !== expectedSha) throw new Error('落盘后哈希不一致');
    syntaxCheck(OUT, 'patched-bundle.js');
  } catch (e) {
    try { fs.unlinkSync(OUT); } catch (ignore) {}
    die('产物自检失败，已删除输出：' + String((e && e.message) || e));
  }
  console.log('[patch] 已写出 ' + OUT);
  console.log('[patch] 产物 sha256=' + expectedSha);
}

try { main(); } catch (e) { die(String((e && e.stack) || e)); }
