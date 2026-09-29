// 生产线 UI 桥 —— 手动线版（WAVE2 简约设计 + M2 手动线接线）。
//
// 职责（两件事，互不干扰）：
//  1) 上游限流条：观察生成流的 error 事件，code=UPSTREAM_LIMIT 时弹「换模型重试」条。
//     约束不变：只在用户显式点击后才写 agent_profile 设置，绝不静默改用户模型配置。
//  2) 生产线面板：状态由 get_pipeline_state 文件即真相推导；手动线动作走真实 IPC——
//     确认细纲入库=confirm_outline、起草正文=draft_chapter（流式，与 Agent 工具同一服务）、
//     记忆重跑=rebuild_memory；其余阶段仍走既有 store 显式通道。
//
// 设计语言（任务 A）：白底 / 1px 浅灰分隔线 #ececec / 无卡片堆叠阴影 / 圆角 8px /
// 系统字体栈 / 13px 正文 12px 辅助 / 行高 1.6 / 留白 14-16px；单强调色琥珀 #d97706
// 仅用于主按钮与待办标记；状态色收敛为 绿#16a34a(完成) 灰#9ca3af(未做) 琥珀(待处理) 红#dc2626(失败)。
//
// 依赖：glue 已定义 window.__TAURI_INTERNALS__，本脚本由服务器注入在 glue 之后
// （main.rs include_str!，见文件末尾 __WX_PIPELINE_UI_API__ 导出说明）。
//
// 容错约定（后端新字段可能尚未上线，全部按「字段不存在 = 走旧逻辑」渲染）：
//  - st.blockers?.length            → 待处理提示行（缺失则不显示）
//  - c.approvalVerified/bodySource  → 「未核验」标记（缺失则不加标记）
//  - c.memory                       → 记忆维度（缺失则视为未同步）
//  - c.outlineStatus                → 细纲确认维度（缺失则退回「有文件=绿」旧渲染）
(() => {
  if (window.__WX_PIPELINE_UI__) return;
  window.__WX_PIPELINE_UI__ = true;
  const internals = window.__TAURI_INTERNALS__;
  if (!internals || typeof internals.invoke !== 'function') return;
  const origInvoke = internals.invoke;
  const ipc = (cmd, args) => origInvoke.call(internals, cmd, args || {});
  const doc = document;

  // ============ 设计 tokens ============
  const T = {
    bg: '#ffffff',
    line: '#ececec',
    lineSoft: '#f6f6f6',
    text: '#1f2328',
    text2: '#6b7280',
    text3: '#9ca3af',
    accent: '#d97706',
    ok: '#16a34a',
    todo: '#9ca3af',
    pending: '#d97706',
    fail: '#dc2626',
    font: '-apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Hiragino Sans GB","Microsoft YaHei",system-ui,sans-serif',
    panelW: 360,
  };

  const ROLE_LABELS = { outline: '细纲', chapter: '正文', review: '审读', summary: '总结', distill: '蒸馏', chat: '聊天' };
  const roleLabel = (r) => ROLE_LABELS[r] || r || '生成';
  let lastMeta = null;

  const el = (tag, cls, text) => {
    const n = doc.createElement(tag);
    if (cls) n.className = cls;
    if (text != null) n.textContent = text;
    return n;
  };

  // ============ 样式（一次注入；类名前缀 wxpipe__，避免与宿主 CSS 冲突） ============
  const STYLE_ID = '__wxpipe_style';
  function ensureStyle() {
    if (doc.getElementById(STYLE_ID)) return;
    const st = el('style');
    st.id = STYLE_ID;
    st.textContent = [
      '#__wx_pipeline_panel{position:fixed;right:0;top:64px;bottom:20px;width:' + T.panelW + 'px;box-sizing:border-box;',
      'background:' + T.bg + ';border:1px solid ' + T.line + ';border-right:none;border-radius:8px 0 0 8px;',
      'font-family:' + T.font + ';font-size:13px;line-height:1.6;color:' + T.text + ';z-index:10001;',
      'display:flex;flex-direction:column;overflow:hidden}',
      '.wxpipe__hd{display:flex;align-items:center;gap:8px;padding:14px 14px 12px;border-bottom:1px solid ' + T.line + '}',
      '.wxpipe__title{font-size:13px;font-weight:600}',
      '.wxpipe__hdact{margin-left:auto;display:flex;gap:2px}',
      '.wxpipe__icon{border:none;background:transparent;color:' + T.text3 + ';font-family:inherit;font-size:12px;',
      'line-height:1.6;padding:3px 6px;border-radius:6px;cursor:pointer}',
      '.wxpipe__icon:hover{background:' + T.lineSoft + ';color:' + T.text + '}',
      '.wxpipe__body{flex:1;overflow-y:auto;padding:0 14px 16px}',
      '.wxpipe__agent{margin:12px 0 2px;width:100%;box-sizing:border-box;border:1px solid ' + T.accent + ';background:' + T.bg + ';',
      'color:' + T.accent + ';font-family:inherit;font-size:12px;line-height:1.6;padding:7px 10px;border-radius:8px;cursor:pointer}',
      '.wxpipe__agent:hover{background:#fffaf2}',
      '.wxpipe__agent[disabled]{border-color:' + T.line + ';color:' + T.text3 + ';cursor:not-allowed;background:' + T.bg + '}',
      '.wxpipe__sec{padding:14px 0 0;font-size:12px;color:' + T.text3 + ';letter-spacing:.02em}',
      '.wxpipe__row{display:flex;align-items:center;gap:8px;padding:6px 0;border-bottom:1px solid ' + T.lineSoft + '}',
      '.wxpipe__dot{width:7px;height:7px;border-radius:50%;flex:none;background:' + T.todo + '}',
      '.wxpipe__name{font-size:13px;color:' + T.text + '}',
      '.wxpipe__mks{margin-left:auto;display:flex;gap:8px;align-items:center;flex:none}',
      '.wxpipe__mk{display:inline-flex;align-items:center;gap:3px;font-size:11.5px;color:' + T.text3 + '}',
      '.wxpipe__mkdot{width:5px;height:5px;border-radius:50%;flex:none;display:inline-block}',
      '.wxpipe__unverified{font-size:11.5px;color:' + T.text3 + '}',
      '.wxpipe__more{display:block;width:100%;text-align:left;border:none;background:transparent;color:' + T.text3 + ';',
      'font-family:inherit;font-size:12px;line-height:1.6;padding:8px 0 0;cursor:pointer}',
      '.wxpipe__more:hover{color:' + T.accent + '}',
      '.wxpipe__count{padding:8px 0 0;font-size:11.5px;color:' + T.text3 + '}',
      '.wxpipe__blocker{padding:10px 0 0;font-size:12px;color:' + T.accent + '}',
      '.wxpipe__next{padding:12px 0 0}',
      '.wxpipe__primary{display:block;width:100%;box-sizing:border-box;border:1px solid ' + T.accent + ';background:' + T.accent + ';',
      'color:#fff;font-family:inherit;font-size:13px;line-height:1.6;padding:8px 12px;border-radius:8px;cursor:pointer}',
      '.wxpipe__primary:hover{background:#c2660a;border-color:#c2660a}',
      '.wxpipe__hint{padding:8px 0 0;font-size:11.5px;color:' + T.text3 + '}',
      '.wxpipe__hint--ok{color:' + T.ok + '}',
      '.wxpipe__aux{display:flex;flex-wrap:wrap;gap:12px;padding:8px 0 0}',
      '.wxpipe__link{border:none;background:transparent;color:' + T.text2 + ';font-family:inherit;font-size:12px;',
      'line-height:1.6;padding:0;cursor:pointer;text-decoration:underline;text-decoration-color:' + T.line + '}',
      '.wxpipe__link:hover{color:' + T.accent + '}',
      '.wxpipe__empty{padding:14px 0;font-size:12px;color:' + T.text3 + '}',
      '.wxpipe__err{padding:14px 0;font-size:12px;color:' + T.fail + '}',
      // 收起态：只是一个细条按钮
      '#__wx_pipeline_entry{position:fixed;right:0;top:40%;z-index:10002;border:1px solid ' + T.line + ';border-right:none;',
      'border-radius:8px 0 0 8px;background:' + T.bg + ';color:' + T.text3 + ';font-family:' + T.font + ';',
      'font-size:11px;line-height:1.6;padding:10px 4px;cursor:pointer;writing-mode:vertical-rl;letter-spacing:.08em}',
      '#__wx_pipeline_entry:hover{color:' + T.accent + ';border-color:' + T.accent + '}',
      // 顶部居中小条 toast（白底细边框，3 秒消失；独立 id __wxpipe_toast，绝不可复用 glue 的 __wx_toast——样式叠加会把旧 toast 拉伸成全屏大黑框）
      '#__wxpipe_toast{position:fixed;left:50%;top:16px;transform:translateX(-50%);z-index:10004;max-width:min(560px,86vw);',
      'background:' + T.bg + ';border:1px solid ' + T.line + ';border-radius:8px;padding:9px 14px;',
      'font-family:' + T.font + ';font-size:12px;line-height:1.6;color:' + T.text + '}',
      '#__wx_limit_bar{position:fixed;left:50%;top:16px;transform:translateX(-50%);z-index:10003;max-width:min(680px,92vw);',
      'display:flex;flex-wrap:wrap;gap:8px;align-items:center;padding:10px 14px;border:1px solid ' + T.accent + ';',
      'border-radius:8px;background:' + T.bg + ';font-family:' + T.font + ';font-size:12px;line-height:1.6;color:' + T.text + '}',
      '.wxpipe__lbtn{border:1px solid ' + T.line + ';border-radius:8px;background:' + T.bg + ';color:' + T.text + ';',
      'font-family:inherit;font-size:12px;line-height:1.6;padding:3px 9px;cursor:pointer}',
      '.wxpipe__lbtn:hover{border-color:' + T.accent + ';color:' + T.accent + '}',
      '.wxpipe__lbtn--ghost{border-style:dashed;color:' + T.text2 + '}',
      // 面板打开时给宿主内容让位（best-effort；找不到宿主根时退化为浮层）
      'html.__wxpipe_reserve{padding-right:' + T.panelW + 'px;overflow-x:hidden}',
    ].join('');
    (doc.head || doc.documentElement || doc.body).appendChild(st);
  }

  // ============ toast：顶部居中小条，3 秒消失 ============
  let toastTimer = null;
  function toast(msg) {
    ensureStyle();
    let t = doc.getElementById('__wxpipe_toast');
    if (!t) {
      t = el('div');
      t.id = '__wxpipe_toast';
      (doc.body || doc.documentElement).appendChild(t);
    }
    t.textContent = msg;
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { try { t.remove(); } catch (e) {} }, 3000);
  }

  // ============ 上游限流条（保留原行为，仅换用同一套 tokens） ============
  function showLimitBar(ev) {
    ensureStyle();
    const alts = Array.isArray(ev.alternates) ? ev.alternates.slice(0, 6) : [];
    const old = doc.getElementById('__wx_limit_bar');
    if (old) old.remove();
    const bar = el('div');
    bar.id = '__wx_limit_bar';
    const role = ev.role || (lastMeta && lastMeta.role) || '';
    const txt = el('span', null, '⚠ ' + (ev.message || '上游限流') + (alts.length && role ? '　「' + roleLabel(role) + '」换模型：' : ''));
    bar.appendChild(txt);
    if (role) {
      alts.forEach((m) => {
        const b = el('button', 'wxpipe__lbtn', m);
        b.onclick = async () => {
          b.disabled = true;
          try {
            const st = await ipc('get_settings', {});
            const key = 'agent_profile__' + role;
            let prof = {};
            try { prof = JSON.parse((st && st[key]) || '{}'); } catch (e) {}
            if (!prof || typeof prof !== 'object') prof = {};
            prof.model = m;
            await ipc('set_setting', { key: key, value: JSON.stringify(prof) });
            bar.remove();
            toast('「' + roleLabel(role) + '」模型已切换为 ' + m + '（仅改这一项分工，可随时在 设置→模型 改回）。请重新点击生成。');
          } catch (e) {
            b.disabled = false;
            toast('切换失败：' + (e && e.message ? e.message : String(e)));
          }
        };
        bar.appendChild(b);
        const fb = el('button', 'wxpipe__lbtn wxpipe__lbtn--ghost', '设为回退');
        fb.title = '把 ' + m + ' 设为「' + roleLabel(role) + '」角色的自动回退模型：主模型限流时自动改用它重试一次（显式开启，可随时在设置里清空）';
        fb.onclick = async () => {
          try {
            await ipc('set_setting', { key: 'agent_fallback__' + role, value: m });
            toast('已开启回退：「' + roleLabel(role) + '」主模型限流时自动改用 ' + m + ' 重试一次。请重新点击生成。');
            bar.remove();
          } catch (e2) { toast('设置回退失败：' + (e2 && e2.message ? e2.message : String(e2))); }
        };
        bar.appendChild(fb);
      });
    }
    const close = el('button', 'wxpipe__icon', '✕');
    close.onclick = () => bar.remove();
    bar.appendChild(close);
    (doc.body || doc.documentElement).appendChild(bar);
    setTimeout(() => { if (bar.isConnected) bar.remove(); }, 45000);
  }

  function observe(ev) {
    try {
      if (!ev || typeof ev !== 'object') return;
      if (ev.type === 'meta' && ev.role) lastMeta = ev;
      if (ev.type === 'error' && ev.code === 'UPSTREAM_LIMIT') showLimitBar(ev);
    } catch (e) {}
  }

  // 包一层 invoke：对生成类命令的 onEvent 通道做透明观察。
  // 关键：官方 Channel 靠 toJSON/id 走 glue 序列化协议，绝不能换成代理对象（会丢事件流）；
  // 只在原通道实例上包一层 onmessage（glue 分发时动态读取当前 onmessage，包装对协议零侵入）。
  const REFRESH_CMDS = ['approve_chapter', 'reject_chapter', 'save_doc', 'save_section', 'save_book_setup_selection', 'write_file', 'delete_file', 'restore_trash', 'create_file'];
  let inflight = 0;
  internals.invoke = function (cmd, args, opts) {
    try {
      if ((cmd === 'chat_stream' || cmd === 'inline_chat' || cmd === 'inline_assist') && args && args.onEvent && typeof args.onEvent.onmessage === 'function') {
        const ch = args.onEvent;
        const orig = ch.onmessage;
        ch.onmessage = (ev) => {
          observe(ev);
          try { orig.call(ch, ev); } catch (e) {}
        };
      }
    } catch (e) {}
    const ret = origInvoke.call(this, cmd, args, opts);
    try {
      if (cmd === 'chat_stream') {
        inflight++;
        Promise.resolve(ret).then(() => { inflight--; scheduleRefresh(); }, () => { inflight--; scheduleRefresh(); });
      } else if (REFRESH_CMDS.indexOf(cmd) >= 0) {
        Promise.resolve(ret).then(scheduleRefresh, () => {});
      }
    } catch (e) {}
    return ret;
  };

  // ============ 动作桥（手动线动作走真实 IPC；其余走既有 store 显式通道，不伪造写操作） ============
  const currentBook = () => (typeof window.__molanCurrentBookId === 'function' ? window.__molanCurrentBookId() : '');

  // Channel 构造（与 agent_ui 同协议）：原生 Channel 优先，退化 transformCallback shim。
  // 官方 Channel 靠 toJSON/id 走 glue 序列化协议，绝不换成代理对象。
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
        if (ch && ch.id != null && typeof ch.toJSON === 'function') return ch;
      } catch (e) {}
    }
    if (typeof internals.transformCallback !== 'function') return null;
    const id = internals.transformCallback(onMsg);
    return { id: id, toJSON() { return '__CHANNEL__:' + id; } };
  }

  // 起草正文：draft_chapter 流式 IPC（与 Agent 工具 draft_chapter_body 同一后端服务）。
  // 进度经事件通道如实展示；done 只说明草稿进待审，绝不显示「已定稿」。
  let drafting = false;
  async function draftBody(ch) {
    const bookId = currentBook();
    if (!bookId) { toast('未找到当前书'); return; }
    if (drafting) { toast('已有起草在进行——等它完成或到创作助手停止'); return; }
    const chan = makeChannel((ev) => {
      if (!ev || typeof ev !== 'object') return;
      if (ev.type === 'progress' && ev.chars > 0) toast('第' + ch + '章生成中… ' + ev.chars + ' 字');
      else if (ev.type === 'step') toast('第' + ch + '章：' + (ev.title || ''));
      else if (ev.type === 'done') toast('第' + ch + '章草稿已进「正文待审」，请审阅后定稿');
      else if (ev.type === 'error') toast('第' + ch + '章起草失败：' + (ev.message || ''));
    });
    if (!chan) { toast('事件通道不可用，无法起草'); return; }
    drafting = true;
    const requestId = (window.crypto && typeof window.crypto.randomUUID === 'function')
      ? window.crypto.randomUUID() : 'draft-' + Date.now() + '-' + Math.random().toString(16).slice(2);
    toast('第' + ch + '章开始起草（前置检查→生成→去味→落待审）…');
    try {
      await ipc('draft_chapter', { bookId: bookId, ch: ch, requestId: requestId, onEvent: chan });
    } catch (e) {
      toast('第' + ch + '章起草失败：' + (e && e.message ? e.message : String(e)));
    } finally {
      drafting = false;
      scheduleRefresh(0);
    }
  }

  function act(kind, ch) {
    if (inflight > 0) { toast('当前有生成在进行——等它完成或先停止，再走下一步'); return; }
    try {
      if (kind === 'outline') window.__molanRunChat({ text: '帮我出这本书的全书大纲：核心立意、主线三幕、主要人物与对抗关系、卷结构与前3章钩子。先给我方向选择，确认后再细化。', displayText: '生成全书大纲', skills: ['剧情推演'], files: [], clearComposer: false });
      else if (kind === 'setup') window.__molanRunChat({ text: '根据现有大纲与设定，整理建书档案：书名定档、类型定位、主角与人物表、世界观、力量体系、伏笔台账。', displayText: '整理建书档案', skills: ['设定共建'], files: [], clearComposer: false });
      else if (kind === 'chapter_outline') { if (window.__molanNextOutline) window.__molanNextOutline(); else window.__molanRunChat({ text: '帮我生成第' + ch + '章细纲。若还需对齐走向，只问一个最关键问题。', displayText: '生成第' + ch + '章细纲', skills: ['小说细纲生成'], files: [], clearComposer: false }); }
      // 手动线：确认入库=作者动作，直接走 confirm_outline IPC（绑定当前内容 hash，返回真实回执）
      else if (kind === 'outline_confirm') {
        ipc('confirm_outline', { bookId: currentBook(), ch: ch }).then((r) => {
          toast('第' + ch + '章细纲已确认入库（' + String((r && r.hash) || '').slice(0, 8) + '…），可以起草正文');
          scheduleRefresh(0);
        }, (e) => { toast('确认失败：' + (e && e.message ? e.message : String(e))); });
      }
      // 手动线：起草正文=draft_chapter 流式 IPC（不再借道聊天文案桥）
      else if (kind === 'chapter_body') draftBody(ch);
      else if (kind === 'review') { if (window.__wx_pend_reensure) window.__wx_pend_reensure(); toast('请在输入框上方的待审条里预览并接受第' + ch + '章'); }
      else if (kind === 'summary') window.__molanRunChat({ text: '请生成第' + ch + '章的承接摘要（供下一章细纲与正文衔接使用）：本章主要事件、人物关系变化、新埋设与已回收的伏笔、结尾钩子。300字以内，生成后我会保存到 参考/摘要_第' + ch + '章.md。', displayText: '生成第' + ch + '章摘要', skills: [], files: [], clearComposer: false });
      else if (kind === 'memory_fix') {
        // 记忆重跑有真实 IPC（rebuild_memory：逐章、幂等、失败如实写 memory_job）：直接执行，不再只指路。
        toast('正在重跑第' + ch + '章记忆同步…');
        ipc('rebuild_memory', { bookId: currentBook(), ch: ch }).then((r) => {
          toast('第' + ch + '章记忆重跑完成' + (r && r.note ? '：' + r.note : '') + '（失败原因可在记忆状态查看）');
          scheduleRefresh(0);
        }, (e) => { toast('记忆重跑失败：' + (e && e.message ? e.message : String(e))); });
      }
      // WAVE2：删除「按按钮文字找 DOM 并 click()」的批量桥（FRONTEND-AGENT-INTEGRATION §9 F4 要求）。
      // 批量链没有独立 IPC，凭文案点按钮既脆弱又不可验证；改为如实指路，不伪造成功。
      else if (kind === 'batch') toast('批量连写请在聊天页底部的写作链发起（含双重确认与逐章待审）');
    } catch (e) { toast('操作失败：' + (e && e.message ? e.message : String(e))); }
  }

  // ============ 状态 → 颜色/文案映射 ============
  const bodyColor = (b) => (b === 'approved' ? T.ok : b === 'pending' ? T.pending : T.todo);
  const bodyLabel = (b) => (b === 'approved' ? '已定稿' : b === 'pending' ? '待审' : '未写');
  const memColor = (m) => (m === 'valid' ? T.ok : (m === 'failed' ? T.fail : (m === 'pending' || m === 'stale' ? T.pending : T.todo)));
  const memLabel = (m) => (m === 'valid' ? '记忆已同步' : m === 'failed' ? '记忆同步失败' : m === 'pending' ? '记忆同步中' : m === 'stale' ? '记忆需复核' : '记忆未同步');

  function marker(label, color, title) {
    const s = el('span', 'wxpipe__mk');
    if (title) s.title = title;
    const d = el('i', 'wxpipe__mkdot');
    d.style.background = color;
    s.appendChild(d);
    s.appendChild(el('span', null, label));
    return s;
  }

  // 章节行的批准来源区分：仅在后端给出字段且明确为「文件来源 + 未核验」时加灰色小字。
  // 字段缺失（旧后端）→ 不加任何标记，走旧渲染。
  function needsUnverified(c) {
    return c && c.approvalVerified === false && c.bodySource === 'file';
  }

  // blockers 容错：后端新字段可能还没上线，形状也未定稿。
  // 实测后端（pipeline.rs derive_state_with_memory）形状为 {type:"memory", chapter, status}；
  // 这里同时兼容 string / {message} / {kind} 等其它可能形状，字段不认识时退化为通用文案，
  // 绝不因为形状变化就抛错或静默丢失提示。
  const MEMORY_BLOCK_LABEL = {
    failed: '记忆同步失败',
    stale: '记忆需复核',
    pending: '记忆同步中',
    missing: '记忆缺失',
  };
  function blockerText(b) {
    if (b == null) return '';
    if (typeof b === 'string') return b;
    if (typeof b.message === 'string' && b.message) return b.message;
    const n = b.chapter != null ? b.chapter : b.ch != null ? b.ch : b.n != null ? b.n : null;
    const type = b.type || b.kind || '';
    const status = b.status || '';
    let label = '';
    if (type === 'outline') label = status === 'stale' ? '细纲确认后又被修改，需重新确认' : '细纲待确认入库';
    else if (type === 'memory') label = MEMORY_BLOCK_LABEL[status] || '记忆待处理';
    else if (status === 'failed') label = '记忆同步失败';
    else if (status === 'stale') label = '记忆需复核';
    else if (status === 'pending') label = '记忆同步中';
    else label = '待处理';
    return (n != null ? '第' + n + '章' : '') + label;
  }

  const NEXT_LABELS = { outline: '去写全书大纲', setup: '整理建书档案', chapter_outline: '生成第', chapter_body: '写第', review: '去审核第' };
  function nextLabel(nx) {
    const s = nx && nx.stage;
    if (!s) return '';
    if (s === 'chapter_outline') return '生成第' + nx.chapter + '章细纲';
    // 手动线新增：细纲已存在但未确认/已失效 → 先确认入库再写正文
    if (s === 'outline_confirm') return '确认第' + nx.chapter + '章细纲入库';
    if (s === 'chapter_body') return '按已确认细纲起草第' + nx.chapter + '章正文';
    if (s === 'review') return '去审核第' + nx.chapter + '章';
    // F4 后端新增：记忆未同步的已批准章阻塞后续推进。rebuild_memory 有真实 IPC，直接重跑。
    if (s === 'memory_fix') return '重跑第' + nx.chapter + '章记忆同步';
    return NEXT_LABELS[s] || '';
  }

  // ============ 面板渲染（纯函数：只依赖传入的 st，便于离线自检） ============
  let expanded = false;
  const COLLAPSE_AT = 10;

  function renderState(body, st, bookId) {
    body.replaceChildren();
    if (!bookId) { body.appendChild(el('div', 'wxpipe__empty', '尚未选择作品')); return; }
    if (!st) { body.appendChild(el('div', 'wxpipe__err', '状态读取失败，可重试')); return; }

    // 顶部入口：打开创作助手
    const agentBtn = el('button', 'wxpipe__agent', '打开创作助手');
    const hasSession = typeof window.__molanCurrentSessionId === 'function' && !!window.__molanCurrentSessionId();
    if (!hasSession) {
      agentBtn.disabled = true;
      agentBtn.title = '未找到当前会话：请先在本书打开一个写作会话';
    }
    agentBtn.onclick = () => {
      const api = window.__molanAgentUI;
      if (!api || typeof api.openAgentPanel !== 'function') { toast('创作助手模块未注入（agent_ui.js 需要在 main.rs 注册 include_str!）'); return; }
      api.openAgentPanel(bookId, typeof window.__molanCurrentSessionId === 'function' ? window.__molanCurrentSessionId() : '');
    };
    body.appendChild(agentBtn);

    // 主流程两步
    const sec1 = el('div', 'wxpipe__sec', '准备');
    body.appendChild(sec1);
    const stepRow = (label, ok) => {
      const r = el('div', 'wxpipe__row');
      const d = el('i', 'wxpipe__dot');
      d.style.background = ok ? T.ok : T.todo;
      r.appendChild(d);
      r.appendChild(el('span', 'wxpipe__name', label));
      body.appendChild(r);
    };
    stepRow('全书大纲', !!st.hasOutline);
    stepRow('建书档案', !!st.hasSetup);

    // 章节列表：每章一行，圆点状态 + 章号 + 纲/文/忆 三个小标记
    const chs = Array.isArray(st.chapters) ? st.chapters : [];
    body.appendChild(el('div', 'wxpipe__sec', '章节'));
    const shown = expanded ? chs : chs.slice(0, COLLAPSE_AT);
    shown.forEach((c) => {
      const r = el('div', 'wxpipe__row');
      r.setAttribute('data-wxpipe-chapter', String(c && c.n));
      const mem = c && c.memory;
      const dotColor = mem === 'failed' ? T.fail : bodyColor(c && c.body);
      const d = el('i', 'wxpipe__dot');
      d.style.background = dotColor;
      r.appendChild(d);
      r.appendChild(el('span', 'wxpipe__name', '第' + (c && c.n) + '章'));
      if (needsUnverified(c)) {
        r.setAttribute('data-wxpipe-unverified', '1');
        r.appendChild(el('span', 'wxpipe__unverified', '未核验'));
      }
      const mks = el('div', 'wxpipe__mks');
      // 细纲维度（手动线）：outlineStatus 存在时按确认状态渲染；字段缺失退回旧「有文件=绿」。
      const os = c && c.outlineStatus;
      const olColor = os === 'confirmed' ? T.ok : os === 'stale' ? T.fail : os === 'saved' ? T.pending : (c && c.outline ? T.ok : T.todo);
      const olTitle = os === 'confirmed' ? '细纲已确认入库' : os === 'stale' ? '细纲确认后又被修改，原确认失效' : os === 'saved' ? '细纲已保存，待确认入库' : (c && c.outline ? '细纲已保存' : '细纲未保存');
      if (os === 'stale') r.setAttribute('data-wxpipe-outline-stale', '1');
      mks.appendChild(marker('纲', olColor, olTitle));
      mks.appendChild(marker('文', bodyColor(c && c.body), bodyLabel(c && c.body)));
      mks.appendChild(marker('忆', memColor(mem), memLabel(mem)));
      r.appendChild(mks);
      body.appendChild(r);
    });
    if (chs.length > COLLAPSE_AT && !expanded) {
      const more = el('button', 'wxpipe__more', '展开全部（共 ' + chs.length + ' 章）');
      more.onclick = () => { expanded = true; scheduleRefresh(0); };
      body.appendChild(more);
    }
    const counts = st.counts || {};
    body.appendChild(el('div', 'wxpipe__count', '共 ' + (counts.total != null ? counts.total : chs.length) + ' 章 · 定稿 ' + (counts.approved || 0) + ' · 待审 ' + (counts.pending || 0)));
    // 后端 F2：正式文件存在但批准凭证未核对时，counts.approved 不计入，另给 unverifiedFormal。
    if (counts.unverifiedFormal) {
      body.appendChild(el('div', 'wxpipe__count', '另有 ' + counts.unverifiedFormal + ' 章正式文件存在但批准来源未核验'));
    }
    if (st.truncated) body.appendChild(el('div', 'wxpipe__count', '目录扫描已截断（最大章号 ' + (st.maxChapterSeen != null ? st.maxChapterSeen : '?') + '），状态可能不完整'));

    // 下一步：blockers 提示行 + 一个主按钮 + 一行说明
    const nx = st.next || {};
    const wrap = el('div', 'wxpipe__next');
    const blockers = Array.isArray(st.blockers) ? st.blockers : [];
    if (blockers.length) {
      const first = blockerText(blockers[0]);
      wrap.appendChild(el('div', 'wxpipe__blocker', '需先处理：' + first + (blockers.length > 1 ? '（共 ' + blockers.length + ' 项）' : '')));
    }
    const label = nextLabel(nx);
    if (label) {
      const b = el('button', 'wxpipe__primary', label);
      b.onclick = () => act(nx.stage, nx.chapter);
      wrap.appendChild(b);
      // 后端 F4：next 指向同一章或更早时只打 blocked 标记（stage 不变），此处如实说明。
      if (nx.blocked === true) wrap.appendChild(el('div', 'wxpipe__hint', '该步骤当前被记忆阻塞，完成后才能继续'));
    } else {
      wrap.appendChild(el('div', 'wxpipe__hint wxpipe__hint--ok', '本章流程完整，可继续下一章'));
    }
    const ctxs = Array.isArray(st.nextContext) ? st.nextContext : [];
    const hint = label
      ? (ctxs.length ? '由文件状态推导 · 将携带 ' + ctxs.length + ' 项上下文：' + ctxs.map((c) => (c && c.label) || '').filter(Boolean).join('、') : '由文件状态推导 · 暂无可用上下文')
      : '由文件状态推导';
    wrap.appendChild(el('div', 'wxpipe__hint', hint));

    // 副操作（纯文字链接，不与主按钮争强调色）
    const aux = el('div', 'wxpipe__aux');
    if (st.summaryDue != null) {
      const a = el('button', 'wxpipe__link', '补第' + st.summaryDue + '章摘要');
      a.onclick = () => act('summary', st.summaryDue);
      aux.appendChild(a);
    }
    if (chs.length) {
      const a = el('button', 'wxpipe__link', '批量连写…');
      a.onclick = () => act('batch', null);
      aux.appendChild(a);
    }
    if (aux.childElementCount) wrap.appendChild(aux);
    body.appendChild(wrap);
  }

  // ============ 面板容器 / 让位 / 刷新调度 ============
  let panelEl = null, panelOpen = false, refreshTimer = null, lastBook = '';

  function scheduleRefresh(delay) {
    if (!panelOpen) return;
    clearTimeout(refreshTimer);
    refreshTimer = setTimeout(renderPanel, delay == null ? 500 : delay);
  }

  async function renderPanel() {
    if (!panelEl || !panelOpen) return;
    const body = panelEl.querySelector('.wxpipe__body');
    if (!body) return;
    const bookId = currentBook();
    if (!bookId) { renderState(body, null, ''); return; }
    let st = null;
    try { st = await ipc('get_pipeline_state', { bookId }); } catch (e) {}
    if (!panelOpen) return;
    if (currentBook() !== bookId) return; // 迟到响应不覆盖新书状态
    renderState(body, st, bookId);
  }

  // 面板固定在右侧；尽量让宿主内容让位，避免遮挡编辑器。
  // best-effort：先找宿主根元素（占视口 60%+ 宽、50%+ 高）；fixed/absolute 的根改 right 偏移，
  // 普通流根改 margin-right；都找不到时退回给 html 加 padding-right。
  const RESERVE_CLASS = '__wxpipe_reserve';
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
    panelEl.id = '__wx_pipeline_panel';
    const head = el('div', 'wxpipe__hd');
    head.appendChild(el('span', 'wxpipe__title', '生产线'));
    const ops = el('div', 'wxpipe__hdact');
    const refresh = el('button', 'wxpipe__icon', '刷新');
    refresh.onclick = () => renderPanel();
    ops.appendChild(refresh);
    const close = el('button', 'wxpipe__icon', '收起');
    close.onclick = () => togglePanel(false);
    ops.appendChild(close);
    head.appendChild(ops);
    panelEl.appendChild(head);
    const body = el('div', 'wxpipe__body');
    panelEl.appendChild(body);
    (doc.body || doc.documentElement).appendChild(panelEl);
  }

  function togglePanel(force) {
    const want = force == null ? !panelOpen : !!force;
    if (want) {
      if (!panelEl) buildPanel();
      panelEl.style.display = '';
      panelOpen = true;
      reserveSpace(true);
      renderPanel();
    } else {
      if (panelEl) panelEl.style.display = 'none';
      panelOpen = false;
      reserveSpace(false);
    }
  }

  function ensureEntry() {
    if (doc.getElementById('__wx_pipeline_entry')) return;
    if (!doc.body) return;
    ensureStyle();
    const b = el('button', null, '生产线');
    b.id = '__wx_pipeline_entry';
    b.title = '生产线：查看本书走到哪一步，并按流程继续';
    b.onclick = () => togglePanel(true);
    doc.body.appendChild(b);
  }

  // 供 agent 侧栏抢占右侧空间（同一时刻只保留一个右侧面板）
  function collapse() { if (panelOpen) togglePanel(false); }

  const boot = () => {
    ensureEntry();
    setInterval(() => {
      ensureEntry();
      const b = currentBook();
      if (b !== lastBook) { lastBook = b; expanded = false; scheduleRefresh(0); }
    }, 2000);
  };
  if (doc.body) boot(); else doc.addEventListener('DOMContentLoaded', boot, { once: true });

  // 自检/联调导出（ui-check.cjs 用它做结构断言；不改变任何面板行为）
  window.__WX_PIPELINE_UI_API__ = {
    version: 'manual-v1',
    tokens: T,
    renderState: renderState,
    renderPanel: renderPanel,
    togglePanel: togglePanel,
    collapse: collapse,
    blockerText: blockerText,
    needsUnverified: needsUnverified,
    nextLabel: nextLabel,
    toast: toast,
    act: act,
    makeChannel: makeChannel,
    __isDrafting: () => drafting,
    __panel: () => panelEl,
  };
})();
