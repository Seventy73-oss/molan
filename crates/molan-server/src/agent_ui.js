// 创作助手运行视图（WAVE2 任务 B）——独立模块，挂在 window.__molanAgentUI。
//
// 注入方式（本文件不改 .rs，注册由 Lead 负责）：
//   在 crates/molan-server/src/main.rs 的 include_str! 注入里追加一行：
//     "<script>window.__WX_BUILD__=\"{}\";{}</script><script>{}</script>",
//     build_ts,
//     include_str!("glue.js"),
//     include_str!("pipeline_ui.js"),
//     include_str!("agent_ui.js")        // ← 新增
//   必须排在 glue.js 之后（依赖 window.__TAURI_INTERNALS__）。
//   修改注入脚本后需 `cargo build -p molan-server` 才生效（cargo test 不更新 exe）。
//
// 事件契约（来自 AUDIT-BACKEND.md §5.2 实测 + FRONTEND-AGENT-INTEGRATION.md §3/4）：
//   POST /ipc/agent_turn { bookId, sessionId, requestId, message, onEvent:"__CHANNEL__:<id>" }
//   NDJSON 每行 {"ch":"<id>","e":{...}}；e.type ∈ meta|delta|reasoning|step|tool|error|interrupted|done|progress
//   心跳 ch="__hb__", e.type="progress", chars=-1 → 必须忽略。
//
// 硬约束（不得违反）：
//  1) 停止只发 abort_chat({requestId})，绝不带 sessionId（后端 F1：空闲按会话取消会遗留取消标记，
//     使该会话下一条请求立即 interrupted 且零输出）。
//  2) done.status==="done" 只表示「运行结束」，Agent 不落盘任何书稿（F7）→ 只显示「回复完成」，
//     绝不显示「已保存 / 已写入」。
//  3) 不用 innerHTML 拼接模型文本，一律 textContent。
//  4) 同会话发送期间禁用输入；不做并发重发（F6：同 requestId 并发 = 同一 run 被真跑两遍）。
(() => {
  if (window.__molanAgentUI && window.__molanAgentUI.__ready) return;
  const internals = window.__TAURI_INTERNALS__;
  if (!internals || typeof internals.invoke !== 'function') return;
  const origInvoke = internals.invoke;
  const ipc = (cmd, args) => origInvoke.call(internals, cmd, args || {});
  const doc = document;

  // ============ 与 pipeline_ui.js 同一套 tokens ============
  const T = {
    bg: '#ffffff', line: '#ececec', lineSoft: '#f6f6f6',
    text: '#1f2328', text2: '#6b7280', text3: '#9ca3af',
    accent: '#d97706', ok: '#16a34a', todo: '#9ca3af', pending: '#d97706', fail: '#dc2626',
    font: '-apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Hiragino Sans GB","Microsoft YaHei",system-ui,sans-serif',
    panelW: 300,
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

  // ============ 样式（一次注入） ============
  const STYLE_ID = '__wxagent_style';
  function ensureStyle() {
    if (doc.getElementById(STYLE_ID)) return;
    const st = el('style');
    st.id = STYLE_ID;
    st.textContent = [
      '#__wx_agent_panel{position:fixed;right:0;top:64px;bottom:20px;width:' + T.panelW + 'px;box-sizing:border-box;',
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
      '.wxag__msgs{flex:1;overflow-y:auto;padding:14px}',
      '.wxag__msg{padding:0 0 14px}',
      '.wxag__role{font-size:11.5px;color:' + T.text3 + ';padding:0 0 4px}',
      '.wxag__text{white-space:pre-wrap;word-break:break-word}',
      '.wxag__text--user{color:' + T.text + '}',
      '.wxag__reason{margin:2px 0 8px;padding:6px 9px;border-left:2px solid ' + T.line + ';font-size:11.5px;',
      'color:' + T.text3 + ';white-space:pre-wrap;word-break:break-word;cursor:pointer}',
      '.wxag__tool{display:flex;align-items:flex-start;gap:6px;padding:3px 0;font-size:12px;color:' + T.text2 + '}',
      '.wxag__toolname{color:' + T.text + ';flex:none}',
      '.wxag__toolsum{color:' + T.text3 + ';word-break:break-word}',
      '.wxag__mark{flex:none;width:12px;text-align:center}',
      '.wxag__mark--run{color:' + T.accent + '}',
      '.wxag__mark--ok{color:' + T.ok + '}',
      '.wxag__mark--err{color:' + T.fail + '}',
      '.wxag__step{font-size:11.5px;color:' + T.text3 + ';padding:2px 0}',
      '.wxag__note{font-size:11.5px;color:' + T.text3 + ';padding:2px 0}',
      '.wxag__note--accent{color:' + T.accent + '}',
      '.wxag__note--ok{color:' + T.ok + '}',
      '.wxag__note--fail{color:' + T.fail + '}',
      '.wxag__props{margin:4px 0 0;padding:6px 9px;border:1px solid ' + T.accent + ';border-radius:8px;',
      'font-size:11.5px;color:' + T.accent + '}',
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
      '#__wx_agent_entry{position:fixed;right:0;top:52%;z-index:10006;border:1px solid ' + T.line + ';border-right:none;',
      'border-radius:8px 0 0 8px;background:' + T.bg + ';color:' + T.text3 + ';font-family:' + T.font + ';',
      'font-size:11px;line-height:1.6;padding:10px 4px;cursor:pointer;writing-mode:vertical-rl;letter-spacing:.08em}',
      '#__wx_agent_entry:hover{color:' + T.accent + ';border-color:' + T.accent + '}',
      'html.__wxagent_reserve{padding-right:' + T.panelW + 'px;overflow-x:hidden}',
    ].join('');
    (doc.head || doc.documentElement || doc.body).appendChild(st);
  }

  // ============ Channel 构造 ============
  // 官方 Tauri v2 协议（与 glue.js 的 clone/parseLine 完全一致）：
  //   Channel.toJSON() → "__CHANNEL__:<id>"；服务端 channel_id() 剥掉前缀得到 "<id>"，
  //   事件以 {"ch":"<id>","e":{…}} 回来；glue.parseLine 对该 id 调 runCallbackId(id,{index,message})。
  // 优先复用宿主已存在的 Channel 构造（window.__TAURI__.core.Channel / window.Channel）；
  // 找不到时用 internals.transformCallback 自建协议等价对象 —— 走的是同一条
  // transformCallback → __CHANNEL__:<id> → runCallback 路径，不是另起一套私有协议。
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
  // 刷新恢复：当前后端没有 run 查询 IPC（get_run/latest_run_for_session/list_turns 只是 core 函数，
  // AUDIT-BACKEND §5.4.4），刷新后无法查「运行中」，也无法续看中途草稿。这里明确不假装能恢复：
  // 面板重开时只显示一行「历史运行无法续看」，不自动重发、不把旧文本当成本次结果。
  const S = {
    bookId: '', sessionId: '',
    phase: 'idle',        // idle|submitting|streaming|retrying|cancelling|done|interrupted|error
    requestId: '', runId: '',
    channelKind: '',
    stopPending: false,
  };

  let panelEl = null, msgsEl = null, taEl = null, sendEl = null, stopEl = null, hintEl = null;
  let curBubble = null;      // 当前 assistant 气泡的文本节点宿主
  let curText = '';
  let curReason = null;
  let tools = new Map();     // callId -> rowEl
  let toolOrder = [];
  let proposalSeen = false;
  let interruptedSeen = false;

  const ROLE = { user: '你', assistant: '创作助手' };

  function scrollDown() { try { if (msgsEl) msgsEl.scrollTop = msgsEl.scrollHeight; } catch (e) {} }

  function addMsg(role) {
    const wrap = el('div', 'wxag__msg');
    wrap.appendChild(el('div', 'wxag__role', ROLE[role] || role));
    const body = el('div', 'wxag__text' + (role === 'user' ? ' wxag__text--user' : ''));
    wrap.appendChild(body);
    msgsEl.appendChild(wrap);
    scrollDown();
    return body;
  }

  function addNote(text, cls) {
    const n = el('div', 'wxag__note' + (cls ? ' ' + cls : ''), text);
    msgsEl.appendChild(n);
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
      msgsEl.appendChild(curReason);
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
      row.__name = el('span', 'wxag__toolname', ev.name || 'tool');
      row.__sum = el('span', 'wxag__toolsum', '');
      row.appendChild(row.__mark);
      row.appendChild(row.__name);
      row.appendChild(row.__sum);
      msgsEl.appendChild(row);
      if (callId) { tools.set(callId, row); toolOrder.push(callId); }
      else { toolOrder.push(row); }
    }
    const status = ev.status || '';
    if (status === 'running') {
      row.__mark.className = 'wxag__mark wxag__mark--run';
      row.__mark.textContent = '◌';
    } else if (status === 'ok') {
      row.__mark.className = 'wxag__mark wxag__mark--ok';
      row.__mark.textContent = '✓';
    } else if (status === 'error') {
      row.__mark.className = 'wxag__mark wxag__mark--err';
      row.__mark.textContent = '✗';
    }
    if (typeof ev.summary === 'string' && ev.summary) row.__sum.textContent = truncate(ev.summary, 160);
    if (ev.name === 'create_change_proposal' && !proposalSeen) {
      proposalSeen = true;
      const p = el('div', 'wxag__props', '提案待你在 DeepWrite 接受（pending 只表示待处理，接受后才生效）');
      msgsEl.appendChild(p);
    }
    scrollDown();
  }

  const TERMINAL_LABEL = {
    interrupted: '已停止',
    budget_exhausted: '预算耗尽',
    tools_unsupported: '渠道不支持工具',
    error: '失败',
  };
  const ERROR_LABEL = {
    BUDGET_EXHAUSTED: '预算耗尽',
    ROUNDS_EXHAUSTED: '轮次耗尽',
    TOOLS_UNSUPPORTED: '渠道不支持工具',
    CANCELLED: '已停止',
  };

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
    if (hintEl) {
      hintEl.textContent = S.phase === 'idle' ? 'Agent 只做分析、规划与提案；不写书稿、不落盘。'
        : S.phase === 'submitting' ? '已发送，等待运行建立…'
          : S.phase === 'streaming' ? '运行中（可停止）'
            : S.phase === 'retrying' ? '上游瞬态错误，同模型重试中…'
              : S.phase === 'cancelling' ? '已请求停止，等待终态…'
                : S.phase === 'done' ? '回复完成（Agent 不落盘）'
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
        toolRow(ev);
        break;
      case 'progress':
        // 心跳（chars=-1）与真实进度都不进正文
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
        // done 只是结束通知，status 未必成功；full 不再次追加（跨轮累积值，不保证等于持久正文）
        const status = ev.status || 'done';
        if (status === 'done') {
          addNote('回复完成', 'wxag__note--ok');
          setPhase('done');
        } else {
          addNote('运行结束：' + (TERMINAL_LABEL[status] || status), status === 'error' ? 'wxag__note--fail' : null);
          setPhase(status === 'interrupted' ? 'interrupted' : status === 'error' ? 'error' : status);
        }
        break;
      }
      default:
        break;
    }
  }

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
        if (status === 'done') { addNote('回复完成', 'wxag__note--ok'); setPhase('done'); }
        else if (status === 'unknown') { addNote('未收到可靠终态（连接可能中断），状态待核对；未自动重发', 'wxag__note--fail'); setPhase('error'); }
        else { addNote('运行结束：' + (TERMINAL_LABEL[status] || status)); setPhase(status); }
      }
    } catch (e) {
      addNote('发送失败：' + (e && e.message ? e.message : String(e)), 'wxag__note--fail');
      setPhase('error');
    }
  }

  async function stop() {
    if (S.phase !== 'submitting' && S.phase !== 'streaming' && S.phase !== 'retrying') return;
    if (!S.requestId) { addNote('缺少运行标识，状态待核对（不猜测取消目标）', 'wxag__note--fail'); return; }
    S.stopPending = true;
    setPhase('cancelling');
    try {
      // 只带 requestId：绝不带 sessionId（后端 F1 未修，按会话取消会毒化该会话下一条请求）
      await ipc('abort_chat', { requestId: S.requestId });
    } catch (e) {
      addNote('停止请求失败：' + (e && e.message ? e.message : String(e)), 'wxag__note--fail');
    }
    // 回 {ok:true} 只表示登记；保持「停止中…」直到流终态事件
  }

  // ============ 面板 ============
  const RESERVE_CLASS = '__wxagent_reserve';
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
    panelEl.id = '__wx_agent_panel';
    const head = el('div', 'wxag__hd');
    head.appendChild(el('span', 'wxag__title', '创作助手'));
    head.appendChild(el('span', 'wxag__mode', 'Agent 分析/提案'));
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

    msgsEl = el('div', 'wxag__msgs');
    panelEl.appendChild(msgsEl);

    const ft = el('div', 'wxag__ft');
    taEl = el('textarea', 'wxag__ta');
    taEl.placeholder = '问现状、要规划、让 Agent 出提案…';
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
    // 刷新恢复说明（无 run 查询 IPC，不假装能续看）
    addNote('刷新后无法续看运行中的步骤（后端暂无运行查询接口）；已完成的消息请到会话里查看。');
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
    if (hintEl && S.phase === 'idle') hintEl.textContent = 'Agent 只做分析、规划与提案；不写书稿、不落盘。';
    syncControls();
    try { if (taEl && !taEl.disabled) taEl.focus(); } catch (e) {}
    return true;
  }

  function closeAgentPanel() {
    if (panelEl) panelEl.style.display = 'none';
    reserveSpace(false);
  }

  function ensureEntry() {
    if (doc.getElementById('__wx_agent_entry')) return;
    if (!doc.body) return;
    ensureStyle();
    const b = el('button', null, '创作助手');
    b.id = '__wx_agent_entry';
    b.title = '创作助手：Agent 分析、规划与提案（不写书稿）';
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

  // ============ 导出（ui-check.cjs 结构断言用） ============
  window.__molanAgentUI = {
    __ready: true,
    version: 'wave2',
    tokens: T,
    openAgentPanel: openAgentPanel,
    closeAgentPanel: closeAgentPanel,
    // 测试/联调用：不经过 DOM 直接喂事件
    __handleEvent: handleEvent,
    __state: S,
    __makeChannel: makeChannel,
    __setEls: function (refs) {
      if (!refs) return;
      if (refs.msgs) msgsEl = refs.msgs;
      if (refs.ta) taEl = refs.ta;
      if (refs.send) sendEl = refs.send;
      if (refs.stop) stopEl = refs.stop;
      if (refs.hint) hintEl = refs.hint;
    },
    __send: send,
    __stop: stop,
  };
})();
