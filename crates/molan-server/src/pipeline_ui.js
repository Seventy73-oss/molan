// 生产线 UI 桥（AGENT-PIPELINE-PLAN P0）：观察生成流的 error 事件；
// code=UPSTREAM_LIMIT 时弹「换模型重试」条，一键把该写作角色切到同渠道备选模型。
// 约束：只在用户显式点击后才写 agent_profile 设置（绝不静默改用户模型配置）；
// 依赖 glue 已定义 window.__TAURI_INTERNALS__，本脚本由服务器注入在 glue 之后。
(() => {
  if (window.__WX_PIPELINE_UI__) return;
  window.__WX_PIPELINE_UI__ = true;
  const internals = window.__TAURI_INTERNALS__;
  if (!internals || typeof internals.invoke !== 'function') return;
  const origInvoke = internals.invoke;
  const ipc = (cmd, args) => origInvoke.call(internals, cmd, args || {});
  let lastMeta = null;
  const ROLE_LABELS = { outline: '细纲', chapter: '正文', review: '审读', summary: '总结', distill: '蒸馏', chat: '聊天' };
  const roleLabel = (r) => ROLE_LABELS[r] || r || '生成';

  function toast(msg) {
    const t = document.createElement('div');
    t.textContent = msg;
    t.style.cssText = 'position:fixed;left:50%;transform:translateX(-50%);bottom:64px;z-index:10003;padding:9px 16px;border-radius:10px;background:rgba(28,28,28,.93);color:#fff;font-size:13px;max-width:86vw;box-shadow:0 6px 24px rgba(0,0,0,.25)';
    document.body.appendChild(t);
    setTimeout(() => t.remove(), 6500);
  }

  function showLimitBar(ev) {
    const alts = Array.isArray(ev.alternates) ? ev.alternates.slice(0, 6) : [];
    const old = document.getElementById('__wx_limit_bar');
    if (old) old.remove();
    const bar = document.createElement('div');
    bar.id = '__wx_limit_bar';
    bar.style.cssText = 'position:fixed;left:50%;transform:translateX(-50%);bottom:104px;z-index:10002;max-width:min(720px,94vw);display:flex;flex-wrap:wrap;gap:8px;align-items:center;padding:10px 14px;border:1px solid #eecf7a;border-radius:12px;background:#fffbe9;box-shadow:0 10px 32px rgba(0,0,0,.2);font-size:13px;color:#5b4a12';
    const role = ev.role || (lastMeta && lastMeta.role) || '';
    const txt = document.createElement('span');
    txt.textContent = '⚠ ' + (ev.message || '上游限流') + (alts.length && role ? '　「' + roleLabel(role) + '」换模型：' : '');
    bar.appendChild(txt);
    if (role) {
      alts.forEach((m) => {
        const b = document.createElement('button');
        b.textContent = m;
        b.style.cssText = 'border:1px solid #d9b64e;border-radius:8px;background:#fff;color:#7a5c00;padding:4px 10px;cursor:pointer;font-size:12px';
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
        const fb = document.createElement('button');
        fb.textContent = '设为回退';
        fb.title = '把 ' + m + ' 设为「' + roleLabel(role) + '」角色的自动回退模型：主模型限流时自动改用它重试一次（显式开启，可随时在设置里清空）';
        fb.style.cssText = 'border:1px dashed #d9b64e;border-radius:8px;background:transparent;color:#a08a3e;padding:4px 8px;cursor:pointer;font-size:11px';
        fb.onclick = async () => {
          try {
            const st2 = await ipc('get_settings', {});
            let prof = {};
            try { prof = JSON.parse((st2 && st2['agent_profile__' + role]) || '{}'); } catch (e2) {}
            await ipc('set_setting', { key: 'agent_fallback__' + role, value: m });
            toast('已开启回退：「' + roleLabel(role) + '」主模型（' + ((prof && prof.model) || '当前模型') + '）限流时自动改用 ' + m + ' 重试一次。请重新点击生成。');
            bar.remove();
          } catch (e2) { toast('设置回退失败：' + (e2 && e2.message ? e2.message : String(e2))); }
        };
        bar.appendChild(fb);
      });
    }
    const close = document.createElement('button');
    close.textContent = '✕';
    close.style.cssText = 'border:none;background:transparent;cursor:pointer;color:#9a8a4f;font-size:14px;margin-left:2px';
    close.onclick = () => bar.remove();
    bar.appendChild(close);
    document.body.appendChild(bar);
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
  // 只在原通道实例上包一层 onmessage（gn 分发时动态读取当前 onmessage，包装对协议零侵入）。
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

  // ============ P1：生产线面板（状态由 get_pipeline_state 文件即真相推导；动作全部走既有 store 显式通道） ============
  let panelEl = null, panelOpen = false, refreshTimer = null, lastBook = '';
  const scheduleRefresh = () => {
    if (!panelOpen) return;
    clearTimeout(refreshTimer);
    refreshTimer = setTimeout(renderPanel, 500);
  };
  const currentBook = () => (typeof window.__molanCurrentBookId === 'function' ? window.__molanCurrentBookId() : '');

  function act(kind, ch) {
    if (inflight > 0) { toast('当前有生成在进行——等它完成或先停止，再走下一步'); return; }
    try {
      if (kind === 'outline') window.__molanRunChat({ text: '帮我出这本书的全书大纲：核心立意、主线三幕、主要人物与对抗关系、卷结构与前3章钩子。先给我方向选择，确认后再细化。', displayText: '生成全书大纲', skills: ['剧情推演'], files: [], clearComposer: false });
      else if (kind === 'setup') window.__molanRunChat({ text: '根据现有大纲与设定，整理建书档案：书名定档、类型定位、主角与人物表、世界观、力量体系、伏笔台账。', displayText: '整理建书档案', skills: ['设定共建'], files: [], clearComposer: false });
      else if (kind === 'chapter_outline') { if (window.__molanNextOutline) window.__molanNextOutline(); else window.__molanRunChat({ text: '帮我生成第' + ch + '章细纲。若还需对齐走向，只问一个最关键问题。', displayText: '生成第' + ch + '章细纲', skills: ['小说细纲生成'], files: [], clearComposer: false }); }
      else if (kind === 'chapter_body') { if (window.__molanNextBody) window.__molanNextBody(); else window.__molanRunChat({ text: '基于细纲写第' + ch + '章正文，章号必须是第' + ch + '章。', displayText: '写第' + ch + '章正文', skills: ['展开正文写作'], files: [], clearComposer: false }); }
      else if (kind === 'review') { if (window.__wx_pend_reensure) window.__wx_pend_reensure(); toast('请在输入框上方的待审条里预览并接受第' + ch + '章'); }
      else if (kind === 'summary') window.__molanRunChat({ text: '请生成第' + ch + '章的承接摘要（供下一章细纲与正文衔接使用）：本章主要事件、人物关系变化、新埋设与已回收的伏笔、结尾钩子。300字以内，生成后我会保存到 参考/摘要_第' + ch + '章.md。', displayText: '生成第' + ch + '章摘要', skills: [], files: [], clearComposer: false });
      else if (kind === 'batch') { const dock = Array.prototype.find.call(document.querySelectorAll('button'), (b) => /写\s*1\s*章|写第?\d*章/.test(b.textContent || '')); if (dock) { dock.click(); toast('已唤起批量写作链（含双重确认与逐章待审）'); } else toast('批量模式请在聊天页底部的写作链发起'); }
    } catch (e) { toast('操作失败：' + (e && e.message ? e.message : String(e))); }
  }

  const el = (tag, style, text) => { const n = document.createElement(tag); if (style) n.style.cssText = style; if (text != null) n.textContent = text; return n; };
  const btn = (label, onclick, primary) => { const b = el('button', 'border:1px solid ' + (primary ? '#b45309' : '#d4d4d8') + ';border-radius:8px;background:' + (primary ? '#f59e0b' : '#fff') + ';color:' + (primary ? '#fff' : '#3f3f46') + ';padding:5px 12px;cursor:pointer;font-size:12.5px', label); b.onclick = onclick; return b; };

  async function renderPanel() {
    if (!panelEl || !panelOpen) return;
    const body = panelEl.querySelector('.wxpipe__body');
    if (!body) return;
    const bookId = currentBook();
    if (!bookId) { body.replaceChildren(el('div', 'color:#a1a1aa;padding:14px 0', '尚未选择作品'));; return; }
    let st = null;
    try { st = await ipc('get_pipeline_state', { bookId }); } catch (e) {}
    if (currentBook() !== bookId) return; // 迟到响应不覆盖新书状态
    if (!st) { body.replaceChildren(el('div', 'color:#b91c1c;padding:14px 0', '状态加载失败，请刷新重试')); return; }
    body.innerHTML = '';
    const step = (label, ok, detail) => {
      const row = el('div', 'display:flex;align-items:center;gap:8px;padding:5px 0;border-bottom:1px dashed #f0f0f0');
      row.appendChild(el('span', 'width:18px;text-align:center;color:' + (ok ? '#16a34a' : '#d4d4d8'), ok ? '✓' : '○'));
      row.appendChild(el('span', 'font-size:13px;color:#3f3f46', label));
      if (detail) row.appendChild(el('span', 'font-size:11.5px;color:#a1a1aa;margin-left:auto;max-width:130px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap', detail));
      body.appendChild(row);
    };
    step('S1 全书大纲', st.hasOutline, st.outlineFile || '');
    step('S2 建书档案', st.hasSetup, '');
    const chs = st.chapters || [];
    const shown = chs.slice(-10);
    shown.forEach((c) => {
      const okOutline = !!c.outline;
      const bodyState = c.body === 'approved' ? '已定稿' : c.body === 'pending' ? '待审' : '未写';
      const color = c.body === 'approved' ? '#16a34a' : c.body === 'pending' ? '#d97706' : '#a1a1aa';
      step('第' + c.n + '章', okOutline && c.body === 'approved', '细纲' + (okOutline ? '✓' : '—') + ' · 正文:' + bodyState);
      body.lastChild.lastChild.style.color = color;
    });
    if (chs.length > shown.length) body.appendChild(el('div', 'font-size:11.5px;color:#a1a1aa;padding:4px 0', '…共 ' + chs.length + ' 章（定稿 ' + st.counts.approved + ' · 待审 ' + st.counts.pending + '）'));
    const nx = st.next || {};
    const labels = { outline: '去写全书大纲', setup: '整理建书档案', chapter_outline: '生成第' + nx.chapter + '章细纲', chapter_body: '写第' + nx.chapter + '章正文', review: '去审核第' + nx.chapter + '章' };
    const wrap = el('div', 'padding:10px 0 2px');
    wrap.appendChild(el('div', 'font-size:11.5px;color:#a1a1aa;margin-bottom:6px', '下一步（由文件状态推导）'));
    if (labels[nx.stage]) wrap.appendChild(btn(labels[nx.stage], () => act(nx.stage, nx.chapter), true));
    else wrap.appendChild(el('div', 'font-size:12.5px;color:#16a34a', '本章流程完整，可继续下一章'));
    const aux = el('div', 'display:flex;gap:6px;flex-wrap:wrap;margin-top:8px');
    if (st.summaryDue) aux.appendChild(btn('补第' + st.summaryDue + '章摘要（承接）', () => act('summary', st.summaryDue)));
    if ((st.chapters || []).length) aux.appendChild(btn('批量连写…', () => act('batch')));
    if (aux.childElementCount) wrap.appendChild(aux);
    const ctxs = Array.isArray(st.nextContext) ? st.nextContext : [];
    if (ctxs.length) {
      wrap.appendChild(el('div', 'font-size:11px;color:#a1a1aa;margin:8px 0 3px', '下一步将自动携带上下文：'));
      const chips = el('div', '');
      ctxs.forEach((c) => {
        chips.appendChild(el('span', 'display:inline-block;margin:2px 4px 2px 0;padding:2px 8px;border-radius:999px;background:#f4f4f5;color:#52525b;font-size:11px', (c.label || '') + (c.file ? '·' + c.file : '')));
      });
      wrap.appendChild(chips);
    }
    body.appendChild(wrap);
  }

  function togglePanel() {
    if (panelEl && panelOpen) { panelEl.style.display = 'none'; panelOpen = false; return; }
    if (!panelEl) {
      panelEl = el('div', 'position:fixed;right:14px;top:84px;z-index:10001;width:300px;max-height:72vh;overflow:auto;background:#fff;border:1px solid #e4e4e7;border-radius:14px;box-shadow:0 12px 40px rgba(0,0,0,.16);padding:12px 14px');
      panelEl.id = '__wx_pipeline_panel';
      const head = el('div', 'display:flex;align-items:center;justify-content:space-between;margin-bottom:6px');
      head.appendChild(el('b', 'font-size:13.5px;color:#27272a', '生产线'));
      const ops = el('div', 'display:flex;gap:6px');
      ops.appendChild(btn('刷新', () => renderPanel()));
      const close = btn('收起', () => togglePanel());
      ops.appendChild(close);
      head.appendChild(ops);
      panelEl.appendChild(head);
      panelEl.appendChild(el('div', 'class-placeholder', ''));
      const body = el('div', '');
      body.className = 'wxpipe__body';
      panelEl.appendChild(body);
      document.body.appendChild(panelEl);
    }
    panelEl.style.display = '';
    panelOpen = true;
    renderPanel();
  }

  function ensureEntry() {
    if (document.getElementById('__wx_pipeline_entry')) return;
    if (!document.body) return;
    const b = el('button', 'position:fixed;right:0;top:40%;z-index:10002;border:1px solid #e4e4e7;border-right:none;border-radius:10px 0 0 10px;background:#fff;color:#7a5c00;padding:10px 6px;cursor:pointer;font-size:12px;writing-mode:vertical-rl;box-shadow:-2px 2px 10px rgba(0,0,0,.08)', '生产线');
    b.id = '__wx_pipeline_entry';
    b.title = '生产线：查看本书走到哪一步，并按流程继续';
    b.onclick = togglePanel;
    document.body.appendChild(b);
  }
  const boot = () => { ensureEntry(); setInterval(() => { ensureEntry(); const b = currentBook(); if (b !== lastBook) { lastBook = b; scheduleRefresh(); } }, 2000); };
  if (document.body) boot(); else document.addEventListener('DOMContentLoaded', boot, { once: true });
})();
