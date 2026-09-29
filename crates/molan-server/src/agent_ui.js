// 创作助手（手动线 v2）——Agent 对话 + 确认卡片 + 历史恢复，挂在 window.__molanAgentUI。
//
// 注入方式：由 main.rs 的 include_str! 链注入，排在 glue.js / pipeline_ui.js 之后
// （依赖 window.__TAURI_INTERNALS__）。修改注入脚本后需 cargo build -p molan-server 才生效。
// ⚠️ 本文件会被内联进 HTML：任何注释/字符串里都绝不允许出现 HTML 的 script 闭合标签序列
// （会把脚本提前闭合，整页源码外露）。verify.sh 有硬检查。
//
// 契约：MANUAL-LINE-CONTRACT.md（事件/工件/IPC 形状）。要点：
//   POST /ipc/agent_turn { bookId, sessionId, requestId, message, onEvent:"__CHANNEL__:<id>" }
//   NDJSON 每行 {"ch":"<id>","e":{...}}；e.type ∈ meta|delta|reasoning|step|tool|error|interrupted|done|progress
//   tool 事件可带 artifact{kind: proposal|outline_saved|outline_confirmed|body_draft|body_finalized,...}
//   心跳 ch="__hb__", e.type="progress", chars=-1 → 必须忽略。
//
// 硬约束（不得违反）：
//  1) 停止只发 abort_chat({requestId})，绝不带 sessionId（后端 F1：空闲按会话取消会遗留取消标记，
//     使该会话下一条请求立即 interrupted 且零输出）。
//  2) done.status==="done" 只表示「运行结束」；无工件时只显示「回复完成」，绝不显示「已保存」；
//     有工件时按工件回执显示真实结果（卡片是持久化回执的投影，按钮全部走真实 IPC）。
//  3) 不用 innerHTML 拼接模型输出，一律 textContent。
//  4) 同会话发送期间禁用输入；不做并发重发（F6：同 requestId 并发 = 同一 run 被真跑两遍）。
//  5) 确认/定稿/接受按钮必须核实业务回执（ok/status/hash），失败显示后端原因，绝不假成功。
(() => {
  if (window.__molanAgentUI && window.__molanAgentUI.__ready) return;
  const internals = window.__TAURI_INTERNALS__;
  if (!internals || typeof internals.invoke !== 'function') return;
  const origInvoke = internals.invoke;
  const ipc = (cmd, args) => origInvoke.call(internals, cmd, args || {});
  const doc = document;

  // ============ 与 pipeline_ui.js 同一套 tokens（panelW 两面板一致，ui-check 断言） ============
  const T = {
    bg: '#ffffff', line: '#ececec', lineSoft: '#f6f6f6',
    text: '#1f2328', text2: '#6b7280', text3: '#9ca3af',
    accent: '#d97706', ok: '#16a34a', todo: '#9ca3af', pending: '#d97706', fail: '#dc2626',
    font: '-apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Hiragino Sans GB","Microsoft YaHei",system-ui,sans-serif',
    panelW: 360,
  };

  const el = (tag, cls, text) => {
    const n = doc.createElement(tag);
    if (cls) n.className = cls;
    if (text != null) n.textContent = text;
    return n;
  };
  const truncate = (s, n) => {
    const t = s == null ? '' : String(s);
    return t.length > n ? t.slice(0, n) + '…' : t;
  };
  const errText = (e) => (e && e.message ? e.message : String(e));
  const chOfName = (name) => {
    const m = /第(\d+)章/.exec(String(name || ''));
    return m ? Number(m[1]) : 0;
  };
  const shortHash = (h) => String(h || '').slice(0, 8);

  // ============ 样式（一次注入；类名前缀 wxag__，id 前缀 __wxag_，绝不复用 __wx_toast） ============
  const STYLE_ID = '__wxag_style';
  function ensureStyle() {
    if (doc.getElementById(STYLE_ID)) return;
    const st = el('style');
    st.id = STYLE_ID;
    st.textContent = [
      '#__wxag_panel{position:fixed;right:0;top:64px;bottom:20px;width:' + T.panelW + 'px;max-width:100vw;box-sizing:border-box;',
      'background:' + T.bg + ';border:1px solid ' + T.line + ';border-right:none;border-radius:8px 0 0 8px;',
      'font-family:' + T.font + ';font-size:13px;line-height:1.6;color:' + T.text + ';z-index:10005;',
      'display:flex;flex-direction:column;overflow:hidden}',
      '.wxag__hd{display:flex;align-items:center;gap:8px;padding:14px 14px 12px;border-bottom:1px solid ' + T.line + '}',
      '.wxag__title{font-size:13px;font-weight:600}',
      '.wxag__mode{font-size:11.5px;color:' + T.text3 + '}',
      '.wxag__hdact{margin-left:auto;display:flex;gap:2px}',
      '.wxag__icon{border:none;background:transparent;color:' + T.text3 + ';font-family:inherit;font-size:12px;',
      'line-height:1.6;padding:3px 6px;border-radius:6px;cursor:pointer}',
      '.wxag__icon:hover{background:' + T.lineSoft + ';color:' + T.text + '}',
      // 状态条（pipeline 摘要 + 每章状态点）
      '.wxag__strip{border-bottom:1px solid ' + T.line + ';padding:8px 14px;font-size:12px;color:' + T.text2 + '}',
      '.wxag__stripsum{cursor:pointer;display:flex;gap:6px;align-items:baseline;flex-wrap:wrap}',
      '.wxag__stripnext{color:' + T.text + ';font-weight:600}',
      '.wxag__stripblk{color:' + T.accent + '}',
      '.wxag__chaps{display:none;padding:6px 0 0;max-height:180px;overflow-y:auto}',
      '.wxag__chaps--open{display:block}',
      '.wxag__chaprow{display:flex;align-items:center;gap:8px;padding:2px 0;font-size:11.5px;color:' + T.text2 + '}',
      '.wxag__mk{display:inline-flex;align-items:center;gap:3px}',
      '.wxag__mkdot{width:5px;height:5px;border-radius:50%;flex:none;display:inline-block}',
      '.wxag__msgs{flex:1;overflow-y:auto;padding:14px}',
      '.wxag__msg{padding:0 0 14px}',
      '.wxag__role{font-size:11.5px;color:' + T.text3 + ';padding:0 0 4px}',
      '.wxag__text{white-space:pre-wrap;word-break:break-word}',
      '.wxag__reason{margin:2px 0 8px;padding:6px 9px;border-left:2px solid ' + T.line + ';font-size:11.5px;',
      'color:' + T.text3 + ';white-space:pre-wrap;word-break:break-word;cursor:pointer}',
      '.wxag__tool{display:flex;align-items:flex-start;gap:6px;padding:3px 0;font-size:12px;color:' + T.text2 + '}',
      '.wxag__toolname{color:' + T.text + ';flex:none}',
      '.wxag__toolsum{color:' + T.text3 + ';word-break:break-word}',
      '.wxag__mark{flex:none;width:12px;text-align:center}',
      '.wxag__mark--run{color:' + T.accent + '}',
      '.wxag__mark--ok{color:' + T.ok + '}',
      '.wxag__mark--err{color:' + T.fail + '}',
      '.wxag__note{font-size:11.5px;color:' + T.text3 + ';padding:2px 0}',
      '.wxag__note--accent{color:' + T.accent + '}',
      '.wxag__note--ok{color:' + T.ok + '}',
      '.wxag__note--fail{color:' + T.fail + '}',
      // 工件卡片（确认动作的真实入口）
      '.wxag__card{margin:6px 0 10px;border:1px solid ' + T.line + ';border-radius:8px;padding:9px 11px}',
      '.wxag__card--ok{border-color:' + T.ok + '}',
      '.wxag__card--pend{border-color:' + T.accent + '}',
      '.wxag__card--fail{border-color:' + T.fail + '}',
      '.wxag__cardhd{font-size:12.5px;font-weight:600;display:flex;gap:6px;align-items:baseline;flex-wrap:wrap}',
      '.wxag__badge{font-size:11px;font-weight:400;color:' + T.text3 + '}',
      '.wxag__badge--ok{color:' + T.ok + '}',
      '.wxag__badge--pend{color:' + T.accent + '}',
      '.wxag__badge--fail{color:' + T.fail + '}',
      '.wxag__cardmeta{font-size:11.5px;color:' + T.text3 + ';padding:2px 0 0;word-break:break-all}',
      '.wxag__issues{margin:4px 0 0;padding-left:16px;font-size:11.5px;color:' + T.text2 + '}',
      '.wxag__cardacts{display:flex;flex-wrap:wrap;gap:6px;padding:7px 0 0}',
      '.wxag__cbtn{border:1px solid ' + T.line + ';background:' + T.bg + ';color:' + T.text + ';font-family:inherit;',
      'font-size:12px;line-height:1.6;padding:3px 10px;border-radius:6px;cursor:pointer}',
      '.wxag__cbtn:hover{border-color:' + T.accent + ';color:' + T.accent + '}',
      '.wxag__cbtn--main{border-color:' + T.accent + ';background:' + T.accent + ';color:#fff}',
      '.wxag__cbtn--main:hover{background:#c2660a;color:#fff}',
      '.wxag__cbtn--danger:hover{border-color:' + T.fail + ';color:' + T.fail + '}',
      '.wxag__cbtn[disabled]{color:' + T.text3 + ';border-color:' + T.line + ';background:' + T.lineSoft + ';cursor:not-allowed}',
      // 全文/差异弹窗（单例）
      '#__wxag_modal{position:fixed;inset:0;z-index:10010;background:rgba(15,17,21,.45);display:flex;',
      'align-items:center;justify-content:center;padding:20px;box-sizing:border-box}',
      '.wxag__modalcard{background:' + T.bg + ';border-radius:10px;max-width:920px;width:100%;max-height:86vh;',
      'display:flex;flex-direction:column;overflow:hidden;font-family:' + T.font + '}',
      '.wxag__modalhd{display:flex;align-items:center;gap:8px;padding:12px 14px;border-bottom:1px solid ' + T.line + ';',
      'font-size:13px;font-weight:600;color:' + T.text + '}',
      '.wxag__modalcols{display:flex;gap:0;overflow:hidden;flex:1;min-height:0}',
      '.wxag__modalcol{flex:1;min-width:0;display:flex;flex-direction:column;border-right:1px solid ' + T.lineSoft + '}',
      '.wxag__modalcol:last-child{border-right:none}',
      '.wxag__collabel{font-size:11.5px;color:' + T.text3 + ';padding:6px 12px;border-bottom:1px solid ' + T.lineSoft + ';flex:none}',
      '.wxag__pre{margin:0;padding:10px 12px;font-size:12px;line-height:1.7;white-space:pre-wrap;word-break:break-word;',
      'overflow-y:auto;flex:1;font-family:ui-monospace,SFMono-Regular,Consolas,monospace;color:' + T.text + '}',
      // 快捷动作 + 输入区
      '.wxag__quick{display:flex;flex-wrap:wrap;gap:6px;padding:8px 14px 0}',
      '.wxag__qbtn{border:1px dashed ' + T.line + ';background:' + T.bg + ';color:' + T.text2 + ';font-family:inherit;',
      'font-size:11.5px;line-height:1.6;padding:2px 9px;border-radius:999px;cursor:pointer}',
      '.wxag__qbtn:hover{border-color:' + T.accent + ';color:' + T.accent + '}',
      '.wxag__qbtn[disabled]{color:' + T.text3 + ';cursor:not-allowed}',
      '.wxag__ft{border-top:1px solid ' + T.line + ';padding:12px 14px 14px}',
      '.wxag__ta{width:100%;box-sizing:border-box;border:1px solid ' + T.line + ';border-radius:8px;',
      'font-family:inherit;font-size:13px;line-height:1.6;color:' + T.text + ';padding:8px 10px;resize:vertical;',
      'min-height:64px;outline:none}',
      '.wxag__ta:focus{border-color:' + T.accent + '}',
      '.wxag__ta[disabled]{background:' + T.lineSoft + ';color:' + T.text3 + '}',
      '.wxag__acts{display:flex;gap:8px;padding:8px 0 0}',
      '.wxag__send{flex:1;border:1px solid ' + T.accent + ';background:' + T.accent + ';color:#fff;font-family:inherit;',
      'font-size:13px;line-height:1.6;padding:7px 12px;border-radius:8px;cursor:pointer}',
      '.wxag__send:hover{background:#c2660a;border-color:#c2660a}',
      '.wxag__send[disabled]{background:' + T.lineSoft + ';border-color:' + T.line + ';color:' + T.text3 + ';cursor:not-allowed}',
      '.wxag__stop{border:1px solid ' + T.line + ';background:' + T.bg + ';color:' + T.text + ';font-family:inherit;',
      'font-size:13px;line-height:1.6;padding:7px 12px;border-radius:8px;cursor:pointer}',
      '.wxag__stop:hover{border-color:' + T.fail + ';color:' + T.fail + '}',
      '.wxag__stop[disabled]{color:' + T.text3 + ';cursor:not-allowed;border-color:' + T.line + '}',
      '.wxag__hint{padding:8px 0 0;font-size:11.5px;color:' + T.text3 + '}',
      // 助手入口（独立细条，避免与生产线入口重叠）
      '#__wxag_entry{position:fixed;right:0;top:52%;z-index:10006;border:1px solid ' + T.line + ';border-right:none;',
      'border-radius:8px 0 0 8px;background:' + T.bg + ';color:' + T.text3 + ';font-family:' + T.font + ';',
      'font-size:11px;line-height:1.6;padding:10px 4px;cursor:pointer;writing-mode:vertical-rl;letter-spacing:.08em}',
      '#__wxag_entry:hover{color:' + T.accent + ';border-color:' + T.accent + '}',
      'html.__wxag_reserve{padding-right:' + T.panelW + 'px;overflow-x:hidden}',
    ].join('');
    (doc.head || doc.documentElement || doc.body).appendChild(st);
  }

  const TOOL_LABELS = {
    scan_book_tree: '扫描资料树', read_book_file: '读取文件', get_pipeline_state: '查生产线状态',
    list_pending_chapters: '查待审队列', get_chapter_context: '查前文记忆', list_skills: '查技能库',
    get_effective_skills: '查生效技能', create_change_proposal: '创建变更提案',
    draft_chapter_outline: '起草章细纲', confirm_chapter_outline: '确认细纲入库',
    draft_chapter_body: '起草章正文', finalize_chapter_draft: '定稿',
  };
  const STAGE_LABELS = {
    outline: '去写全书大纲', setup: '整理建书档案', chapter_outline: '起草细纲',
    outline_confirm: '确认细纲入库', chapter_body: '起草正文', review: '审核待审稿', memory_fix: '处理记忆同步',
  };

  // ============ Channel 构造（官方 Tauri v2 协议，与 glue 的 clone/parseLine 一致） ============
  // 优先复用宿主 Channel 构造；找不到时用 internals.transformCallback 自建协议等价对象——
  // 走同一条 transformCallback → __CHANNEL__:<id> → runCallback 路径，不另起私有协议。
  function makeChannel(handler) {
    const onMsg = (raw) => {
      const payload = (raw && typeof raw === 'object' && 'message' in raw) ? raw.message : raw;
      try { handler(payload); } catch (e) {}
    };
    const Ctor = (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.Channel)
      || (window.__TAURI__ && window.__TAURI__.Channel)
      || (internals && internals.Channel)
      || null;
    if (typeof Ctor === 'function') {
      try {
        const ch = new Ctor(onMsg);
        if (ch && ch.id != null && typeof ch.toJSON === 'function') return { ch: ch, kind: 'native' };
      } catch (e) {}
    }
    if (typeof internals.transformCallback !== 'function') return null;
    const id = internals.transformCallback(onMsg);
    let om = null;
    const shim = {
      id: id,
      toJSON() { return '__CHANNEL__:' + id; },
      get onmessage() { return om; },
      set onmessage(f) { om = f; },
    };
    return { ch: shim, kind: 'shim' };
  }

  // ============ 运行状态机 ============
  // idle -> submitting -> streaming <-> retrying -> cancelling -> 终态
  const S = {
    bookId: '', sessionId: '',
    phase: 'idle',
    requestId: '', runId: '',
    channelKind: '',
    stopPending: false,
  };

  let panelEl = null, msgsEl = null, taEl = null, sendEl = null, stopEl = null, hintEl = null;
  let stripEl = null, chaptersEl = null, quickEl = null;
  let curBubble = null, curText = '', curReason = null;
  let tools = new Map();
  let toolOrder = [];
  let proposalSeen = false, interruptedSeen = false, artifactSeen = false, lastArtifactNote = '';
  let lastDraftRow = null;      // draft_chapter_body 运行行（progress 事件更新字数）
  let lastPipeline = null;      // 最近一次 get_pipeline_state（状态条/快捷动作/对账用）
  let loadedFor = '';           // 已加载历史的 book:session（切换才重载）

  const ROLE = { user: '你', assistant: '创作助手' };

  function scrollDown() { try { if (msgsEl) msgsEl.scrollTop = msgsEl.scrollHeight; } catch (e) {} }

  function addMsg(role) {
    const wrap = el('div', 'wxag__msg');
    wrap.appendChild(el('div', 'wxag__role', ROLE[role] || role));
    const body = el('div', 'wxag__text' + (role === 'user' ? ' wxag__text--user' : ''));
    wrap.appendChild(body);
    if (msgsEl) msgsEl.appendChild(wrap);
    scrollDown();
    return body;
  }

  function addNote(text, cls) {
    const n = el('div', 'wxag__note' + (cls ? ' ' + cls : ''), text);
    if (msgsEl) msgsEl.appendChild(n);
    scrollDown();
    return n;
  }

  // 模型文本一律 textContent 追加，绝不 innerHTML
  function appendDelta(text) {
    if (!curBubble) curBubble = addMsg('assistant');
    curText += text;
    curBubble.textContent = curText;
    scrollDown();
  }

  function appendReason(text) {
    if (!curReason) {
      curReason = el('div', 'wxag__reason');
      curReason.title = '点击折叠/展开';
      curReason.__full = '';
      curReason.__open = true;
      curReason.onclick = () => {
        curReason.__open = !curReason.__open;
        curReason.textContent = curReason.__open ? curReason.__full : '思考过程（点击展开）';
      };
      if (msgsEl) msgsEl.appendChild(curReason);
    }
    curReason.__full += text;
    if (curReason.__open) curReason.textContent = curReason.__full;
    scrollDown();
  }

  function toolRow(ev) {
    const callId = ev.callId || '';
    let row = callId ? tools.get(callId) : null;
    if (!row) {
      row = el('div', 'wxag__tool');
      row.__mark = el('span', 'wxag__mark wxag__mark--run', '◌');
      row.__name = el('span', 'wxag__toolname', TOOL_LABELS[ev.name] || ev.name || 'tool');
      row.__name.title = ev.name || '';
      row.__sum = el('span', 'wxag__toolsum', '');
      row.appendChild(row.__mark);
      row.appendChild(row.__name);
      row.appendChild(row.__sum);
      if (msgsEl) msgsEl.appendChild(row);
      if (callId) { tools.set(callId, row); toolOrder.push(callId); } else { toolOrder.push(row); }
    }
    const status = ev.status || '';
    if (status === 'running') {
      row.__mark.className = 'wxag__mark wxag__mark--run';
      row.__mark.textContent = '◌';
      if (ev.name === 'draft_chapter_body') { row.__sum.textContent = '生成中（较长，可停止）…'; lastDraftRow = row; }
    } else if (status === 'ok') {
      row.__mark.className = 'wxag__mark wxag__mark--ok';
      row.__mark.textContent = '✓';
      if (lastDraftRow === row) lastDraftRow = null;
    } else if (status === 'error') {
      row.__mark.className = 'wxag__mark wxag__mark--err';
      row.__mark.textContent = '✗';
      if (lastDraftRow === row) lastDraftRow = null;
    }
    if (typeof ev.summary === 'string' && ev.summary) row.__sum.textContent = truncate(ev.summary, 160);
    // 无结构化 artifact 的提案工具（旧后端兼容）：文字指路；有 artifact 时由卡片接管
    if (ev.name === 'create_change_proposal' && status === 'ok' && !ev.artifact && !proposalSeen) {
      proposalSeen = true;
      addNote('提案待你接受（pending 只表示待处理，接受后才生效）', 'wxag__note--accent');
    }
    scrollDown();
    return row;
  }

  // ============ 弹窗（单例；textContent 渲染，Esc/遮罩关闭） ============
  let modalEl = null;
  function closeModal() {
    if (modalEl) { try { modalEl.remove(); } catch (e) {} modalEl = null; }
    if (doc.removeEventListener) doc.removeEventListener('keydown', onModalKey);
  }
  function onModalKey(e) { if (e.key === 'Escape') closeModal(); }
  function openModal(title, cols) {
    closeModal();
    ensureStyle();
    modalEl = el('div');
    modalEl.id = '__wxag_modal';
    const card = el('div', 'wxag__modalcard');
    const hd = el('div', 'wxag__modalhd');
    hd.appendChild(el('span', null, title));
    const x = el('button', 'wxag__icon', '✕');
    x.style.marginLeft = 'auto';
    x.onclick = closeModal;
    hd.appendChild(x);
    card.appendChild(hd);
    const wrap = el('div', 'wxag__modalcols');
    (cols || []).forEach((c) => {
      const col = el('div', 'wxag__modalcol');
      if (c.label) col.appendChild(el('div', 'wxag__collabel', c.label));
      col.appendChild(el('pre', 'wxag__pre', c.text == null ? '' : String(c.text)));
      wrap.appendChild(col);
    });
    card.appendChild(wrap);
    modalEl.appendChild(card);
    modalEl.onclick = (e) => { if (e.target === modalEl) closeModal(); };
    (doc.body || doc.documentElement).appendChild(modalEl);
    if (doc.addEventListener) doc.addEventListener('keydown', onModalKey);
  }

  // ============ 工件卡片（实时事件与历史 steps 共用同一渲染函数） ============
  function setBadge(card, text, cls) {
    let b = card.__badge;
    if (!b) { b = el('span', 'wxag__badge'); card.__hd.appendChild(b); card.__badge = b; }
    b.textContent = text;
    b.className = 'wxag__badge' + (cls ? ' wxag__badge--' + cls : '');
    card.className = 'wxag__card' + (cls ? ' wxag__card--' + (cls === 'pend' ? 'pend' : cls) : '');
  }
  function cbtn(label, cls, onclick) {
    const b = el('button', 'wxag__cbtn' + (cls ? ' ' + cls : ''), label);
    b.onclick = onclick;
    return b;
  }
  function cardNote(card, text, cls) {
    const n = el('div', 'wxag__note' + (cls ? ' ' + cls : ''), text);
    card.insertBefore(n, card.__acts);
    return n;
  }

  // 渲染一张工件卡并返回。opts.history=true 表示来自历史消息（reconcile 随后对账）。
  function renderArtifact(host, art, opts) {
    if (!art || typeof art !== 'object' || !art.kind) return null;
    const o = opts || {};
    const card = el('div', 'wxag__card');
    // data-* 用 setAttribute（与 pipeline_ui 同法；最小 DOM 沙箱无 dataset）
    card.setAttribute('data-kind', art.kind);
    if (art.ch != null) card.setAttribute('data-ch', String(art.ch));
    if (art.proposalId) card.setAttribute('data-pid', String(art.proposalId));
    const hd = el('div', 'wxag__cardhd');
    card.__hd = hd;
    card.__acts = el('div', 'wxag__cardacts');
    card.appendChild(hd);
    const meta = (s) => { card.appendChild(el('div', 'wxag__cardmeta', s)); };
    if (art.kind === 'proposal') {
      hd.appendChild(el('span', null, '变更提案 · ' + (art.group || '') + '/' + (art.name || '')));
      meta((art.summary || '') + '（pending：接受后才写入文件）');
      const diff = cbtn('查看差异', '', async () => {
        diff.disabled = true;
        try {
          const p = await ipc('dw_get_proposal', { bookId: S.bookId, id: art.proposalId });
          openModal('提案差异 · ' + (art.name || ''), [
            { label: '当前内容（基线）', text: (p && p.baseContent) || '（空 = 新建文件）' },
            { label: '提案内容', text: (p && p.proposedContent) || '' },
          ]);
        } catch (e) { cardNote(card, '读取提案失败：' + errText(e), 'wxag__note--fail'); }
        diff.disabled = false;
      });
      const okb = cbtn('接受', 'wxag__cbtn--main', async () => {
        okb.disabled = true; rjb.disabled = true;
        try {
          const r = await ipc('dw_accept_proposal', { bookId: S.bookId, id: art.proposalId });
          if (!r || r.ok !== true || r.status !== 'accepted') throw new Error('回执异常：' + JSON.stringify(r));
          setBadge(card, '已接受', 'ok');
          okb.remove(); rjb.remove();
          // 细纲提案：接受后追加「确认入库」（绑定刚写入的 appliedHash，防期间被改）
          if ((art.group || r.group) === '细纲' && r.appliedHash) {
            const n = chOfName(r.name || art.name);
            if (n > 0) card.__acts.appendChild(cbtn('确认入库', 'wxag__cbtn--main', () => confirmOutline(card, n, r.appliedHash)));
          }
          afterAction();
        } catch (e) {
          cardNote(card, '接受失败（未写入）：' + errText(e), 'wxag__note--fail');
          okb.disabled = false; rjb.disabled = false;
        }
      });
      const rjb = cbtn('拒绝', 'wxag__cbtn--danger', async () => {
        rjb.disabled = true;
        try {
          await ipc('dw_reject_proposal', { bookId: S.bookId, id: art.proposalId, reason: '作者在创作助手面板拒绝' });
          setBadge(card, '已拒绝', '');
          okb.remove(); rjb.remove();
          afterAction();
        } catch (e) { cardNote(card, '拒绝失败：' + errText(e), 'wxag__note--fail'); rjb.disabled = false; }
      });
      card.__acts.appendChild(diff); card.__acts.appendChild(okb); card.__acts.appendChild(rjb);
      setBadge(card, o.history ? '历史提案 · 状态待核对' : '待你处理', 'pend');
    } else if (art.kind === 'outline_saved') {
      hd.appendChild(el('span', null, '细纲草稿 · 第' + art.ch + '章'));
      meta((art.name || '') + ' · ' + (art.chars != null ? art.chars : '?') + ' 字 · hash ' + shortHash(art.hash) + '…（保存≠确认）');
      card.__acts.appendChild(cbtn('查看全文', '', async () => {
        try {
          const text = await ipc('read_file', { bookId: S.bookId, group: '细纲', name: art.name });
          openModal('细纲 · ' + (art.name || ''), [{ label: '当前内容', text: text || '（空）' }]);
        } catch (e) { cardNote(card, '读取失败：' + errText(e), 'wxag__note--fail'); }
      }));
      card.__acts.appendChild(cbtn('确认入库', 'wxag__cbtn--main', () => confirmOutline(card, art.ch, art.hash || '')));
      setBadge(card, '待确认入库', 'pend');
    } else if (art.kind === 'outline_confirmed') {
      hd.appendChild(el('span', null, '细纲已确认 · 第' + art.ch + '章'));
      meta((art.name || '') + ' · hash ' + shortHash(art.hash) + '…' + (art.confirmedAt ? ' · ' + new Date(art.confirmedAt).toLocaleString() : '') + '。此后修改细纲会使确认失效，需重新确认。');
      setBadge(card, '已确认入库', 'ok');
    } else if (art.kind === 'body_draft') {
      hd.appendChild(el('span', null, '正文草稿 · 第' + art.ch + '章'));
      meta((art.group || '正文待审') + '/' + (art.name || '') + ' · ' + (art.chars != null ? art.chars : '?') + ' 字 · hash ' + shortHash(art.hash) + '…（草稿≠定稿）');
      const rv = art.review;
      if (rv && rv.ok === true) meta('剧情审核：通过');
      else if (rv) {
        meta('剧情审核：提出 ' + ((rv.issues || []).length) + ' 条问题' + (rv.note ? '（' + truncate(rv.note, 60) + '）' : ''));
        if ((rv.issues || []).length) {
          const ul = el('ul', 'wxag__issues');
          rv.issues.slice(0, 6).forEach((s) => ul.appendChild(el('li', null, String(s))));
          card.appendChild(ul);
        }
      } else meta('剧情审核：未执行（正文过短）');
      card.__acts.appendChild(cbtn('查看全文', '', async () => {
        try {
          const text = await ipc('read_file', { bookId: S.bookId, group: art.group || '正文待审', name: art.name });
          openModal('正文草稿 · ' + (art.name || ''), [{ label: '待审内容', text: text || '（空）' }]);
        } catch (e) { cardNote(card, '读取失败：' + errText(e), 'wxag__note--fail'); }
      }));
      const okb = cbtn('定稿', 'wxag__cbtn--main', async () => {
        okb.disabled = true; rjb.disabled = true;
        try {
          const r = await ipc('approve_chapter', { bookId: S.bookId, name: art.name });
          if (!r || r.ok !== true) throw new Error('回执异常：' + JSON.stringify(r));
          setBadge(card, r.alreadyApproved ? '已定稿（幂等重入）' : '已定稿', 'ok');
          okb.remove(); rjb.remove();
          cardNote(card, '已定稿：' + (r.finalName || art.name) + '；记忆同步已排队', 'wxag__note--ok');
          pollMemory(card, art.ch);
          afterAction();
        } catch (e) {
          cardNote(card, '定稿失败（未改变稿件）：' + errText(e), 'wxag__note--fail');
          okb.disabled = false; rjb.disabled = false;
        }
      });
      const rjb = cbtn('退回', 'wxag__cbtn--danger', async () => {
        if (!window.confirm('退回第' + art.ch + '章待审稿？稿件将进回收站（可恢复）')) return;
        rjb.disabled = true;
        try {
          const r = await ipc('reject_chapter', { bookId: S.bookId, ch: art.ch, name: art.name });
          if (!r || r.ok !== true) throw new Error('回执异常：' + JSON.stringify(r));
          setBadge(card, '已退回', '');
          okb.remove(); rjb.remove();
          afterAction();
        } catch (e) { cardNote(card, '退回失败：' + errText(e), 'wxag__note--fail'); rjb.disabled = false; }
      });
      card.__acts.appendChild(okb); card.__acts.appendChild(rjb);
      setBadge(card, '待审阅', 'pend');
    } else if (art.kind === 'body_finalized') {
      hd.appendChild(el('span', null, '已定稿 · 第' + art.ch + '章'));
      meta((art.finalName || '') + ' · hash ' + shortHash(art.hash) + '… · 记忆同步已排队（进度见状态条/生产线）');
      setBadge(card, art.alreadyApproved ? '已定稿（幂等重入）' : '已定稿', 'ok');
      if (!o.history) pollMemory(card, art.ch);
    } else {
      return null;
    }
    card.appendChild(card.__acts);
    (host || msgsEl || doc.body).appendChild(card);
    if (!o.history) {
      artifactSeen = true;
      lastArtifactNote = {
        proposal: '提案已创建（见上方卡片，接受后才生效）',
        outline_saved: '细纲草稿已保存（未确认，见上方卡片）',
        outline_confirmed: '细纲已确认入库（见上方回执）',
        body_draft: '正文草稿已进待审（定稿由你决定）',
        body_finalized: '已定稿，记忆同步已排队（见上方回执）',
      }[art.kind] || '';
    }
    scrollDown();
    return card;
  }

  // 确认入库（卡片按钮共用）：expectedHash 传作者看到的版本；后端不一致会拒绝并说明。
  async function confirmOutline(card, ch, expectedHash) {
    const btns = card.__acts.querySelectorAll('button');
    Array.prototype.forEach.call(btns, (b) => { b.disabled = true; });
    try {
      const args = { bookId: S.bookId, ch: ch };
      if (expectedHash) args.expectedHash = expectedHash;
      const r = await ipc('confirm_outline', args);
      if (!r || r.ok !== true || !r.hash) throw new Error('回执异常：' + JSON.stringify(r));
      setBadge(card, '已确认入库', 'ok');
      card.__acts.replaceChildren();
      cardNote(card, '已绑定 hash ' + shortHash(r.hash) + '…；此后修改细纲需重新确认', 'wxag__note--ok');
      afterAction();
    } catch (e) {
      cardNote(card, '确认失败：' + errText(e), 'wxag__note--fail');
      Array.prototype.forEach.call(btns, (b) => { b.disabled = false; });
    }
  }

  // 定稿后的记忆同步轮询（5s×12）：如实显示 同步中/已同步/失败；失败给重试按钮。
  function pollMemory(card, ch) {
    let tries = 0;
    const pick = (arr) => {
      let last = null;
      (Array.isArray(arr) ? arr : []).forEach((r) => { if (r && Number(r.ch) === Number(ch)) last = r; });
      return last;
    };
    const tick = async () => {
      tries += 1;
      let st = null;
      try { st = await ipc('memory_status', { bookId: S.bookId }); } catch (e) { st = null; }
      const mem = st ? pick(st.memories) : null;
      const job = st ? pick(st.jobs) : null;
      if (mem && mem.status === 'valid') { cardNote(card, '记忆已同步', 'wxag__note--ok'); card.__memDone = true; return; }
      if (job && job.status === 'failed') {
        cardNote(card, '记忆同步失败：' + truncate(job.error || '原因见记忆状态', 60), 'wxag__note--fail');
        const rb = cbtn('重试记忆', '', async () => {
          rb.disabled = true;
          try { await ipc('rebuild_memory', { bookId: S.bookId, ch: ch }); rb.remove(); tries = 0; loop(); }
          catch (e) { cardNote(card, '重试失败：' + errText(e), 'wxag__note--fail'); rb.disabled = false; }
        });
        card.__acts.appendChild(rb);
        return;
      }
      if (tries < 12) loop();
      else cardNote(card, '记忆仍在同步中（可稍后在生产线面板查看）', '');
    };
    const loop = () => { clearTimeout(card.__memTimer); card.__memTimer = setTimeout(tick, 5000); };
    if (card.__memDone) return;
    cardNote(card, '记忆同步中…', '');
    loop();
  }

  // ============ 历史恢复 + 对账 ============
  function renderHistory(host, rows) {
    if (!host || !Array.isArray(rows)) return;
    rows.forEach((m) => {
      if (!m || typeof m !== 'object') return;
      if (m.role === 'user') {
        if (m.content) addMsg('user').textContent = m.content;
      } else if (m.role === 'assistant') {
        if (m.content) addMsg('assistant').textContent = m.content;
        const steps = Array.isArray(m.steps) ? m.steps : [];
        steps.forEach((sp) => {
          if (!sp || sp.type !== 'tool') return;
          toolRow({ callId: '', name: sp.name, status: sp.status, summary: (sp.summary || '') + '（历史）' });
          if (sp.artifact && sp.artifact.kind) renderArtifact(host, sp.artifact, { history: true });
        });
        const r = m.result;
        if (r && r.agent && r.status && r.status !== 'done') {
          addNote('该轮运行终态：' + r.status + (r.error ? '（' + truncate(r.error, 80) + '）' : ''), r.status === 'error' ? 'wxag__note--fail' : '');
        }
      }
    });
  }

  const chapterField = (ch, key) => {
    const c = ((lastPipeline && lastPipeline.chapters) || []).find((x) => x && Number(x.n) === Number(ch));
    return c ? c[key] : null;
  };

  // 对账：历史卡片按当前真实状态更新徽标（对不上=「状态待核对」，不猜）。
  async function reconcile() {
    if (!msgsEl || !S.bookId) return;
    let pend = null, props = null;
    try { pend = await ipc('list_pending_chapters', { bookId: S.bookId }); } catch (e) {}
    try { props = await ipc('dw_list_proposals', { bookId: S.bookId, status: 'pending' }); } catch (e) {}
    const pendChs = new Set((Array.isArray(pend) ? pend : []).filter((r) => r && r.status === 'pending').map((r) => Number(r.ch)));
    const propIds = new Set((Array.isArray(props) ? props : []).map((p) => String(p.id)));
    const cards = msgsEl.querySelectorAll('.wxag__card[data-kind]');
    Array.prototype.forEach.call(cards, (card) => {
      const kind = card.getAttribute ? card.getAttribute('data-kind') : null;
      const badgeText = card.__badge ? card.__badge.textContent : '';
      if (badgeText.indexOf('已') === 0) return; // 已是终态徽标（已接受/已定稿…）不动
      if (kind === 'proposal') {
        const live = propIds.has(card.getAttribute('data-pid'));
        setBadge(card, live ? '待你处理' : '已处理或状态待核对', live ? 'pend' : '');
      } else if (kind === 'outline_saved') {
        const os = chapterField(card.getAttribute('data-ch'), 'outlineStatus');
        if (os === 'confirmed') setBadge(card, '已确认入库', 'ok');
        else if (os === 'stale') setBadge(card, '细纲已修改，可重新确认', 'fail');
      } else if (kind === 'body_draft') {
        const ch = Number(card.getAttribute('data-ch'));
        const body = chapterField(ch, 'body');
        if (body === 'approved') setBadge(card, '已定稿', 'ok');
        else if (pendChs.has(ch)) setBadge(card, '待审阅', 'pend');
        else setBadge(card, '已处理或状态待核对', '');
      }
    });
  }

  // ============ 状态条 + 快捷动作 ============
  function stageText(nx) {
    const s = nx && nx.stage;
    if (!s) return '';
    const ch = nx && nx.chapter;
    if (s === 'outline' || s === 'setup') return STAGE_LABELS[s];
    return (STAGE_LABELS[s] || s) + (ch ? ' · 第' + ch + '章' : '');
  }
  function blockerText(b) {
    if (!b) return '';
    if (typeof b === 'string') return b;
    const n = b.chapter != null ? '第' + b.chapter + '章' : '';
    if (b.type === 'outline') return n + '细纲确认后又被修改，需重新确认';
    const L = { failed: '记忆同步失败', stale: '记忆需复核', pending: '记忆同步中', missing: '记忆缺失' };
    if (b.type === 'memory') return n + (L[b.status] || '记忆待处理');
    return n + '待处理';
  }
  function renderStrip() {
    if (!stripEl) return;
    stripEl.replaceChildren();
    const sum = el('div', 'wxag__stripsum');
    if (!lastPipeline) {
      sum.appendChild(el('span', null, '状态读取失败'));
      const r = el('button', 'wxag__qbtn', '重试');
      r.onclick = () => refreshStrip();
      sum.appendChild(r);
      stripEl.appendChild(sum);
      return;
    }
    const nx = lastPipeline.next || {};
    sum.title = '点击展开/收起每章状态';
    sum.appendChild(el('span', 'wxag__stripnext', '下一步：' + (stageText(nx) || '本章流程完整')));
    const bls = Array.isArray(lastPipeline.blockers) ? lastPipeline.blockers : [];
    if (bls.length) sum.appendChild(el('span', 'wxag__stripblk', '需先处理：' + blockerText(bls[0]) + (bls.length > 1 ? ' 等' + bls.length + '项' : '')));
    sum.onclick = () => { if (chaptersEl) chaptersEl.classList.toggle('wxag__chaps--open'); };
    stripEl.appendChild(sum);
    chaptersEl = el('div', 'wxag__chaps');
    const dot = (color, title) => {
      const s = el('span', 'wxag__mk');
      s.title = title || '';
      const d = el('i', 'wxag__mkdot');
      d.style.background = color;
      s.appendChild(d);
      return s;
    };
    (Array.isArray(lastPipeline.chapters) ? lastPipeline.chapters : []).slice(0, 60).forEach((c) => {
      const row = el('div', 'wxag__chaprow');
      row.appendChild(el('span', null, '第' + c.n + '章'));
      const os = c.outlineStatus;
      if (os) row.appendChild(dot(os === 'confirmed' ? T.ok : os === 'stale' ? T.fail : T.pending, '细纲：' + (os === 'confirmed' ? '已确认' : os === 'stale' ? '确认后已修改' : '已保存待确认')));
      row.appendChild(dot(c.body === 'approved' ? T.ok : c.body === 'pending' ? T.pending : T.todo,
        '正文：' + (c.body === 'approved' ? (c.approvalVerified ? '已定稿（凭证核验）' : '正式文件存在，批准来源未验证') : c.body === 'pending' ? '待审' : '未写')));
      row.appendChild(dot(c.memory === 'valid' ? T.ok : c.memory === 'failed' ? T.fail : (c.memory === 'pending' || c.memory === 'stale') ? T.pending : T.todo, '记忆：' + (c.memory || '未同步')));
      chaptersEl.appendChild(row);
    });
    stripEl.appendChild(chaptersEl);
  }
  async function refreshStrip() {
    if (!S.bookId) return;
    try { lastPipeline = await ipc('get_pipeline_state', { bookId: S.bookId }); } catch (e) { lastPipeline = null; }
    renderStrip();
    renderQuick();
  }
  function sendText(text) {
    if (!taEl) return;
    taEl.value = text;
    syncControls();
    send();
  }
  // 快捷动作=对话 shortcut：填入固定指令并走同一 send()（与手动输入完全同路径）
  function renderQuick() {
    if (!quickEl) return;
    quickEl.replaceChildren();
    const nx = (lastPipeline && lastPipeline.next) || {};
    const ch = nx.chapter;
    const q = (label, text) => {
      const b = el('button', 'wxag__qbtn', label);
      b.onclick = () => sendText(text);
      quickEl.appendChild(b);
    };
    if (nx.stage === 'chapter_outline' && ch) q('起草第' + ch + '章细纲', '请为第' + ch + '章起草细纲：先读前文有效事实与结尾，输出本章目标/场景顺序/信息差/结尾钩子，然后用 draft_chapter_outline 保存。');
    if (nx.stage === 'outline_confirm' && ch) q('查看第' + ch + '章细纲', '请展示第' + ch + '章细纲全文，我确认后入库。');
    if (nx.stage === 'chapter_body' && ch) q('起草第' + ch + '章正文', '按已确认的第' + ch + '章细纲写正文，用 draft_chapter_body 起草进待审。');
    if (nx.stage === 'review' && ch) q('查看第' + ch + '章待审稿', '请读取第' + ch + '章待审稿并总结要点，我来决定定稿还是退回。');
    q('查看现状', '用 get_pipeline_state 汇报当前生产线状态和下一步。');
    const busy = S.phase === 'submitting' || S.phase === 'streaming' || S.phase === 'retrying' || S.phase === 'cancelling';
    Array.prototype.forEach.call(quickEl.children, (b) => { b.disabled = busy; });
  }
  function afterAction() { refreshStrip(); }

  // ============ 事件处理 ============
  const TERMINAL_LABEL = { interrupted: '已停止', budget_exhausted: '预算耗尽', tools_unsupported: '渠道不支持工具', error: '失败' };
  const ERROR_LABEL = { BUDGET_EXHAUSTED: '预算耗尽', ROUNDS_EXHAUSTED: '轮次耗尽', TOOLS_UNSUPPORTED: '渠道不支持工具', CANCELLED: '已停止' };

  function setPhase(p) { S.phase = p; syncControls(); }

  function syncControls() {
    const busy = S.phase === 'submitting' || S.phase === 'streaming' || S.phase === 'retrying' || S.phase === 'cancelling';
    if (taEl) taEl.disabled = busy;
    if (sendEl) sendEl.disabled = busy || !taEl || !taEl.value.trim();
    if (stopEl) {
      const canStop = S.phase === 'submitting' || S.phase === 'streaming' || S.phase === 'retrying';
      stopEl.disabled = !canStop;
      stopEl.textContent = S.phase === 'cancelling' || S.stopPending ? '停止中…' : '停止';
    }
    if (quickEl) Array.prototype.forEach.call(quickEl.children, (b) => { b.disabled = busy; });
    if (hintEl) {
      hintEl.textContent = S.phase === 'idle' ? '手动线：细纲入库、正文定稿都由你确认；Agent 起草，你拍板。'
        : S.phase === 'submitting' ? '已发送，等待运行建立…'
          : S.phase === 'streaming' ? '运行中（可停止）'
            : S.phase === 'retrying' ? '上游瞬态错误，同模型重试中…'
              : S.phase === 'cancelling' ? '已请求停止，等待终态…'
                : S.phase === 'done' ? (artifactSeen ? '本轮操作完成（见上方回执）' : '回复完成')
                  : S.phase === 'interrupted' ? '已停止'
                    : '运行结束：' + (TERMINAL_LABEL[S.phase] || S.phase);
    }
  }

  function handleEvent(ev) {
    if (!ev || typeof ev !== 'object') return;
    switch (ev.type) {
      case 'meta':
        S.runId = ev.runId || S.runId;
        setPhase('streaming');
        break;
      case 'delta':
        if (typeof ev.text === 'string' && ev.text) appendDelta(ev.text);
        break;
      case 'reasoning':
        if (typeof ev.text === 'string' && ev.text) appendReason(ev.text);
        break;
      case 'step':
        addNote('第 ' + (ev.index != null ? ev.index : '?') + ' 步：' + (ev.title || ''), null);
        break;
      case 'tool':
        // 结构化工件：ok 且带 artifact → 渲染确认卡（与历史 steps 同一函数）
        toolRow(ev);
        if (ev.status === 'ok' && ev.artifact && ev.artifact.kind) renderArtifact(msgsEl, ev.artifact, {});
        break;
      case 'progress':
        // 心跳 chars=-1 忽略；draft_chapter_body 生成中报真实字数
        if (ev.chars > 0 && lastDraftRow && lastDraftRow.__sum) lastDraftRow.__sum.textContent = '已生成 ' + ev.chars + ' 字…';
        break;
      case 'error': {
        const code = ev.code || '';
        if (code === 'RETRY') {
          setPhase('retrying');
          addNote('重试中：' + (ev.message || '同模型重试'), 'wxag__note--accent');
        } else if (code === 'CANCELLED') {
          addNote('已停止', null);
        } else {
          addNote((ERROR_LABEL[code] || '错误') + '：' + (ev.message || ''), 'wxag__note--fail');
        }
        break;
      }
      case 'interrupted':
        // 可重复出现；归并而非重复插卡
        if (!interruptedSeen) {
          interruptedSeen = true;
          addNote('已停止（残稿 ' + (ev.partialChars != null ? ev.partialChars : '?') + ' 字，未落盘）', null);
        }
        setPhase('interrupted');
        break;
      case 'done': {
        // done 只是运行结束：有工件按回执说话，无工件绝不显示「已保存」
        const status = ev.status || 'done';
        if (status === 'done') {
          addNote(artifactSeen ? (lastArtifactNote || '本轮操作完成（见上方回执）') : '回复完成', 'wxag__note--ok');
          setPhase('done');
        } else {
          addNote('运行结束：' + (TERMINAL_LABEL[status] || status), status === 'error' ? 'wxag__note--fail' : null);
          setPhase(status === 'interrupted' ? 'interrupted' : status === 'error' ? 'error' : status);
        }
        refreshStrip();
        break;
      }
      default:
        break;
    }
  }

  // ============ 发送 / 停止 ============
  async function send() {
    if (S.phase === 'submitting' || S.phase === 'streaming' || S.phase === 'retrying' || S.phase === 'cancelling') return;
    const msg = taEl ? taEl.value.trim() : '';
    if (!msg) return;
    if (!S.bookId || !S.sessionId) { addNote('缺少书或会话，无法发送', 'wxag__note--fail'); return; }
    const make = makeChannel(handleEvent);
    if (!make) { addNote('事件通道不可用（__TAURI_INTERNALS__ 缺少 transformCallback），已停止发送', 'wxag__note--fail'); return; }
    S.channelKind = make.kind;
    const requestId = (window.crypto && typeof window.crypto.randomUUID === 'function')
      ? window.crypto.randomUUID()
      : 'req-' + Date.now() + '-' + Math.random().toString(16).slice(2);
    S.requestId = requestId;
    S.stopPending = false;
    interruptedSeen = false;
    proposalSeen = false;
    artifactSeen = false;
    lastArtifactNote = '';
    curBubble = null; curText = ''; curReason = null;
    tools = new Map(); toolOrder = [];
    addMsg('user').textContent = msg;
    if (taEl) taEl.value = '';
    setPhase('submitting');
    try {
      const r = await ipc('agent_turn', {
        bookId: S.bookId, sessionId: S.sessionId, requestId: requestId, message: msg, onEvent: make.ch,
      });
      // 终态重放 / 丢连接：没有 done 事件时按回执兜底；ok:true 不保证原任务成功，要看 status
      if (S.phase === 'submitting' || S.phase === 'streaming' || S.phase === 'retrying' || S.phase === 'cancelling') {
        const status = (r && r.status) || (r && r.replayed ? 'unknown' : 'unknown');
        if (status === 'done') { addNote(artifactSeen ? (lastArtifactNote || '本轮操作完成（见上方回执）') : '回复完成', 'wxag__note--ok'); setPhase('done'); }
        else if (status === 'unknown') { addNote('未收到可靠终态（连接可能中断），状态待核对；未自动重发', 'wxag__note--fail'); setPhase('error'); }
        else { addNote('运行结束：' + (TERMINAL_LABEL[status] || status)); setPhase(status); }
      }
      refreshStrip();
      reconcile();
    } catch (e) {
      addNote('发送失败：' + errText(e), 'wxag__note--fail');
      setPhase('error');
    }
  }

  async function stop() {
    if (S.phase !== 'submitting' && S.phase !== 'streaming' && S.phase !== 'retrying') return;
    if (!S.requestId) { addNote('缺少运行标识，状态待核对（不猜测取消目标）', 'wxag__note--fail'); return; }
    S.stopPending = true;
    setPhase('cancelling');
    try {
      // 只带 requestId：绝不带 sessionId（按会话取消会毒化该会话下一条请求）
      await ipc('abort_chat', { requestId: S.requestId });
    } catch (e) {
      addNote('停止请求失败：' + errText(e), 'wxag__note--fail');
    }
    // 回 {ok:true} 只表示登记；保持「停止中…」直到流终态事件
  }

  // ============ 面板 ============
  const RESERVE_CLASS = '__wxag_reserve';
  let reserveTarget = null;
  function findAppRoot() {
    const cands = ['#root', '#app', '#__next', 'body > div', 'body > main'];
    const vw = window.innerWidth || 1200, vh = window.innerHeight || 800;
    for (const sel of cands) {
      let n = null;
      try { n = doc.querySelector(sel); } catch (e) { n = null; }
      if (!n || typeof n.getBoundingClientRect !== 'function') continue;
      let r = { width: 0, height: 0 };
      try { r = n.getBoundingClientRect() || r; } catch (e) {}
      if (r.width >= vw * 0.6 && r.height >= vh * 0.5) return n;
    }
    return null;
  }
  function reserveSpace(on) {
    try {
      if (on) {
        if (!reserveTarget) {
          const root = findAppRoot();
          if (root) {
            let pos = '';
            try { pos = (window.getComputedStyle && window.getComputedStyle(root).position) || ''; } catch (e) {}
            const prop = (pos === 'fixed' || pos === 'absolute') ? 'right' : 'marginRight';
            reserveTarget = { node: root, prop: prop, prev: root.style[prop] || '' };
            root.style[prop] = T.panelW + 'px';
          } else if (doc.documentElement && doc.documentElement.classList) {
            doc.documentElement.classList.add(RESERVE_CLASS);
            reserveTarget = { html: true };
          }
        }
      } else if (reserveTarget) {
        if (reserveTarget.html) {
          if (doc.documentElement && doc.documentElement.classList) doc.documentElement.classList.remove(RESERVE_CLASS);
        } else {
          try { reserveTarget.node.style[reserveTarget.prop] = reserveTarget.prev; } catch (e) {}
        }
        reserveTarget = null;
      }
    } catch (e) { reserveTarget = null; }
  }

  function buildPanel() {
    ensureStyle();
    panelEl = el('div');
    panelEl.id = '__wxag_panel';
    const head = el('div', 'wxag__hd');
    head.appendChild(el('span', 'wxag__title', '创作助手'));
    head.appendChild(el('span', 'wxag__mode', '手动线 · 逐步确认'));
    const ops = el('div', 'wxag__hdact');
    const clear = el('button', 'wxag__icon', '清屏');
    clear.title = '只清空当前视图，不影响已持久化的会话消息';
    clear.onclick = () => { if (msgsEl) msgsEl.replaceChildren(); };
    ops.appendChild(clear);
    const close = el('button', 'wxag__icon', '收起');
    close.onclick = () => closeAgentPanel();
    ops.appendChild(close);
    head.appendChild(ops);
    panelEl.appendChild(head);

    stripEl = el('div', 'wxag__strip');
    panelEl.appendChild(stripEl);

    msgsEl = el('div', 'wxag__msgs');
    panelEl.appendChild(msgsEl);

    quickEl = el('div', 'wxag__quick');
    panelEl.appendChild(quickEl);

    const ft = el('div', 'wxag__ft');
    taEl = el('textarea', 'wxag__ta');
    taEl.placeholder = '例：为第3章起草细纲 / 细纲可以，入库 / 按已确认细纲写正文 / 这个版本定稿';
    taEl.oninput = () => syncControls();
    taEl.onkeydown = (e) => {
      if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); if (!sendEl.disabled) send(); }
    };
    ft.appendChild(taEl);
    const acts = el('div', 'wxag__acts');
    sendEl = el('button', 'wxag__send', '发送');
    sendEl.disabled = true;
    sendEl.onclick = () => send();
    stopEl = el('button', 'wxag__stop', '停止');
    stopEl.disabled = true;
    stopEl.onclick = () => stop();
    acts.appendChild(sendEl);
    acts.appendChild(stopEl);
    ft.appendChild(acts);
    hintEl = el('div', 'wxag__hint');
    ft.appendChild(hintEl);
    panelEl.appendChild(ft);

    (doc.body || doc.documentElement).appendChild(panelEl);
  }

  // 打开面板：切换书/会话时重载历史（靠持久记录恢复，不靠内存猜测；失败如实提示不阻塞发送）。
  // 竞态防护：host 在 await 前捕获——迟到响应发现视图已被替换（__setEls/清屏/再切会话）就放弃渲染，
  // 绝不 replaceChildren 清掉别人正在用的实时内容。
  async function loadSession() {
    const key = S.bookId + ':' + S.sessionId;
    if (loadedFor === key) return;
    loadedFor = key;
    const host = msgsEl;
    if (host) host.replaceChildren();
    lastPipeline = null;
    let rows = null;
    try { rows = await ipc('list_messages', { bookId: S.bookId, sessionId: S.sessionId }); } catch (e) { rows = null; }
    if (loadedFor !== key || host !== msgsEl) return;
    if (Array.isArray(rows) && rows.length) {
      renderHistory(host, rows);
      addNote('历史已恢复（' + rows.length + ' 条）；卡片状态以对账结果为准', null);
    } else if (rows === null) {
      addNote('历史加载失败：状态待核对（不影响发送）', 'wxag__note--fail');
    }
    await refreshStrip();
    reconcile();
  }

  function openAgentPanel(bookId, sessionId) {
    ensureStyle();
    S.bookId = bookId || '';
    S.sessionId = sessionId || '';
    if (!S.bookId || !S.sessionId) return false;
    // 同一时刻只保留一个右侧面板，避免互相遮挡
    try { if (window.__WX_PIPELINE_UI_API__ && window.__WX_PIPELINE_UI_API__.collapse) window.__WX_PIPELINE_UI_API__.collapse(); } catch (e) {}
    if (!panelEl) buildPanel();
    panelEl.style.display = '';
    reserveSpace(true);
    syncControls();
    loadSession();
    try { if (taEl && !taEl.disabled) taEl.focus(); } catch (e) {}
    return true;
  }

  function closeAgentPanel() {
    if (panelEl) panelEl.style.display = 'none';
    reserveSpace(false);
  }

  function ensureEntry() {
    if (doc.getElementById('__wxag_entry')) return;
    if (!doc.body) return;
    ensureStyle();
    const b = el('button', null, '创作助手');
    b.id = '__wxag_entry';
    b.title = '创作助手（手动线）：对话推进建档/细纲/正文/定稿，每步写入由你确认';
    b.onclick = () => {
      const bid = typeof window.__molanCurrentBookId === 'function' ? window.__molanCurrentBookId() : '';
      const sid = typeof window.__molanCurrentSessionId === 'function' ? window.__molanCurrentSessionId() : '';
      if (!bid || !sid) { window.__WX_PIPELINE_UI_API__ && window.__WX_PIPELINE_UI_API__.toast('未找到当前书或会话：请先打开一个写作会话'); return; }
      openAgentPanel(bid, sid);
    };
    doc.body.appendChild(b);
  }

  const boot = () => { ensureEntry(); setInterval(ensureEntry, 2000); };
  if (doc.body) boot(); else doc.addEventListener('DOMContentLoaded', boot, { once: true });

  // ============ 导出（ui-check.cjs 结构断言用；只增不减） ============
  window.__molanAgentUI = {
    __ready: true,
    version: 'manual-v1',
    tokens: T,
    openAgentPanel: openAgentPanel,
    closeAgentPanel: closeAgentPanel,
    // 测试/联调钩子：不经过 DOM 直接喂事件 / 渲染工件 / 渲染历史
    __handleEvent: handleEvent,
    __renderArtifact: renderArtifact,
    renderHistory: renderHistory,
    __state: S,
    __makeChannel: makeChannel,
    __setEls: function (refs) {
      if (!refs) return;
      if (refs.msgs) msgsEl = refs.msgs;
      if (refs.ta) taEl = refs.ta;
      if (refs.send) sendEl = refs.send;
      if (refs.stop) stopEl = refs.stop;
      if (refs.hint) hintEl = refs.hint;
      if (refs.strip !== undefined) stripEl = refs.strip;
      if (refs.quick !== undefined) quickEl = refs.quick;
    },
    __send: send,
    __stop: stop,
  };
})();
