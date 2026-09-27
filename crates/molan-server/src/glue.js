(() => {
  if (window.__WRITERX_GLUE__) return; window.__WRITERX_GLUE__ = true;
  window.isTauri = true;
  const wxCurrentBook = () => typeof window.__molanCurrentBookId === "function" ? window.__molanCurrentBookId() : "";
  window.__ipcLog = window.__ipcLog || [];
  const _log = (kind, msg) => { window.__ipcLog.push(kind + ': ' + msg); if (window.__ipcLog.length > 500) window.__ipcLog.shift(); };
  // B 契约：fullAuto 只由「本次启动」显式决定。选择仅存本次会话内存（按书），start 后立刻复位 false。
  // 不写 localStorage、不落后端 → 隔日/换设备都不会默认继承全自动定稿。dock 与设置面板共用这一份。
  window.__wx_fullAutoPick = window.__wx_fullAutoPick || Object.create(null);
  const wxFullAutoOf = (bid) => window.__wx_fullAutoPick[bid] === true;
  const wxSetFullAuto = (bid, v) => { window.__wx_fullAutoPick[bid] = v === true; };
  const wxResetFullAuto = (bid) => { window.__wx_fullAutoPick[bid] = false; };
  const wxConfirmWrite = (bookName, fromCh, toCh, fullAuto) => {
    const count = toCh - fromCh + 1;
    const mode = fullAuto ? '全自动定稿（不逐章询问，审核后直接写入正文）' : '逐章待审（每章需要你接受后才转正）';
    if (!window.confirm('即将为《' + bookName + '》生成第 ' + fromCh + '—' + toCh + ' 章（共 ' + count + ' 章）。\n模式：' + mode + '。\n\n确认开始生成吗？')) return false;
    return !fullAuto || window.confirm('再次确认：本次将连续生成并自动定稿，过程中不逐章征求你的意见。确定要开启吗？');
  };
  // F12：delete_file 返回 trashId，撤销/还原用它；旧前端误传对象 id 导致恢复静默失败，此处换回真 id。
  (function(){ // F12: delete_file 返回 trashId；restore_trash 收到对象 id 时换回真 id，查不到报可见错误
  const R = new Map();
  invoke = async (cmd, a, o) => { const res = await invokeBase(cmd, a, o); try {
    if (cmd === 'delete_file' && typeof res.trashId === 'string' && res.trashId) R.set([a.bookId, a.group, a.name].join('|'), res.trashId);
    if (cmd === 'restore_trash' && a.id && typeof a.id === 'object') { const id = a.id.trashId || R.get([a.bookId || '', a.id.group || '', a.id.name || ''].join('|'));
      if (id) return invokeBase('restore_trash', { ...a, id }, o);
      return { ok: false, err: '撤销超时或记录已失效，请到目录栏「回收站」手动还原' }; }
  } catch (e) {} return res; }; })();
  // 优先使用当前书的真实会话；last_state 可能指向旧书或已删会话，必须向后端核实。
  const wxSessionForBook = async (bookId) => {
    const sessions = (await invoke('list_sessions', { bookId })) || [];
    if (!Array.isArray(sessions)) throw new Error('会话列表不可用，已停止启动以免生成记录丢失');
    const settings = (await invoke('get_settings', {})) || {};
    let state = {};
    try { state = JSON.parse(settings.last_state || '{}'); } catch (e) {}
    const preferred = wxCurrentBook() === bookId && typeof window.__molanCurrentSessionId === "function" ? window.__molanCurrentSessionId() : state.bookId === bookId && (state.session || state.sessionId);
    const selected = sessions.find(s => s.id === preferred) || sessions[0];
    const persist = id => Promise.resolve(id); // 启动后台任务不改变用户当前导航；last_state由store负责。
    if (selected && selected.id) return await persist(selected.id);
    if (!window.confirm('本书还没有写作会话，要新建一个「写作会话」吗？')) throw new Error('未创建会话：你取消了新建，已停止启动（绝不静默创建，避免重复会话）');
    const created = await invoke('create_session', { bookId, title: '写作会话' }); if (!created || !created.id) throw new Error('创建会话失败，已停止启动');
    return await persist(created.id);
  };
  window.addEventListener('error', (e) => _log('ERR', (e.message || '') + ' || STACK: ' + String(e.error && e.error.stack || '') .slice(0, 1200)));
  window.addEventListener('unhandledrejection', (e) => _log('REJ', String(e.reason && e.reason.stack || e.reason || e)).slice(0, 1200));
  // 离线版清理：隐藏账号/会员相关 UI（左栏账号按钮 + 设置里的账号页）
  const purgeAccountUI = () => {
    try {
      if (!document.querySelector('style#__wx_no_account')) {
        const st = document.createElement('style');
        st.id = '__wx_no_account';
        st.textContent =
          '.notice-mask{display:none !important}' +
          '[class*="userbar"],button.mdrawer__user{display:none !important}' +
          'button.set__navitem[data-wx-hide]{display:none !important}' +
          '.acct__hd,.acct__vip,.acct__guest{display:none !important}';
        document.head.appendChild(st);
      }
      // 官方痕迹文本过滤：命中黑名单的小部件直接隐藏，长文本中清词
      const OFFICIAL_WORDS = ['QQ交流群', 'QQ 交流群', '550954548', '免费会员', '正式会员', '点此了解会员', '点此订阅',
        '注册即送', '试用已结束', '订阅已到期', '内置免费模型', '会员专供', '次数包', '永久授权', '预览通道',
        'mochang', 'writerx.cn', 'hub.writerx', '墨知', '检查更新', '使用手册与更新日志', '官网 · 最新版本'];
      // 营销文案简化：离线版无次数概念，去掉「免费模型」话术
      const SIMPLIFY_WORDS = [['v1.0.3', ''], ['版本 1.0.3', ''], ['免费模型不限量', ''], ['余额跨章结算，不够写下一章会暂停', '长文连写可随时停止'], ['免费模型', '模型']];
      const simplifyText = () => {
        try {
          const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
          while (walker.nextNode()) {
            const n = walker.currentNode;
            for (const pair of SIMPLIFY_WORDS) {
              if (n.nodeValue && n.nodeValue.includes(pair[0])) n.nodeValue = n.nodeValue.split(pair[0]).join(pair[1]);
            }
          }
        } catch (e) {}
      };
      simplifyText();
      const purgeText = () => {
        try {
          const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
          const hits = [];
          while (walker.nextNode()) {
            const n = walker.currentNode;
            for (const w of OFFICIAL_WORDS) {
              if (n.nodeValue && n.nodeValue.includes(w)) { hits.push({ n, w }); break; }
            }
          }
          for (const { n, w } of hits) {
            let el = n.parentElement;
            // 小元素：整块隐藏
            if (el && el.children.length <= 2 && el.textContent.length < 140) {
              el.closest('.userbar, [class*="userbar"], button, [class*="badge"], [class*="vip"], [class*="version"], [class*="chmgr"], [class*="builtin"]')?.style.setProperty('display', 'none', 'important');
              el.style.setProperty('display', 'none', 'important');
              continue;
            }
            // 卡片级容器（如「内置免费模型」整卡）：找最近的视觉卡再隐藏
            let card = el;
            for (let up = 0; up < 5 && card && card !== document.body; up++) {
              if (card.childElementCount <= 12 && (card.textContent || '').length <= 600) { if (up >= 2) break; card = card.parentElement; }
              else break;
            }
            if (card && card !== document.body && (card.textContent || '').length <= 600) {
              card.style.setProperty('display', 'none', 'important');
              continue;
            }
            n.nodeValue = n.nodeValue.split(w).join('');
          }
        } catch (e) {}
      };
      const markAccount = () => Array.from(document.querySelectorAll('button.set__navitem'))
        .forEach(b => { if (['账号', '反馈与支持'].includes(b.textContent.trim())) b.setAttribute('data-wx-hide', '1'); });
      purgeText();
      simplifyText();
      markAccount();
      // 设置面板打开时，若激活的是已被隐藏的账号页，则退回「模型」页
      const fallbackToModel = () => {
        const activeItem = document.querySelector('button.set__navitem.is-active');
        if (activeItem && activeItem.hasAttribute('data-wx-hide')) {
          const mb = Array.from(document.querySelectorAll('button.set__navitem'))
            .find(b => b.textContent.trim() === '模型');
          if (mb) mb.click();
        }
      };
      fallbackToModel();
      let moTimer = null;
      const mo = purgeAccountUI._mo || (purgeAccountUI._mo = new MutationObserver(() => {
        // v2.2 流畅度：流式输出时 DOM 每秒几十批，合并为最多 500ms 一次文本巡检
        if (moTimer !== null || document.hidden) return;
        moTimer = setTimeout(() => {
          moTimer = null;
          if (document.hidden) return;
          markAccount();
          simplifyText();
          fallbackToModel();
        }, 500);
      }));
      mo.observe(document.body, { childList: true, subtree: true });
    } catch (e) {}
  };
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', () => { setTimeout(purgeAccountUI, 800); });
  } else {
    setTimeout(purgeAccountUI, 800);
  }

  // 自动关闭官方欢迎/免责声明弹窗（离线版无更新提示，属于纯打扰）
  // 欢迎弹窗在加载后约 4 秒才出现：轮询直到它出现并关闭（最多 20 秒）
  let welcomeTries = 0;
  const dismissWelcome = () => {
    try {
      const dlg = document.querySelector('[role="dialog"]');
      if (dlg) {
        const btns = Array.from(dlg.querySelectorAll('button'));
        const know = btns.find((b) => b.textContent.trim() === '我知道了');
        if (know) { know.click(); }
      }
    } catch (e) {}
  };
  const welcomeTimer = setInterval(() => {
    try {
      const dlg = document.querySelector('[role="dialog"]');
      if (dlg) {
        const know = Array.from(dlg.querySelectorAll('button')).find((b) => b.textContent.trim() === '我知道了');
        if (know) know.click();
      }
    } catch (e) {}
    if (++welcomeTries > 25) clearInterval(welcomeTimer);
  }, 800);
  setTimeout(dismissWelcome, 1500);
  const channels = new Map();
  // 官方 Tauri v2 协议：Channel 通过 transformCallback 注册回调，得到数字 id；
  // 序列化为 "__CHANNEL__:<id>" 字符串，事件经 runCallback(id, {index, message}) 送达。
  const cbRegistry = new Map();
  let cbSeq = 1;
  function transformCallback(cb, once = false) {
    const id = cbSeq++;
    cbRegistry.set(id, { cb: typeof cb === 'function' ? cb : () => {}, once: !!once });
    return id;
  }
  function unregisterCallback(id) { cbRegistry.delete(Number(id)); }

  const isChannel = (x) =>
    x && typeof x === 'object' && x.id != null && typeof x.onmessage === 'function';

  function clone(v, seen) {
    if (v === null || typeof v !== 'object') return v;
    // 官方 Channel：toJSON() -> "__CHANNEL__:<id>"，走 transformCallback 回调协议
    if (typeof v.toJSON === 'function' && !(v instanceof Date)) {
      const j = v.toJSON();
      if (typeof j === 'string' && j.startsWith('__CHANNEL__')) return j;
    }
    if (isChannel(v)) {
      channels.set(String(v.id), v);
      return { id: v.id };
    }
    if (seen.has(v)) return undefined;
    seen.set(v, true);
    let out;
    if (Array.isArray(v)) {
      out = v.map((x) => clone(x, seen));
    } else if (v instanceof Date) {
      out = v.toISOString();
    } else {
      out = {};
      for (const k of Object.keys(v)) {
        const c = clone(v[k], seen);
        if (c !== undefined) out[k] = c;
      }
    }
    seen.delete(v);
    return out;
  }

  function parseLine(line, state) {
    if (!line.trim()) return;
    let m;
    try { m = JSON.parse(line); } catch (e) { return; }
    if (m.e !== undefined && channels.has(String(m.ch))) {
      try { channels.get(String(m.ch))(m.e); } catch (e) { console.warn('[glue] channel cb error', e); }
    } else if (m.e !== undefined) {
      const idx = state.seq.get(String(m.ch)) || 0;
      state.seq.set(String(m.ch), idx + 1);
      runCallbackId(m.ch, { index: idx, message: m.e });
    } else if (m.r !== undefined) {
      state.result = m.r;
    } else if (m.err) {
      const err = new Error(m.err.message || 'IPC error');
      if (m.err.msg) err.msg = m.err.msg;
      state.error = err;
    }
  }

  async function invokeBase(cmd, args = {}, options = {}) {
    _log('INVOKE', cmd); if (cmd === 'delete_session' && args && args.confirm !== args.sessionId) { if (!window.confirm('确定永久删除这个会话及全部消息吗？此操作不可撤销。')) throw new Error('已取消删除会话'); args = Object.assign({}, args, { confirm: args.sessionId }); }
    // 危险操作安全拦截：清空全库需人工二次确认并补全令牌
    if (cmd === 'reset_all_data' && (!args || args.confirm !== 'RESET_ALL')) {
      const confirmed = window.confirm('⚠️ 危险警告：清空全部数据将彻底删除所有书籍、会话和消息记录！\n\n确定要清空吗？此操作不可逆。');
      if (!confirmed) {
        throw new Error('用户已取消重置全部数据');
      }
      args = Object.assign({}, args, { confirm: 'RESET_ALL' });
    }
    // 浏览器可本地完成的命令
    if (cmd === 'open_url' && args && args.url) {
      window.open(args.url, '_blank', 'noopener');
      return null;
    }
    if (cmd === 'log_frontend_error') {
      console.warn('[frontend error]', args && (args.message || args.context));
      return null;
    }
    // 记录当前书：glue 拿不到 React store，用「最近一次带 bookId 的请求」作为当前书兜底，
    // 供 F16 按 bookId 记 dirty、以及导出/任务归属判断使用（不改变任何 IPC 语义）。
    if (wxCurrentBook()) window.__wx_last_book_id = wxCurrentBook();
    // 变更数据目录：网页版数据在服务器，这只会写 settings 却让用户以为「已迁移」。
    // 契约要求未实现能力明确不支持。真实不可用就在最早处拒绝，绝不假成功。
    if (cmd === 'change_data_dir') {
      throw new Error('网页版不支持更改数据目录：工作区由服务器挂载配置。如需迁移请在宿主机调整挂载，或联系管理员。');
    }

    // 供导出取文件名/触发下载的小工具（仅前端本地，不落盘任何人为伪造路径）。
    const __wx_blob_download = (b64, name, mime) => {
      const bin = atob(b64);
      const bytes = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      const blob = new Blob([bytes], { type: mime || 'application/octet-stream' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url; a.download = name || 'molan-export'; a.style.display = 'none';
      document.body.appendChild(a);
      a.click();
      setTimeout(() => { try { a.remove(); URL.revokeObjectURL(url); } catch (e) {} }, 4000);
      _log('EXPORT', (name || '') + ' · ' + bin.length + ' bytes');
      return true;
    };

    // ---- F17：网页版文件对话框（plugin:dialog|save/open）→ 服务端 web_export 真正的 Blob 下载 ----
    // 原生桌面 dialog 在网页版被后端明确拒绝（handlers/mod.rs: 网页版不支持文件选择弹窗），
    // 而官方 bundle 的「备份全书(zip)/导出全本 TXT/导出全部数据」都要先拿到 dialog 返回的路径。
    // 这里把 dialog|save 私有地变成「给调用方一个虚拟路径」，再在后续 export_* 调用处用 web_export
    // 取真实字节并触发浏览器 Blob 下载。绝不返回假路径让用户以为已落盘；字节必须来自服务端。
    // 只接管「导出/备份」这一类 dialog：defaultPath 为 .zip/.txt。技能包(.wxskill)、SKILL.md、
    // 导入文件选择等仍按原样落到后端，由后端给出明确「网页版不支持」错误——不伪造它们的成功。
    const __wx_is_export_dialog = (o) => !!o && /\.(zip|txt)$/i.test(String(o.defaultPath || ''));
    if (cmd === 'plugin:dialog|save' && args && __wx_is_export_dialog(args.options)) {
      const base = String(args.options.defaultPath || '').replace(/^.*[\\/]/, '') || 'molan-export.zip';
      window.__wx_export_dlg = { name: base };
      return base;
    }
    // 仅「选择导出位置」这个目录选择属于导出流程（export_txt）；数据目录迁移等其它目录选择
    // 不接管，仍落到后端给出明确「不支持」错误，避免伪造一个假目录让用户以为已选好。
    const __wx_is_export_dir_dialog = (o) => !!o && o.directory === true && String(o.title || '') === '选择导出位置';
    if (cmd === 'plugin:dialog|open' && args && __wx_is_export_dir_dialog(args.options)) {
      window.__wx_export_dlg = { name: 'molan-export.txt' };
      return '（网页下载）';
    }

    // 诊断包：后端已返回真实文本内容（非文件路径）。直接在浏览器落成 .txt，不走 web_export（那是书稿导出）。
    // 用一次性 passthrough 标记避免递归调用自身。
    if (cmd === 'export_diagnostics' && !window.__wx_diag_passthrough) {
      window.__wx_diag_passthrough = true;
      let out = null;
      try { out = await invoke('export_diagnostics', args, options); }
      finally { window.__wx_diag_passthrough = false; }
      const content = (out && out.content) || '';
      if (!content) throw new Error('导出失败：服务端未返回诊断内容');
      const bytes = new TextEncoder().encode(content);
      let bin = '';
      for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
      __wx_blob_download(btoa(bin), (out && out.file) || 'diagnostics.txt', 'text/plain;charset=utf-8');
      return out.file || 'diagnostics.txt';
    }

    const __WX_EXPORT_CMDS = { export_book_zip: 1, export_txt: 1 };
    if (__WX_EXPORT_CMDS[cmd]) {
      const fmt = cmd === 'export_book_zip' ? 'zip' : 'txt';
      const out = await invoke('web_export', { bookId: (args && args.bookId) || wxCurrentBook() || '', format: fmt });
      if (!out || out.ok !== true || !out.base64) {
        throw new Error((out && out.message) || '导出失败：服务端未返回可下载内容');
      }
      const name = window.__wx_export_dlg && window.__wx_export_dlg.name ? window.__wx_export_dlg.name : (out.name || 'molan-export.' + fmt);
      window.__wx_export_dlg = null;
      const mime = out.mime || (fmt === 'zip' ? 'application/zip' : 'text/plain;charset=utf-8');
      __wx_blob_download(out.base64, name, mime);
      return out.name || name;
    }
    if (cmd === 'export_all_data') {
      const out = await invoke('web_export', { bookId: '', format: 'all' });
      if (!out || out.ok !== true || !out.base64) {
        throw new Error((out && out.message) || '导出失败：服务端未返回可下载内容');
      }
      __wx_blob_download(out.base64, out.name || '墨澜工坊-数据备份.zip', out.mime || 'application/zip');
      return out.name || '墨澜工坊-数据备份.zip';
    }

    const body = clone(args, new Map());
    let resp;
    try {
      resp = await fetch('/ipc/' + cmd, {
        method: 'POST',
        // G 鉴权：浏览器同源请求默认携带 HttpOnly 会话 Cookie（credentials 默认 'same-origin'，
        // 显式写出以防被包装/覆盖）。匿名主页不再注入 token；X-Molan-Token 仅作脚本验收兼容。
        credentials: 'same-origin',
        headers: { 'Content-Type': 'application/json', ...(window.__MOLAN_TOKEN__ ? { 'X-Molan-Token': window.__MOLAN_TOKEN__ } : {}) },
        body: JSON.stringify({ args: body, options: options || {} }),
      });
    } catch (e) {
      throw new Error('[glue] 无法连接复刻后端，请确认 node server.js 正在运行：' + e.message);
    }
    if (!resp.ok) {
      let t = '';
      try { t = await resp.text(); } catch (e) {}
      throw new Error('[glue] IPC HTTP ' + resp.status + ': ' + (t || '').slice(0, 300));
    }
    const state = { result: undefined, error: null, seq: new Map() };
    const reader = resp.body.getReader();
    const dec = new TextDecoder();
    let buf = '';
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += dec.decode(value, { stream: true });
      let nl;
      while ((nl = buf.indexOf('\n')) !== -1) {
        parseLine(buf.slice(0, nl), state);
        buf = buf.slice(nl + 1);
      }
    }
    if (state.error) throw state.error;
    return state.result;
  }

  const runCallbackId = (cbId, value) => {
    const entry = cbRegistry.get(Number(cbId));
    if (entry) {
      if (entry.once) cbRegistry.delete(Number(cbId));
      try { entry.cb(value); } catch (e) { console.warn('[glue] cb error', e); }
    }
  };
  const internals = {
    invoke,
    convertFileSrc(p) { return p; },
    transformCallback,
    unregisterCallback,
    runCallback: runCallbackId,
    metadata: {
      currentWindow: { label: 'main' },
      currentWebview: { label: 'main' },
      currentWebviewWindow: { label: 'main' },
    },
  };
  Object.defineProperty(window, '__TAURI_INTERNALS__', { value: internals, configurable: true });
  // 部分内部代码会读 window.__TAURI__（如 isTauri 检查的另一形态）
  Object.defineProperty(window, '__TAURI__', { value: internals, configurable: true });

  // ============ 「墨案」主题：暖纸底 + 墨色 + 朱砂 accent（方案 v2 P0） ============
  // 全部走官方 CSS 变量替换（零结构改动），light/dark 两套都覆盖。
  // 字体：界面=内置思源黑（清晰度）；小说正文=内置思源宋（书卷气）。
  (function inkTheme() {
    var ID = '__WX_INK_THEME__';
    var CSS = [
      // ---- 内置字体 ----
      '@font-face{font-family:"Noto Sans SC WX";src:url("/assets/fonts/notosanssc.woff2") format("woff2");font-weight:400;font-style:normal;font-display:swap}',
      '@font-face{font-family:"Noto Sans SC WX";src:url("/assets/fonts/notosanssc-700.woff2") format("woff2");font-weight:700;font-style:normal;font-display:swap}',
      '@font-face{font-family:"Noto Serif SC WX";src:url("/assets/fonts/notoserifsc.woff2") format("woff2");font-weight:400;font-style:normal;font-display:swap}',
      '@font-face{font-family:"Noto Serif SC WX";src:url("/assets/fonts/notoserifsc-600.woff2") format("woff2");font-weight:600;font-style:normal;font-display:swap}',
      // ---- 字体变量：UI=黑体，正文=宋体 ----
      ':root{--font-ui:"Noto Sans SC WX","MiSans","HarmonyOS Sans SC","PingFang SC","Segoe UI Variable Text","Segoe UI",system-ui,sans-serif !important;' +
        '--font-prose:"Noto Serif SC WX","Source Han Serif SC","Noto Serif SC","Georgia",serif !important;' +
        '--font-brand:"Noto Sans SC WX","MiSans","HarmonyOS Sans SC","Segoe UI Variable Display","Segoe UI",system-ui,sans-serif !important;' +
        // ---- 墨案 Token（浅色）----
        '--c-app-bg:#FAF8F4 !important;--c-topbar-bg:#FAF8F4 !important;--c-left-bg:#F4F1EB !important;--c-center-bg:#FAF8F4 !important;' +
        '--c-white:#FFFFFF !important;--c-card-light:#FFFFFF !important;--c-card-light-2:#FDFCFA !important;--c-card-light-3:#FFFFFF !important;' +
        '--c-accent:#C7433C !important;--c-accent-hover:#B03A34 !important;--c-accent-bg:#FBEAE9 !important;--c-accent-border:#F0CBC8 !important;--c-accent-row-sel:#FDF5F4 !important;' +
        '--c-text-main:#1F2328 !important;--c-text-2:#3F4550 !important;--c-text-3:#5C6470 !important;--c-text-secondary:#5C6470 !important;--c-text-secondary-2:#6B7280 !important;' +
        '--c-text-muted:#9AA1AC !important;--c-text-muted-2:#9AA1AC !important;--c-text-placeholder:#9AA1AC !important;--c-text-btn:#5C6470 !important;' +
        '--c-border:#E8E3DA !important;--c-border-2:#E8E3DA !important;--c-border-3:#EFEBE3 !important;--c-border-btn:#DCD6CA !important;--c-border-input:#DCD6CA !important;--c-border-soft:#EFEBE3 !important;' +
        '--c-row-hover:#F5F2EC !important;--c-row-active-bg:#EFEAE0 !important;' +
        '--c-user-bubble-bg:#F4F1EB !important;--c-user-bubble-border:#E8E3DA !important;--c-user-bubble-text:#1F2328 !important;' +
        '--sh-card:0 1px 2px rgba(90,70,40,.04),0 4px 16px rgba(90,70,40,.06) !important;' +
        '--sh-card-hover:0 2px 6px rgba(90,70,40,.06),0 12px 32px rgba(90,70,40,.10) !important;' +
        '--sh-card-soft:0 1px 3px rgba(90,70,40,.05) !important;' +
        '--sh-btn:0 1px 2px rgba(90,70,40,.08) !important;' +
        // 小说正文：宋体 + 17px/2.0 + 首行缩进 + 锁列（ai 消息 = 正文流）
        '.aimsg__prose,.prose{font-family:var(--font-prose) !important;font-size:17px !important;line-height:2.0 !important;color:var(--c-text-main) !important}' +
        '.aimsg__prose p{margin:.35em 0 !important}' +
        '.aimsg__prose > p:first-child{text-indent:0 !important}' +
        '@media (min-width:1024px){.chatmain{max-width:calc(34em * 17px * 0.5 + 88px);margin:0 auto;width:100%}}' +
        'html{-webkit-font-smoothing:antialiased !important;text-rendering:optimizeLegibility !important}' +
        // ---- 暗色主题（夜写）----
        ':root[data-theme=dark],html[data-theme=dark]{' +
        '--c-app-bg:#17181A !important;--c-topbar-bg:#17181A !important;--c-left-bg:#141518 !important;--c-center-bg:#17181A !important;' +
        '--c-white:#1E2023 !important;--c-card-light:#1E2023 !important;--c-card-light-2:#1C1E21 !important;--c-card-light-3:#1E2023 !important;' +
        '--c-accent:#C7433C !important;--c-accent-hover:#D45A53 !important;--c-accent-bg:#3A2220 !important;--c-accent-border:#5C3430 !important;--c-accent-row-sel:#2E1D1B !important;' +
        '--c-text-main:#E6E3DC !important;--c-text-2:#C9C4BB !important;--c-text-3:#A8A29A !important;--c-text-secondary:#A8A29A !important;--c-text-secondary-2:#8F897F !important;' +
        '--c-text-muted:#6B6560 !important;--c-text-muted-2:#6B6560 !important;--c-text-placeholder:#6B6560 !important;--c-text-btn:#A8A29A !important;' +
        '--c-border:#2A2C30 !important;--c-border-2:#2A2C30 !important;--c-border-3:#24262A !important;--c-border-btn:#35373C !important;--c-border-input:#35373C !important;--c-border-soft:#24262A !important;' +
        '--c-row-hover:#202226 !important;--c-row-active-bg:#26282C !important;' +
        '--c-user-bubble-bg:#1E2023 !important;--c-user-bubble-border:#2A2C30 !important;--c-user-bubble-text:#E6E3DC !important;' +
        '--sh-card:0 1px 2px rgba(0,0,0,.2),0 4px 16px rgba(0,0,0,.25) !important;' +
        '--sh-card-hover:0 2px 6px rgba(0,0,0,.25),0 12px 32px rgba(0,0,0,.35) !important;' +
        '--sh-card-soft:0 1px 3px rgba(0,0,0,.2) !important;' +
        '--sh-btn:0 1px 2px rgba(0,0,0,.3) !important}'
    ].join('');
    function ensure() {
      if (document.getElementById(ID)) return;
      var st = document.createElement('style');
      st.id = ID;
      st.textContent = CSS;
      (document.head || document.documentElement).appendChild(st);
    }
    if (document.head) ensure();
    new MutationObserver(ensure).observe(document.documentElement, { childList: true, subtree: true });
  })();

  // ============ H5 止血四件套（方案 v2 §5.3）：dvh/输入字号/安全区/滚动 ============
  (function h5Fix() {
    var ID = '__WX_H5_FIX__';
    var CSS = [
      '@media (max-width:720px){',
      // 单栏止血：桌面骨架右栏在窄屏挤满宽度 → 隐藏右栏让对话流铺满（方案 §5.1 手机单栏）
      '.body__side--right{display:none !important}',
      '.centerstage{display:flex !important;width:100% !important;flex:1 !important}',
      '.chathead__tools .chathead__search{display:none !important}',
      // 视口高度：禁 100vh，用 dvh 兜底 svh
      'html,body{height:100dvh !important;overflow:hidden !important;overscroll-behavior:none !important}',
      '@supports not (height:100dvh){html,body{height:100svh !important}}',
      // 输入框字号 ≥16px 防 iOS focus 缩放；触摸目标 ≥44px
      'html .dock__textarea,html .dock__compose,html .mdock__input{font-size:16px !important}',
      'html .dock__send{min-width:44px !important;min-height:44px !important}',
      'html .dock__tool,html .dock__skill,html .dock__file,html .dock__model{min-height:36px !important}',
      // 底部安全区
      'html .dockwrap{padding-bottom:calc(8px + env(safe-area-inset-bottom,0px)) !important}',
      'html .mshell__top{padding-top:env(safe-area-inset-top,0px) !important}',
      // 对话流滚动穿透防护
      'html .chatmain{overscroll-behavior:contain !important;-webkit-overflow-scrolling:touch !important}',
      'html body{touch-action:manipulation !important}',
      '}',
      // 手机正文阅读排版：行高 1.9，两侧 20px（方案 §5.4）
      '@media (max-width:720px){.aimsg__prose,.prose{font-size:17px !important;line-height:1.9 !important;padding:0 4px !important}}'
    ].join('');
    function ensure() {
      if (document.getElementById(ID)) return;
      var st = document.createElement('style');
      st.id = ID;
      st.textContent = CSS;
      (document.head || document.documentElement).appendChild(st);
    }
    if (document.head) ensure();
    new MutationObserver(ensure).observe(document.documentElement, { childList: true, subtree: true });
  })();

  // ============ 移动端紧凑样式：动态注入并常驻 head 末尾（压过应用内字号设置） ============
  (function mobileCss() {
    var ID = '__WX_MOBILE_CSS__';
    var CSS = [
      '@media (max-width:720px){',
      'html:root{--fs-body:17px !important;--font-ui:-apple-system,BlinkMacSystemFont,"PingFang SC","HarmonyOS Sans SC","MiSans","Microsoft YaHei",sans-serif !important;--font-prose:-apple-system,BlinkMacSystemFont,"PingFang SC","HarmonyOS Sans SC","MiSans","Microsoft YaHei",sans-serif !important}',
      '.mshell__top{height:40px !important}.header{height:40px !important}.header__tag{display:none !important}',
      'html .chathead{height:42px !important;padding:0 10px !important;gap:8px !important}',
      '.chathead__tools{gap:5px !important}',
      'html .chathead__toolbtn{height:28px !important;padding:0 8px !important;font-size:12px !important}',
      '.chathead__msgcount{display:none !important}',
      '.aimsg{gap:10px !important}',
      'html .usermsg__bubble{padding:8px 12px !important;font-size:17px !important}',
      '.dockwrap{padding:0 8px calc(6px + env(safe-area-inset-bottom,0px)) !important}',
      'html .dock__compose{padding:8px 10px 4px !important;font-size:17px !important}',
      'html .dock__textarea{max-height:38vh !important;font-size:17px !important}',
      '.dock__refs{top:8px !important;left:10px !important}',
      'html .dock__toolbar{padding:4px 8px 6px !important;gap:5px !important}',
      '.dock__tool,.dock__skill,.dock__file,.dock__web-search,.dock__next,.dock__model,.dock__reasoning{height:28px !important;padding:0 8px !important;font-size:12px !important}',
      'html .dock__send{flex:0 0 28px !important;width:28px !important;height:28px !important}',
      '.dock__disclaimer{display:none !important}',
      '.dock__batchhint{padding:6px 8px !important;font-size:11px !important}',
      '.dock__quota{margin-left:4px !important;font-size:10px !important}',
      '.dock__model{max-width:150px !important}',
      'html .editorpane__bar{min-height:42px !important;padding:5px 8px !important}',
      'html .editorpane__ai{height:28px !important;padding:0 8px !important;font-size:12px !important}',
      'html .mtool__scroll{padding:8px 10px 10px !important}',
      'html .modal__sub{font-size:12.5px !important}',
      'html .aimsg__body,html .aimsg__prose{font-size:17px !important;line-height:1.78 !important}',
      'html .aimsg__prose h1{font-size:21px !important}',
      'html .aimsg__prose h2{font-size:19px !important}',
      'html .aimsg__prose h3{font-size:17.5px !important}',
      'html .aimsg__prose h4{font-size:16.5px !important}',
      'html .aimsg__prose table{font-size:15px !important}',
      'html .aimsg__prose pre code{font-size:14px !important}',
      'html .usermsg__text{font-size:17px !important}',
      'html .mdock__input{font-size:17px !important}',
      'html .mdrawer__row{font-size:16.5px !important}',
      'html .chathead__title{font-size:17px !important}',
      'html .mshell__title{font-size:17px !important}',
      '}'
    ].join('');
    var ensure = function () {
      var el = document.getElementById(ID);
      if (!el) {
        el = document.createElement('style');
        el.id = ID;
        el.textContent = CSS;
      }
      // 常驻 head 末尾：被其它脚本挤掉位置就重新追加
      if (el.parentNode !== document.head || document.head.lastElementChild !== el) {
        document.head.appendChild(el);
      }
    };
    setInterval(ensure, 700);
    if (document.body) ensure();
    else document.addEventListener('DOMContentLoaded', ensure);
  })();

  // ============ 构建版本检测：服务器更新后客户端自动强刷一次 ============
  (function buildCheck() {
    try {
      var ts = window.__WX_BUILD__ || '';
      if (!ts) return;
      var k = '__wx_build_seen';
      var seen = localStorage.getItem(k);
      localStorage.setItem(k, ts);
      if (seen && seen !== ts) {
        sessionStorage.setItem('__wx_hard_reload', '1');
        window.location.reload();
      }
    } catch (e) {}
  })();

  // ============ 移动端自动识别：触屏小屏设备强制 ?mobile=1 进入移动布局 ============
  // 官方前端只认 Android UA（lt() 判定），iPhone/iPad 会被判成桌面三栏。这里补齐：
  (function mobileDetect() {
    try {
      var q = new URLSearchParams(window.location.search);
      if (q.get('mobile') === '1' || q.get('mobile') === '0') return; // 用户已显式指定
      var touch = ('ontouchstart' in window) || (navigator.maxTouchPoints || 0) > 0;
      var small = Math.min(window.screen.width, window.screen.height) <= 820;
      var narrow = Math.min(window.innerWidth, window.innerHeight) <= 560; // 窄窗口（含桌面缩窄）也走移动壳（方案 §5.1）
      var mobileUA = /Android|iPhone|iPad|iPod|Mobile|HarmonyOS/i.test(navigator.userAgent);
      if ((mobileUA && touch) || (touch && small) || (mobileUA && narrow)) {
        q.set('mobile', '1');
        window.location.replace(window.location.pathname + '?' + q.toString() + window.location.hash);
      }
    } catch (e) {}
  })();

  // ============ 移动抽屉：注入「书本目录」入口（直达资料库目录页） ============
  (function mdrawerLibrary() {
    setInterval(function () {
      try {
        var drawer = document.querySelector('.mdrawer');
        if (!drawer || drawer.querySelector('.__mdl_injected')) return;
        var rows = Array.prototype.slice.call(drawer.querySelectorAll('.mdrawer__row'));
        var searchRow = null;
        for (var i = 0; i < rows.length; i++) {
          if (rows[i].textContent.indexOf('搜索') >= 0) { searchRow = rows[i]; break; }
        }
        if (!searchRow) return;
        var btn = document.createElement('button');
        btn.className = 'mdrawer__row __mdl_injected';
        btn.innerHTML = '<svg xmlns="http://www.w3.org/2000/svg" width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M2 3h6a4 4 0 0 1 4 4v14a3 3 0 0 0-3-3H2z"></path><path d="M22 3h-6a4 4 0 0 0-4 4v14a3 3 0 0 1 3-3h7z"></path></svg><span>书本目录</span>';
        btn.addEventListener('click', function () {
          var openLib = function () {
            var lib = document.querySelector('.mshell__top button[aria-label="资料库"]');
            if (lib) lib.click();
          };
          if (document.querySelector('.mshell__top button[aria-label="资料库"]')) {
            var burger = document.querySelector('.mshell__top button[aria-label="会话"]');
            if (burger) burger.click();
            setTimeout(openLib, 180);
          } else {
            var back = document.querySelector('button[aria-label="返回"]');
            if (back) back.click();
            setTimeout(openLib, 400);
          }
        });
        searchRow.insertAdjacentElement('afterend', btn);
      } catch (e) {}
    }, 800);
  })();

  // ============ 共享小工具（F8 去重：书源下载 / Dock / 待审条 / 写作面板原本各自重复定义） ============
  // 原始 IPC 通道：逐行解析 r/err 帧（与原各挂载内局部 ipc 实现逐字等价，仅收敛为一份）。
  async function __wx_ipc_raw(cmd, args) {
    const resp = await fetch('/ipc/' + cmd, {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json', ...(window.__MOLAN_TOKEN__ ? { 'X-Molan-Token': window.__MOLAN_TOKEN__ } : {}) },
      body: JSON.stringify({ args: args || {} }),
    });
    let text = '';
    try { text = await resp.text(); } catch (e) {}
    // 与 invoke() 同规则：HTTP 非 200 一律抛错（协议上处理器错误走 200 的 err 帧，非 200 必是失败）
    if (!resp.ok) throw new Error('IPC HTTP ' + resp.status + ': ' + (text || '').slice(0, 300));
    let last = null;
    for (const line of text.split('\n')) {
      if (!line.trim()) continue;
      const j = JSON.parse(line);
      if (j && j.r !== undefined) last = j.r;
      if (j && j.err) throw new Error(j.err.message || 'IPC error');
    }
    return last;
  }
  // 「正文」组最新章号（写作面板与 Dock 启动共用）
  async function __wx_latest_chapter_of(bookId) {
    try {
      const tree = await invoke('scan_tree', { bookId });
      let max = 0;
      for (const g of (tree || [])) {
        if (g.dir !== '正文' && g.groupDir !== '正文') continue;
        for (const f of (g.files || [])) {
          const m = String(f.name || '').match(/第(\d+)章/);
          if (m) max = Math.max(max, parseInt(m[1], 10));
        }
      }
      return max;
    } catch (e) { return 0; }
  }
  // 当前书：读 settings.last_state（Dock 与待审浮条共用；失败返回 null 不猜书）
  async function __wx_current_state() {
    try {
      const all = await invoke('get_settings');
      const raw = all && all['last_state'];
      if (!raw) return null;
      const st = JSON.parse(raw);
      return st && st.bookId ? st : null;
    } catch (e) { return null; }
  }

  // ============ 书源：官方「小说拆解」弹窗增强（下载整本书入库）+ 顶部工具入口 ============
  // 官方 bundle 已内置完整搜书 UI（webdec__*：搜索→点书→选章→拆解），后端 IPC 全通。
  // 这里只做两件事：1) 拆解弹窗章节列表尾部注入「下载整本书」；
  //                  2) 顶部工具行（chathead__tools）补「自动写作」入口（与 风格蒸馏/小说拆解 同排）。
  const installBookTools = () => {
    const ipc = __wx_ipc_raw;   // F8 去重：原 IPC 通道收敛为共享实现

    // 当前书桥：书名从 chathead 标题读，id 用 list_books 匹配（bundle store 无法直接访问）
    window.__wx_current_book = async () => {
      const t = document.querySelector('.chathead__title');
      const name = t ? t.textContent.trim() : '';
      if (!name) return null;
      const books = (await ipc('list_books')) || [];
      return books.find((b) => (b.title || '').trim() === name) || null;
    };

    // ---- 1) 拆解弹窗：下载整本书 → 写入当前书「参考」组 ----
    const DL_BTN_ID = '__wx_dlbook';
    const ensureDlBtn = () => {
      const cat = document.querySelector('.modal--decompose .webdec__cathead, .mtool .mtool__cathead');
      if (!cat || document.getElementById(DL_BTN_ID)) return;
      const modal = cat.closest('.modal, .mtool');
      if (!modal) return;
      const bookLine = cat.querySelector('.webdec__bookline b, .mtool__bookline b');
      const bookName = bookLine ? bookLine.textContent.trim() : '';
      const chNodes = modal.querySelectorAll('.webdec__ch, .mtool__ch');
      if (!chNodes.length || !bookName) return;
      const btn = document.createElement('button');
      btn.id = DL_BTN_ID;
      btn.className = 'webdec__ghost';
      btn.type = 'button';
      btn.innerHTML =
        '<svg xmlns="http://www.w3.org/2000/svg" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" style="margin-right:5px"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>' +
        '下载整本书';
      btn.title = '抓取全部章节正文，存入本书资料库「参考」组';
      btn.addEventListener('click', async () => {
        btn.disabled = true;
        btn.textContent = '正在获取目录…';
        try {
          const sel = modal.querySelector('.webdec__select');
          const sourceId = sel ? sel.value : '';
          if (!sourceId) throw new Error('书源未选择');
          const hits = await ipc('search_books', { sourceId, keyword: bookName });
          const hit = (hits || []).find((h) => (h.name || h.title) === bookName) || (hits || [])[0];
          if (!hit) throw new Error('书源里找不到《' + bookName + '》');
          const cat2 = await ipc('fetch_book_catalog', { sourceId, bookUrl: hit.url });
          const all = (cat2 && cat2.chapters) || [];
          if (!all.length) throw new Error('目录为空');
          const parts = [];
          const BATCH = 20;
          for (let i = 0; i < all.length; i += BATCH) {
            const batch = all.slice(i, i + BATCH);
            btn.textContent = '正在抓取 ' + Math.min(i + BATCH, all.length) + '/' + all.length + ' 章…';
            try {
              const t = await ipc('fetch_chapter_texts', { sourceId, chapters: batch });
              if (t) parts.push(t);
            } catch (e) { /* 单批失败跳过 */ }
          }
          const full = parts.join('\n\n');
          if (!full.trim()) throw new Error('全部章节抓取失败');
          const cur = window.__wx_current_book ? await window.__wx_current_book() : null;
          if (!cur) throw new Error('未选择书籍（先在书架选一本书再下载）');
          // F9：与保存对话框同规则——文件名先过 safe_name 镜像；分组按服务端「细纲开头→细纲」
          // 纠偏规则预判（handlers/mod.rs write_file），展示去向与实际落盘一致；
          // 写入前查同名：write_file 会带版本快照覆盖旧稿，这里绝不静默覆盖。
          const fname = __wx_finalize_name(bookName + '（书源全文）');
          if (!fname) throw new Error('书名清洗后无法作为文件名，未写入');
          const effGroup = __wx_effective_group('参考', fname);
          const dlTree = await ipc('scan_tree', { bookId: cur.id });
          if (__wx_tree_has_file(dlTree, effGroup, fname)) {
            throw new Error(effGroup + ' / ' + fname + ' 已存在，未写入（不覆盖旧稿）；请先处理同名文件');
          }
          await ipc('write_file', { bookId: cur.id, group: effGroup, name: fname, content: full });
          btn.textContent = '已入库：' + effGroup + ' / ' + fname;
          btn.disabled = true;
        } catch (e) {
          btn.textContent = '下载失败：' + (e.message || e);
          btn.disabled = false;
        }
      });
      cat.appendChild(btn);
    };

    // ---- 2) 顶部工具行：自动写作按钮 ----
    const HEAD_BTN_ID = '__wx_auto_headbtn';
    const ensureHeadBtn = () => {
      const tools = document.querySelector('.chathead__tools');
      if (!tools || document.getElementById(HEAD_BTN_ID)) return;
      const b = document.createElement('button');
      b.id = HEAD_BTN_ID;
      b.className = 'chathead__toolbtn';
      b.type = 'button';
      b.title = '写作面板：章节范围 / 待审章节（接受后才转正到正文） / 智能体分工';
      b.innerHTML =
        '<svg xmlns="http://www.w3.org/2000/svg" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20h9"/><path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4Z"/></svg><span>写作</span>';
      b.addEventListener('click', () => {
        if (window.__wx_auto_open_studio) window.__wx_auto_open_studio();
      });
      tools.appendChild(b);
    };

    let btTimer = null;
    const ensureBt = () => {
      if (btTimer !== null) return;
      btTimer = setTimeout(() => { btTimer = null; ensureDlBtn(); ensureHeadBtn(); }, 150);
    };
    if (document.body) {
      new MutationObserver(ensureBt).observe(document.body, { childList: true, subtree: true });
      ensureDlBtn(); ensureHeadBtn();
    } else {
      document.addEventListener('DOMContentLoaded', () => {
        new MutationObserver(ensureBt).observe(document.body, { childList: true, subtree: true });
        ensureDlBtn(); ensureHeadBtn();
      });
    }
  };
  installBookTools();

  // ============ 自动写作 UI 套件（design-taste：单强调色 / SVG 图标 / 骨架·空态·错误态 / 触感反馈） ============
  const WB = (() => {
    const ICONS = {
      pen: '<path d="M12 20h9"/><path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4Z"/>',
      play: '<path d="M7 4.5v15l12-7.5Z"/>',
      stop: '<rect x="6" y="6" width="12" height="12" rx="2"/>',
      resume: '<path d="M3 12a9 9 0 1 0 3.3-6.9"/><path d="M3 4v5h5"/>',
      chevron: '<path d="M6 9l6 6 6-6"/>',
      check: '<path d="M20 6L9 17l-5-5"/>',
      x: '<path d="M18 6L6 18M6 6l12 12"/>',
      eye: '<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7Z"/><circle cx="12" cy="12" r="3"/>',
      inbox: '<path d="M22 12h-6l-2 3h-4l-2-3H2"/><path d="M5.5 5.1L2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.5-6.9a2 2 0 0 0-1.8-1.1H7.3a2 2 0 0 0-1.8 1.1Z"/>',
      users: '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.9"/><path d="M16 3.1a4 4 0 0 1 0 7.8"/>'
    };
    function icon(name, size) {
      const ns = 'http://www.w3.org/2000/svg';
      const svg = document.createElementNS(ns, 'svg');
      svg.setAttribute('viewBox', '0 0 24 24');
      svg.setAttribute('width', String(size || 14));
      svg.setAttribute('height', String(size || 14));
      svg.setAttribute('fill', 'none');
      svg.setAttribute('stroke', 'currentColor');
      svg.setAttribute('stroke-width', '2');
      svg.setAttribute('stroke-linecap', 'round');
      svg.setAttribute('stroke-linejoin', 'round');
      svg.innerHTML = ICONS[name] || '';
      return svg;
    }
    function ensureCss() {
      if (document.getElementById('__wx_wb_css')) return;
      const st = document.createElement('style');
      st.id = '__wx_wb_css';
      st.textContent = [
        '.wx-wb{--acc:var(--c-accent,var(--mln-accent,#C7433C));--ink:var(--mln-text,#18181b);--ink2:var(--mln-text2,#52525b);--ink3:var(--mln-text3,#a1a1aa);--line:var(--mln-border,#e4e4e7);--bg2:var(--mln-surface2,#fafafa);--surf:var(--mln-surface,#fff);color:var(--ink);font-size:12.5px}',
        '.wx-wb *{box-sizing:border-box}',
        '.wx-wb .h{font-size:13px;font-weight:600;letter-spacing:-.01em}',
        '.wx-wb .lbl{display:block;font-size:11px;font-weight:500;color:var(--ink2);margin-bottom:5px}',
        '.wx-wb .inp{width:100%;border:1px solid var(--line);border-radius:8px;padding:7px 10px;font-size:12.5px;color:var(--ink);background:var(--surf);outline:none;transition:border-color .2s cubic-bezier(.16,1,.3,1),box-shadow .2s cubic-bezier(.16,1,.3,1)}',
        '.wx-wb .inp:focus{border-color:var(--acc);box-shadow:0 0 0 3px rgba(199,67,60,.12)}',
        '.wx-wb .btn{display:inline-flex;align-items:center;justify-content:center;gap:5px;border:1px solid var(--line);background:var(--surf);color:var(--ink2);border-radius:8px;padding:7px 12px;font-size:12.5px;cursor:pointer;transition:background .18s cubic-bezier(.16,1,.3,1),border-color .18s cubic-bezier(.16,1,.3,1),color .18s cubic-bezier(.16,1,.3,1),transform .12s cubic-bezier(.16,1,.3,1)}',
        '.wx-wb .btn:hover{border-color:var(--mln-border-strong,#d4d4d8);color:var(--ink);background:var(--bg2)}',
        '.wx-wb .btn:active{transform:scale(.97)}',
        '.wx-wb .btn.pri{background:var(--acc);border-color:var(--acc);color:#fff;font-weight:500}',
        '.wx-wb .btn.pri:hover{background:var(--c-accent-hover,var(--mln-primary-hover,#B03A34));border-color:var(--c-accent-hover,var(--mln-primary-hover,#B03A34))}',
        '.wx-wb .btn.danger:hover{border-color:var(--c-accent,var(--mln-accent,#fca5a5));color:var(--c-accent,var(--mln-accent,#dc2626));background:var(--c-accent-bg,rgba(199,67,60,.08))}',
        '.wx-wb .btn:disabled{opacity:.4;pointer-events:none}',
        '.wx-wb .pill{display:inline-flex;align-items:center;gap:6px;font-size:11px;color:var(--ink2);background:var(--bg2);border:1px solid var(--line);border-radius:999px;padding:3px 9px}',
        '.wx-wb .dot{width:6px;height:6px;border-radius:50%;background:var(--ink3);flex:none}',
        '.wx-wb .dot.run{background:var(--mln-warn,#B8860B);animation:wxPulse 1.6s ease-in-out infinite}',
        '.wx-wb .dot.err{background:var(--mln-accent,#dc2626)}',
        '@keyframes wxPulse{0%,100%{opacity:1;transform:scale(1)}50%{opacity:.35;transform:scale(.75)}}',
        '.wx-wb .skel{height:56px;border-radius:10px;background:linear-gradient(90deg,var(--mln-surface2,#f4f4f5) 25%,var(--mln-border,#e9e9ec) 37%,var(--mln-surface2,#f4f4f5) 63%);background-size:400% 100%;animation:wxShim 1.4s ease infinite}',
        '@keyframes wxShim{0%{background-position:100% 50%}100%{background-position:0 50%}}',
        '.wx-wb .item{animation:wxIn .35s cubic-bezier(.16,1,.3,1) both}',
        '@keyframes wxIn{from{opacity:0;transform:translateY(6px)}to{opacity:1;transform:none}}',
        '.wx-wb .tip{font-size:11px;color:var(--ink3);min-height:14px;word-break:break-all;transition:color .2s}',
        '.wx-wb .tip.bad{color:var(--c-accent,var(--mln-accent,#dc2626))}',
        '.wx-wb .empty{display:flex;flex-direction:column;align-items:center;padding:20px 8px;color:var(--ink3);gap:1px}',
        '.wx-wb .empty svg{stroke:var(--mln-border-strong,#d4d4d8);margin-bottom:6px}',
        '.wx-wb .dd{position:relative}',
        '.wx-wb .dd__btn{width:100%;justify-content:space-between}',
        '.wx-wb .dd__val{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}',
        '.wx-wb .dd__pop{display:none;position:absolute;left:0;right:0;top:calc(100% + 4px);max-width:none;max-height:240px;overflow-y:auto;z-index:80;background:var(--surf);border:1px solid var(--line);border-radius:12px;box-shadow:var(--mln-sh-pop,0 12px 32px -8px rgba(24,24,27,.14));padding:6px;flex-direction:column}',
        '.wx-wb .dd__it{display:flex;align-items:center;gap:8px;padding:7px 8px;border-radius:8px;font-size:12.5px;color:var(--ink);cursor:pointer;transition:background .12s cubic-bezier(.16,1,.3,1)}',
        '.wx-wb .dd__it:hover{background:var(--bg2)}',
        '.wx-wb .dd__it.on{color:var(--acc);font-weight:600}',
        '.wx-wb .divide{border-top:1px solid var(--line)}',
        '.wx-wb .pv{display:none;margin-top:8px;max-height:300px;overflow:auto;white-space:pre-wrap;word-break:break-word;background:var(--bg2);border:1px solid var(--line);border-radius:10px;padding:10px;font-size:12px;line-height:1.7;color:var(--ink)}'
      ].join('');
      document.head.appendChild(st);
    }
    let openDd = null;
    document.addEventListener('click', (e) => {
      if (openDd && !(e.target.closest && e.target.closest('.dd'))) openDd();
    });
    function dropdown(placeholder) {
      const el = document.createElement('div');
      el.className = 'dd';
      const btn = document.createElement('button');
      btn.type = 'button'; btn.className = 'btn dd__btn';
      const val = document.createElement('span'); val.className = 'dd__val';
      const ar = icon('chevron', 12);
      ar.style.cssText = 'flex:none;color:var(--mln-text3,#a1a1aa);transition:transform .2s cubic-bezier(.16,1,.3,1)';
      btn.appendChild(val); btn.appendChild(ar);
      const pop = document.createElement('div'); pop.className = 'dd__pop';
      const state = { options: [], value: '', change: null };
      const render = () => {
        const hit = state.options.find((o) => o.value === state.value);
        val.textContent = hit ? hit.label : placeholder;
        val.style.color = hit ? '' : 'var(--mln-text3,#a1a1aa)';
      };
      const close = () => { pop.style.display = 'none'; ar.style.transform = ''; if (openDd === close) openDd = null; };
      const open = () => {
        if (openDd && openDd !== close) openDd();
        pop.innerHTML = '';
        state.options.forEach((o) => {
          const it = document.createElement('div');
          it.className = 'dd__it' + (o.value === state.value ? ' on' : '');
          it.textContent = o.label;
          it.addEventListener('click', () => {
            state.value = o.value; render(); close();
            if (state.change) state.change(o.value);
          });
          pop.appendChild(it);
        });
        const rc = el.getBoundingClientRect();
        pop.style.top = 'auto'; pop.style.bottom = 'auto';
        if (window.innerHeight - rc.bottom < 250) pop.style.bottom = 'calc(100% + 4px)';
        else pop.style.top = 'calc(100% + 4px)';
        pop.style.display = 'flex';
        ar.style.transform = 'rotate(180deg)';
        openDd = close;
      };
      btn.addEventListener('click', (e) => { e.stopPropagation(); if (pop.style.display === 'none') open(); else close(); });
      el.appendChild(btn); el.appendChild(pop);
      render();
      return {
        el,
        setOptions(o) { state.options = o; if (!o.some((x) => x.value === state.value)) state.value = ''; render(); },
        get value() { return state.value; },
        set value(v) { state.value = v; render(); },
        onChange(f) { state.change = f; }
      };
    }
    function btn(txt, iconName, cls) {
      const b = document.createElement('button');
      b.type = 'button'; b.className = 'btn' + (cls ? ' ' + cls : '');
      if (iconName) b.appendChild(icon(iconName, 13));
      const s2 = document.createElement('span'); s2.textContent = txt; b.appendChild(s2);
      return b;
    }
    return { ensureCss, icon, dropdown, btn };
  })();

  // —— 自动写作面板（共享构建器）：设置页与右侧栏复用，opts.compact 控制密度 ——
  const createAutoPanel = (opts) => {
    WB.ensureCss();
    const compact = !!opts.compact;
    const root = document.createElement('div');
    root.className = 'wx-wb';
    root.style.cssText = 'display:flex;flex-direction:column;gap:14px;';
    const gap = compact ? 10 : 14;

    // 头部：标题 + 状态胶囊（呼吸点）
    const head = document.createElement('div');
    head.style.cssText = 'display:flex;align-items:center;gap:8px;';
    const title = document.createElement('span');
    title.style.cssText = 'display:inline-flex;align-items:center;gap:7px;font-size:' + (compact ? 14 : 15) + 'px;font-weight:600;letter-spacing:-.01em;color:var(--mln-text,#18181b);';
    title.appendChild(WB.icon('pen', compact ? 15 : 17));
    title.appendChild(document.createTextNode('写作'));
    const modeHint = document.createElement('span');
    modeHint.style.cssText = 'font-size:11px;font-weight:400;color:var(--mln-text3,#a1a1aa);margin-left:4px;';
    modeHint.textContent = '默认写入「正文待审」，接受后转正到正文';
    title.appendChild(modeHint);
    const pill = document.createElement('span'); pill.className = 'pill';
    const dot = document.createElement('span'); dot.className = 'dot';
    const pillTxt = document.createElement('span');
    pill.appendChild(dot); pill.appendChild(pillTxt);
    head.appendChild(title); head.appendChild(pill);
    if (opts.onClose) {
      const closeB = document.createElement('span');
      closeB.style.cssText = 'margin-left:auto;display:inline-flex;align-items:center;gap:3px;font-size:11.5px;color:var(--mln-text3,#a1a1aa);cursor:pointer;transition:color .15s;';
      closeB.appendChild(document.createTextNode('收起'));
      closeB.appendChild(WB.icon('x', 12));
      closeB.addEventListener('mouseenter', () => { closeB.style.color = 'var(--mln-text2,#52525b)'; });
      closeB.addEventListener('mouseleave', () => { closeB.style.color = 'var(--mln-text3,#a1a1aa)'; });
      closeB.addEventListener('click', opts.onClose);
      head.appendChild(closeB);
    }
    root.appendChild(head);

    // 表单区：label 在上（Rule 6）
    const form = document.createElement('div');
    form.style.cssText = 'display:flex;flex-direction:column;gap:' + gap + 'px;';
    const bookWrap = document.createElement('div');
    const bookLbl = document.createElement('label'); bookLbl.className = 'lbl'; bookLbl.textContent = '书籍';
    const bookDd = WB.dropdown('选择书籍');
    bookWrap.appendChild(bookLbl); bookWrap.appendChild(bookDd.el);
    const rangeWrap = document.createElement('div');
    const rangeLbl = document.createElement('label'); rangeLbl.className = 'lbl'; rangeLbl.textContent = '章节范围';
    const rangeRow = document.createElement('div');
    rangeRow.style.cssText = 'display:grid;grid-template-columns:1fr auto 1fr;align-items:center;gap:8px;';
    const fromIn = document.createElement('input'); fromIn.type = 'number'; fromIn.min = '1'; fromIn.placeholder = '起'; fromIn.className = 'inp';
    const toIn = document.createElement('input'); toIn.type = 'number'; toIn.min = '0'; toIn.placeholder = '止'; toIn.className = 'inp';
    const arrow = document.createElement('span'); arrow.textContent = '→'; arrow.style.color = 'var(--mln-text3,#a1a1aa)';
    rangeRow.appendChild(fromIn); rangeRow.appendChild(arrow); rangeRow.appendChild(toIn);
    const hint = document.createElement('div'); hint.style.cssText = 'font-size:11px;color:var(--mln-text3,#a1a1aa);margin-top:4px;';
    hint.textContent = '起止留空 = 仅生成下一章；逐章待审模式每次只写 1 章';
    rangeWrap.appendChild(rangeLbl); rangeWrap.appendChild(rangeRow); rangeWrap.appendChild(hint);
    // fullAuto 显式开关（B 契约）：本次启动是否全自动直接定稿；默认关闭 = 新正文进「正文待审」。
    // 必须显式传给 auto_write_start，不允许后端回退到历史书级 flag。
    const autoWrap = document.createElement('label');
    autoWrap.style.cssText = 'display:flex;align-items:center;gap:7px;font-size:12px;color:var(--mln-text2,#52525b);cursor:pointer;';
    const autoChk = document.createElement('input');
    autoChk.type = 'checkbox';
    autoChk.style.cssText = 'width:14px;height:14px;accent-color:var(--c-accent,#C7433C);cursor:pointer;';
    const autoTxt = document.createElement('span');
    autoTxt.textContent = '本次全自动定稿（不勾选 = 新正文进「正文待审」等待接受）';
    autoWrap.appendChild(autoChk); autoWrap.appendChild(autoTxt);
    const saveModeWrap = document.createElement('label'); saveModeWrap.style.cssText = 'display:flex;align-items:center;gap:7px;font-size:12px;color:var(--mln-text2,#52525b);cursor:pointer;'; saveModeWrap.appendChild(document.createTextNode('保存模式：')); const saveModeSel = document.createElement('select'); [['ask', '询问后保存'], ['auto', '显式写作任务由后端自动入待审'], ['off', '仅预览，仍可手动保存']].forEach(p => { const o = document.createElement('option'); o.value = p[0]; o.textContent = p[0] + '=' + p[1]; saveModeSel.appendChild(o); }); saveModeWrap.appendChild(saveModeSel); // 三态 key=book_auto_save__+bookId（默认 ask）：auto 仅对显式写作任务由后端生效（产出自动入待审），普通聊天绝不因此自动写盘，前端绝不自动重复保存
    saveModeSel.addEventListener('change', async () => { const bid = bookDd.value; if (!bid) { tip('请先选择一本书', true); saveModeSel.value = 'ask'; return; } const v = saveModeSel.value; if (v === 'auto' && !window.confirm('auto：仅对显式写作任务生效，由后端自动把产出放入待审队列；普通聊天不会因此自动写盘，前端也绝不自动保存。确认选 auto 吗？')) { saveModeSel.value = 'ask'; return; } try { await invoke('set_setting', { key: 'book_auto_save__' + bid, value: v }); tip('已保存「保存模式」=' + v + '（' + bid + '）'); } catch (e) { tip('保存模式写入失败：' + (e.message || e), true); } });
    const actRow = document.createElement('div');
    actRow.style.cssText = 'display:flex;gap:8px;';
    const startB = WB.btn('开始', 'play', 'pri'); startB.style.flex = '1.4';
    const stopB = WB.btn('停止', 'stop'); stopB.style.flex = '1'; stopB.disabled = true;
    const resumeB = WB.btn('续跑', 'resume'); resumeB.style.flex = '1'; resumeB.disabled = true;
    actRow.appendChild(startB); actRow.appendChild(stopB); actRow.appendChild(resumeB);
    form.appendChild(bookWrap); form.appendChild(rangeWrap); form.appendChild(autoWrap); form.appendChild(saveModeWrap); form.appendChild(actRow);
    root.appendChild(form);

    // 分工区（仅设置页）
    let agentsSec = null;
    if (opts.agents) {
      agentsSec = document.createElement('div');
      agentsSec.className = 'divide';
      agentsSec.style.cssText = 'border-top:1px solid var(--mln-border,#e4e4e7);padding-top:' + gap + 'px;display:flex;flex-direction:column;gap:10px;';
      const agHead = document.createElement('div'); agHead.style.cssText = 'display:flex;align-items:baseline;gap:8px;';
      const agTitle = document.createElement('span'); agTitle.className = 'h';
      agTitle.style.cssText = 'display:inline-flex;align-items:center;gap:6px;';
      agTitle.appendChild(WB.icon('users', 14));
      agTitle.appendChild(document.createTextNode('智能体分工'));
      const agHint = document.createElement('span'); agHint.style.cssText = 'font-size:11px;color:var(--mln-text3,#a1a1aa);';
      agHint.textContent = '主模型负责对话与调度；下面五个角色分别执行对应阶段，留空则跟随当前渠道';
      agHead.appendChild(agTitle); agHead.appendChild(agHint);
      const agBox = document.createElement('div');
      agBox.style.cssText = 'display:flex;flex-direction:column;gap:8px;';
      agentsSec.appendChild(agHead); agentsSec.appendChild(agBox);
      root.appendChild(agentsSec);
    }

    // 记忆/连续性状态区（仅设置页）：真实读 memory_status，不做假状态。
    // 无接口或读取失败时明确显示「不可用」，绝不显示成「全部正常」。
    let memSec = null, memJobsEl = null, memStaleEl = null, memDepsEl = null, memRebuildB = null, memEffEl = null;
    if (opts.agents) {
      memSec = document.createElement('div');
      memSec.className = 'divide';
      memSec.style.cssText = 'border-top:1px solid var(--mln-border,#e4e4e7);padding-top:' + gap + 'px;display:flex;flex-direction:column;gap:8px;';
      const mHead = document.createElement('div'); mHead.style.cssText = 'display:flex;align-items:baseline;gap:8px;flex-wrap:wrap;';
      const mTitle = document.createElement('span'); mTitle.className = 'h';
      mTitle.style.cssText = 'display:inline-flex;align-items:center;gap:6px;';
      mTitle.appendChild(WB.icon('inbox', 14));
      mTitle.appendChild(document.createTextNode('章节记忆 / 连续性'));
      const mHint = document.createElement('span'); mHint.style.cssText = 'font-size:11px;color:var(--mln-text3,#a1a1aa);';
      mHint.textContent = '记忆抽取失败不影响定稿；可对失败/过期章重建';
      mHead.appendChild(mTitle); mHead.appendChild(mHint);
      const mk = (id) => { const d = document.createElement('div'); d.id = id; d.style.cssText = 'font-size:11.5px;line-height:1.7;color:var(--mln-text2,#52525b);word-break:break-word;'; return d; };
      memJobsEl = mk('__wx_mem_jobs'); memStaleEl = mk('__wx_mem_stale'); memDepsEl = mk('__wx_mem_deps');
      memEffEl = mk('__wx_mem_skills');
      memRebuildB = WB.btn('重建记忆', 'resume');
      memRebuildB.style.cssText = 'align-self:flex-start;padding:5px 10px;font-size:11.5px;';
      memSec.appendChild(mHead); memSec.appendChild(memEffEl); memSec.appendChild(memJobsEl);
      memSec.appendChild(memStaleEl); memSec.appendChild(memDepsEl); memSec.appendChild(memRebuildB);
      root.appendChild(memSec);
    }

    const tipEl = document.createElement('div'); tipEl.className = 'tip';
    root.appendChild(tipEl);

    // —— 行为逻辑（IPC 与旧版完全一致） ——
    const tip = (m, bad) => { tipEl.textContent = m || ''; tipEl.classList.toggle('bad', !!bad); };
    // fullAuto 选择读写走顶层的会话内存（wxFullAutoOf / wxSetFullAuto / wxResetFullAuto）
    let __lastStatus = '';
    let panelBusy = false;                    // 面板请求互斥：双击/连点不再发第二次
    let lastTaskBook = '';                    // 最近一次 paintStatus 里的任务所属书（停止按任务作用域）
    const bookLabels = {};                    // bookId -> 书名（跨书提示用）
    const setPanelBusy = (b) => {
      panelBusy = !!b;
      if (panelBusy) { startB.disabled = true; stopB.disabled = true; resumeB.disabled = true; }
      // 解除 busy 时具体可用性交给下一次 paintStatus 决定（不在此处猜 running/resumable）
    };
    const paintStatus = (st) => {
      st = st || {};
      const running = !!st.running;
      const s2 = running ? 'running' : st.status;
      // 任务刚结束（running→done/stopped/failed）：文件已落盘，刷新前端文件树（按任务所属书）
      if (__lastStatus === 'running' && s2 !== 'running' && s2) refreshReactTree({ bookId: st.bookId || bookDd.value || '' });
      __lastStatus = s2;
      let txt;
      if (running) {
        const p = (function () { for (const k of ['curCh', 'currentCh', 'current', 'cur']) if (st[k] != null) return st[k]; return '?'; })();
        const t = (function () { for (const k of ['totalCh', 'total', 'to']) if (st[k] != null) return st[k]; return '?'; })();
        txt = '生成中 · 第' + p + '/' + t + '章';
      } else if (s2 === 'done') txt = '已完成';
      else if (s2 === 'stopped') txt = '已暂停';
      else if (s2 === 'failed') txt = '已中断';
      else if (s2 === 'interrupted') txt = '已中断';
      else txt = ''; // 无任务时不显示状态占位（方案 §8）
      // 跨书任务提示：任务可能跑在别的书上。当前面板选中的书不是任务书时，明确提示归属，
      // 并把停止/续跑语义指向「任务所属书」，避免用户误以为在操作本书。
      const taskBook = st.bookId ? String(st.bookId) : '';
      lastTaskBook = taskBook;
      const otherBook = running && taskBook && bookDd.value && taskBook !== bookDd.value;
      if (otherBook) {
        const taskBookTitle = bookLabels[taskBook] || taskBook;
        txt = txt + ' · 任务在《' + taskBookTitle + '》';
        tip('当前任务属于《' + taskBookTitle + '》，不在本书。停止/续跑作用于该任务所属书。', false);
      }
      pillTxt.textContent = txt;
      pill.style.display = txt ? '' : 'none';
      dot.className = 'dot' + (running ? ' run' : (s2 === 'failed' || s2 === 'interrupted' ? ' err' : ''));
      // busy 期间统一禁用，避免双击/连点产生重复请求
      stopB.disabled = !running || panelBusy;
      resumeB.disabled = !st.resumable || panelBusy;
      startB.disabled = panelBusy;
      if (s2 === 'failed' && st.error) tip('写作中断：' + String(st.error) + '。已写内容已保留，可从断点续跑', true);
    };
    const latestChapterOf = __wx_latest_chapter_of;   // F8 去重：与 Dock 共用
    const tick = async () => {
      if (!bookDd.value) { pillTxt.textContent = '未选书籍'; return; }
      try {
        const st = await invoke('auto_write_status', { bookId: bookDd.value });
        // 跨书任务：当前书无任务，但全局有任务在别的书跑 → 也拉一次归属信息，避免完全看不到
        if (st && !st.running && !st.status) {
          try {
            const g = await invoke('auto_write_status', { bookId: '' });
            if (g && g.running) paintStatus(g);
            else paintStatus(st);
          } catch (e2) { paintStatus(st); }
        } else {
          paintStatus(st);
        }
      } catch (e) { pillTxt.textContent = '连接异常'; dot.className = 'dot err'; }
    };
    bookDd.onChange(async () => {
      autoChk.checked = wxFullAutoOf(bookDd.value);   // 切换书籍时同步该书的本次选择（仅本会话）
      invoke('get_settings', {}).then(s => { const cur = s && s['book_auto_save__' + bookDd.value]; saveModeSel.value = (cur === 'ask' || cur === 'auto' || cur === 'off') ? cur : 'ask'; }).catch(() => {}); // 切换书籍时同步该书持久化的「保存模式」，无效回落 ask
      await tick();
      if (opts.agents) { await renderMemory(); await renderEffectiveSkills(); }
    });
    autoChk.addEventListener('change', () => {
      wxSetFullAuto(bookDd.value, autoChk.checked);
      tip(autoChk.checked ? '本次将全自动定稿（审核通过才写入正文）；启动后自动复位' : '本次新正文进入「正文待审」，接受后转正');
    });
    startB.addEventListener('click', async () => {
      if (panelBusy) return;
      if (!bookDd.value) return tip('请先选择书籍', true);
      const fromCh = parseInt(fromIn.value, 10);
      let toCh = parseInt(toIn.value, 10);
      const latest = await latestChapterOf(bookDd.value);
      const start = isNaN(fromCh) || fromCh <= 0 ? latest + 1 : fromCh;
      if (isNaN(toCh) || toCh <= 0) toCh = start;
      const fa = !!autoChk.checked;
      if (!fa && toCh !== start) return tip('逐章待审模式每次只写 1 章；多章连写请显式勾选本次全自动定稿', true);
      if (toCh < start) return tip('止章不能小于起章', true);
      // 单次区间上限 200 章（与服务端一致）：按区间长度限制，避免绝对章号导致倒置范围
      if (toCh - start + 1 > 200) {
        return tip('单次最多 200 章，请把范围改为第 ' + start + '→' + (start + 199) + ' 章', true);
      }
      const bookName = bookLabels[bookDd.value] || bookDd.value;
      if (!wxConfirmWrite(bookName, start, toCh, fa)) return tip('已取消，未启动写作');
      setPanelBusy(true);
      tip('正在启动…', false);
      try {
        const sessionId = await wxSessionForBook(bookDd.value);
        await invoke('auto_write_start', { bookId: bookDd.value, sessionId, fromCh: start, toCh, fullAuto: fa, confirmed: true, confirmAuto: fa });
        // 启动后立刻复位：本次选择只对这一次 start 生效，避免隔日/再次启动默认继承全自动
        wxResetFullAuto(bookDd.value);
        autoChk.checked = false;
        tip('已启动：第' + start + '→' + toCh + '章' + (fa ? '（全自动定稿）' : '（正文待审）'));
        await tick();
      } catch (e) { tip('启动失败：' + (e.message || e), true); }
      finally { setPanelBusy(false); await tick().catch(() => {}); }
    });
    stopB.addEventListener('click', async () => {
      if (panelBusy) return;
      setPanelBusy(true);
      try {
        // 停止按任务作用域：带上「当前任务所属书」（跨书时仍指向真正在跑的任务，绝不误停别的书）
        // 忙碌中(panelBusy)已提前 return，不会跨书重复 stop。
        await invoke('auto_write_stop', { bookId: lastTaskBook || bookDd.value || '' });
        tip('已请求停止');
        await tick();
      } catch (e) { tip('停止失败：' + (e.message || e), true); }
      finally { setPanelBusy(false); await tick().catch(() => {}); }
    });
    resumeB.addEventListener('click', async () => {
      if (panelBusy) return;
      if (!bookDd.value) return tip('请先选择书籍', true);
      setPanelBusy(true);
      try {
        const task = await invoke('auto_write_last_task', { bookId: bookDd.value });
        const range = task && task.fromCh ? '第' + task.fromCh + '—' + task.toCh + '章' : '未知范围';
        const fullAuto = task && (task.fullAuto === 1 || task.fullAuto === true);
        const mode = fullAuto ? '全自动定稿（审核通过后直接写正文）' : '逐章待审';
        if (!window.confirm('确认续跑本书任务（' + range + '）？\n模式：' + mode + '。不会覆盖已有稿。')) return;
        if (fullAuto && !window.confirm('再次确认：本次续跑仍会自动定稿，期间不逐章询问。确定继续吗？')) return;
        await invoke('auto_write_resume', { bookId: bookDd.value, confirmed: true, confirmAuto: !!fullAuto }); tip('已续跑'); await tick();
      }
      catch (e) { tip('续跑失败：' + (e.message || e), true); }
      finally { setPanelBusy(false); await tick().catch(() => {}); }
    });
    // 分工渲染（设置页）
    const renderAgents = (data) => {
      if (!agentsSec) return;
      const box = agentsSec.lastElementChild;
      box.innerHTML = '';
      const profiles = (data && data.profiles) || {};
      const chans = (data && data.channels) || [];
      const ROLE_LABELS = { distill: '拆书/蒸馏', outline: '结构/大纲', chapter: '正文写作', review: '审核/去味', summary: '总结/标题' };
      const chOpts = [{ value: '', label: '跟随当前渠道' }].concat(
        chans.map((c) => ({ value: c.id, label: (c.label || c.id) + ':' + (c.model || '') }))
      );
      for (const task of ['distill', 'outline', 'chapter', 'review', 'summary']) {
        const prof = profiles[task] || {};
        const row = document.createElement('div');
        row.style.cssText = 'display:grid;grid-template-columns:64px 1fr 1.2fr auto;align-items:center;gap:8px;';
        const nm = document.createElement('span'); nm.textContent = ROLE_LABELS[task];
        nm.style.cssText = 'font-size:12px;font-weight:600;color:var(--mln-text,#18181b);';
        const dd = WB.dropdown('跟随当前渠道');
        dd.setOptions(chOpts); dd.value = prof.channelId || '';
        const mi = document.createElement('input'); mi.className = 'inp'; mi.type = 'text';
        mi.placeholder = '模型留空=渠道默认'; mi.value = prof.model || '';
        mi.style.padding = '5px 8px'; mi.style.fontSize = '12px';
        const sv = WB.btn('保存'); sv.style.cssText = 'padding:5px 10px;font-size:11.5px;';
        sv.addEventListener('click', async () => {
          try {
            await invoke('set_agent_profile', { task: task, channelId: dd.value, model: mi.value.trim() });
            tip('已保存「' + ROLE_LABELS[task] + '」分工');
          } catch (e) { tip('保存分工失败：' + (e.message || e), true); }
        });
        row.appendChild(nm); row.appendChild(dd.el); row.appendChild(mi); row.appendChild(sv);
        box.appendChild(row);
      }
    };

    // ---- 真实记忆状态渲染（memory_status）----
    let memBusy = false;
    const renderMemory = async () => {
      if (!memSec) return;
      const bid = bookDd.value;
      if (!bid) { memJobsEl.textContent = '记忆状态：未选书籍'; memStaleEl.textContent = ''; memDepsEl.textContent = ''; return; }
      let st = null;
      try { st = await invoke('memory_status', { bookId: bid }); }
      catch (e) {
        // 接口不存在或失败：明确不可用，不显示成「无问题」
        memJobsEl.textContent = '记忆状态：不可用（' + (e.message || e) + '）';
        memStaleEl.textContent = ''; memDepsEl.textContent = '';
        memRebuildB.disabled = true;
        return;
      }
      st = st || {};
      const jobs = st.jobs || [];
      const mems = st.memories || [];
      const deps = st.draftDependencies || [];
      const failed = jobs.filter((j) => j.status === 'failed');
      const pending = jobs.filter((j) => j.status !== 'done' && j.status !== 'failed');
      const stale = mems.filter((m) => m.status === 'stale');
      const staleDeps = deps.filter((d) => d.status === 'stale');
      memJobsEl.textContent = '记忆任务：失败 ' + failed.length + ' · 待处理 ' + pending.length + ' · 共 ' + jobs.length
        + (failed.length ? '（失败章：' + failed.slice(0, 6).map((j) => '第' + j.ch + '章').join('、') + (failed.length > 6 ? ' 等' : '') + '）' : '');
      memJobsEl.style.color = failed.length ? 'var(--c-accent,var(--mln-accent,#dc2626))' : 'var(--mln-text2,#52525b)';
      memStaleEl.textContent = '过期记忆：' + stale.length
        + (stale.length ? '（章：' + stale.slice(0, 6).map((m) => '第' + m.ch + '章').join('、') + (stale.length > 6 ? ' 等' : '') + '）' : '');
      memStaleEl.style.color = stale.length ? 'var(--c-accent,var(--mln-accent,#dc2626))' : 'var(--mln-text2,#52525b)';
      memDepsEl.textContent = '待审依赖失效：' + staleDeps.length
        + (staleDeps.length ? '（章：' + staleDeps.slice(0, 6).map((d) => '第' + d.ch + '章').join('、') + (staleDeps.length > 6 ? ' 等' : '') + '，需重新审核）' : '');
      memDepsEl.style.color = staleDeps.length ? 'var(--c-accent,var(--mln-accent,#dc2626))' : 'var(--mln-text2,#52525b)';
      memRebuildB.disabled = memBusy;
    };
    // 本次实际生效的技能（C effective_skills 只读接口；无接口时明确说明而不是假装有）
    const renderEffectiveSkills = async () => {
      if (!memEffEl) return;
      const bid = bookDd.value;
      if (!bid) { memEffEl.textContent = '本次生效技能：未选书籍'; return; }
      const tasks = ['outline', 'chapter', 'summary', 'review'];
      const parts = [];
      for (const t of tasks) {
        try {
          const r = await invoke('resolve_effective_skills', { bookId: bid, task: t });
          const arr = (r && (r.skills || r.items || r)) || [];
          const names = (Array.isArray(arr) ? arr : []).map((x) => (typeof x === 'string' ? x : (x && (x.name || x.id)) || '')).filter(Boolean);
          parts.push(t + '：' + (names.length ? names.join('、') : '无'));
        } catch (e) {
          memEffEl.textContent = '本次生效技能：不可用（后端未提供 resolve_effective_skills）';
          memEffEl.style.color = 'var(--mln-text3,#a1a1aa)';
          return;
        }
      }
      memEffEl.textContent = '本次生效技能 — ' + parts.join('；');
      memEffEl.style.color = 'var(--mln-text2,#52525b)';
    };
    if (memRebuildB) {
      memRebuildB.addEventListener('click', async () => {
        if (memBusy) return;                       // 忙中防重复
        const bid = bookDd.value;
        if (!bid) return tip('请先选择书籍', true);
        memBusy = true; memRebuildB.disabled = true;
        const old = memRebuildB.textContent;
        memRebuildB.textContent = '重建中…';
        try {
          await invoke('rebuild_memory', { bookId: bid });
          tip('已提交重建：失败/过期章会逐章重跑，可在上方查看进度');
        } catch (e) { tip('重建失败：' + (e.message || e), true); }
        finally { memBusy = false; memRebuildB.disabled = false; memRebuildB.textContent = old; await renderMemory(); }
      });
    }

    const loadAll = async () => {
      try {
        const books = await invoke('list_books');
        bookDd.setOptions((books || []).map((b) => ({ value: b.id, label: b.title || b.id })));
        for (const b of (books || [])) bookLabels[b.id] = b.title || b.id;   // 跨书任务提示用
        if (!bookDd.value && books && books.length) bookDd.value = books[0].id;
        autoChk.checked = wxFullAutoOf(bookDd.value);   // 显式开关回显该书「本次」选择（仅本会话；start 后已复位）
      } catch (e) { tip('书籍列表加载失败：' + (e.message || e), true); }
      if (opts.agents) {
        try { renderAgents((await invoke('get_agent_profiles', {})) || {}); }
        catch (e) { tip('分工配置加载失败：' + (e.message || e), true); }
        await renderMemory();
        await renderEffectiveSkills();
      }
      await tick();
    };
    return { el: root, loadAll, tick, refreshMemory: (memSec ? renderMemory : null), refreshSkills: (memEffEl ? renderEffectiveSkills : null) };
  };

  // ============ 文件树刷新助手（共享） ============
  // 背景：React 的文件树只在「选书」时重新 scan_tree。AI 写盘（对话落盘 / 自动写作 / 审批转正）
  // 之后树不刷新，用户会以为「没写进去」。store 未暴露到 window，只能通过重选当前书触发重扫。
  // 保护：编辑器打开时不打扰（用户正在看稿），按 bookId 记录待刷新，退出编辑器/回到对话视图时再消费。
  // F16：dirty 从「一个全局布尔」改为「按 bookId 的集合」——切书后不会丢掉上一本书的待刷新，
  // 也不会在别的书上误刷；退出编辑器时由 __wx_consume_tree_dirty 消费，不再依赖模拟切书保存副作用。
  let __wx_tree_timer = null;
  const __wx_tree_dirty = new Set();          // 待刷新 bookId 集合；'' = 未绑定书的通用刷新
  const __wx_book_id_now = () => (wxCurrentBook() || '');
  const refreshReactTree = (opts) => {
    try {
      const force = !!(opts && opts.force);
      const bid = (opts && opts.bookId) || __wx_book_id_now() || '';
      const editorOpen = !!document.querySelector('.centerstage .editorpane.is-open');
      if (editorOpen && !force) { __wx_tree_dirty.add(bid); return; }
      __wx_tree_dirty.delete(bid);

      // 书卡在「切换作品」下拉里，默认不在 DOM：先找；找不到就展开下拉并轮询等书卡出现。
      const findCard = () => document.querySelector('.bookcard.is-active .bookcard__main')
        || document.querySelector('.bookcard__main');
      const card = findCard();
      if (card) { card.click(); return; }   // 下拉本来就开着
      const opener = document.querySelector('.librail__bookmain');
      if (!opener) return;
      opener.click();                        // 展开（此分支下必为关闭态）
      if (__wx_tree_timer) clearInterval(__wx_tree_timer);
      let tries = 0;
      __wx_tree_timer = setInterval(() => {
        tries += 1;
        const c = findCard();
        if (c) {
          clearInterval(__wx_tree_timer); __wx_tree_timer = null;
          c.click();
          setTimeout(() => { try { opener.click(); } catch (e) {} }, 650);  // 收起，恢复原样
        } else if (tries >= 12) {            // 最多等 ~1.8s
          clearInterval(__wx_tree_timer); __wx_tree_timer = null;
          try { opener.click(); } catch (e) {}                              // 没等到也收起
        }
      }, 150);
    } catch (e) { /* 静默：刷新失败不影响写作 */ }
  };
  window.__wx_refresh_tree = refreshReactTree;

  // F16：退出编辑器时消费 dirty（编辑器关闭 / 从编辑视图回到对话视图）。
  // 既保留「编辑器内不打扰」，又不丢新稿：只要 dirty 集合非空且编辑器确实已关闭，就补一次刷新。
  //
  // 触发策略（父代理要求：timeout0 + 有限重试 + 按书，不能用固定长延时提前消费）：
  // 1) 捕获阶段委托点击「返回」按钮 → setTimeout(0) 后再检查；
  // 2) 若编辑器此时仍开着，则在有界次数内用短间隔重试（最多 20×50ms = 1s），
  //    一旦编辑器关闭立即消费，绝不早于编辑器关闭；
  // 3) body MutationObserver 只作兜底，且同样以「编辑器已关闭」为硬条件。
  let __wx_dirty_watch = null;
  const __wx_dirty_retry = new Map();          // bookId -> 剩余重试次数（按书独立，避免互相打断）
  const __wx_editor_open = () => !!document.querySelector('.centerstage .editorpane.is-open');
  const __wx_consume_tree_dirty = (onlyBid) => {
    try {
      if (__wx_tree_dirty.size === 0) return 0;
      if (__wx_editor_open()) return 0;        // 硬条件：编辑器没关绝不消费
      const targets = onlyBid ? [onlyBid].filter((b) => __wx_tree_dirty.has(b)) : Array.from(__wx_tree_dirty);
      if (!targets.length) return 0;
      // 一次刷新即可覆盖同一棵书树；按书清理，避免误清其它书的 dirty
      for (const b of targets) {
        refreshReactTree({ force: true, bookId: b });
        __wx_tree_dirty.delete(b);
        __wx_dirty_retry.delete(b);
      }
      return targets.length;
    } catch (e) { return 0; }
  };
  window.__wx_consume_tree_dirty = __wx_consume_tree_dirty;
  // 编辑器关闭后的有界重试：先 setTimeout(0) 让 React 完成这次状态提交，再最多重试 20 次 × 50ms
  const __wx_schedule_consume = (bid) => {
    const key = bid || __wx_book_id_now() || '';
    if (__wx_dirty_retry.has(key)) return;
    __wx_dirty_retry.set(key, 20);
    const step = () => {
      const left = __wx_dirty_retry.get(key);
      if (left == null) return;
      if (__wx_editor_open()) {
        if (left <= 0) { __wx_dirty_retry.delete(key); return; }
        __wx_dirty_retry.set(key, left - 1);
        setTimeout(step, 50);                  // 有界等待编辑器真正关闭
        return;
      }
      __wx_dirty_retry.delete(key);
      __wx_consume_tree_dirty(key);
    };
    setTimeout(step, 0);
  };
  // 捕获阶段委托：点「返回」/关闭编辑器时排程（不阻断 React 自身处理）
  document.addEventListener('click', (e) => {
    try {
      const t = e.target;
      if (!t || !t.closest) return;
      if (t.closest('.editorpane__back')) __wx_schedule_consume(__wx_book_id_now());
    } catch (err) {}
  }, true);
  if (document.body) {
    __wx_dirty_watch = new MutationObserver(() => {
      if (__wx_tree_dirty.size === 0) return;
      if (document.hidden) return;
      __wx_consume_tree_dirty();               // 兜底：同样要求编辑器已关闭
    });
    __wx_dirty_watch.observe(document.body, { childList: true, subtree: true, attributes: true, attributeFilter: ['class'] });
  }

  // ============ 写回原文：选区改写对话的落盘闭环 ============
  // 对话产出若不满足整章落盘契约（如选区改写只输出改写段落），服务端会发 save_skipped。
  // 这里给最后一条 AI 消息挂「写回原文」按钮：把引用区间替换为本次产出，生成变更提案，
  // 由提案审阅按 CAS 审批写入——不新增任何绕过审批的写盘通道。
  var __wx_toast_el = null, __wx_toast_timer = null;
  function __wx_toast(msg, ms) {
    if (!__wx_toast_el) {
      __wx_toast_el = document.createElement('div');
      __wx_toast_el.id = '__wx_toast';
      __wx_toast_el.style.cssText = 'position:fixed;left:50%;bottom:86px;transform:translateX(-50%);z-index:2147483400;' +
        'max-width:78vw;padding:9px 14px;border-radius:10px;background:rgba(24,24,27,.92);color:#fff;font-size:12.5px;' +
        'line-height:1.55;box-shadow:0 6px 24px rgba(0,0,0,.28);pointer-events:none;white-space:pre-wrap;font-family:var(--font-ui,inherit)';
      __wx_toast_el.style.display = 'none';
      document.body.appendChild(__wx_toast_el);
    }
    __wx_toast_el.textContent = msg;
    __wx_toast_el.style.display = 'block';
    if (__wx_toast_timer) clearTimeout(__wx_toast_timer);
    __wx_toast_timer = setTimeout(function () { __wx_toast_el.style.display = 'none'; }, ms || 4200);
  }

  function __wx_parse_quote(message) {
    var m = /^【引用选区(?: · ([^\]】]+))?】\n([\s\S]*?)\n\n【我的要求】/.exec(message || '');
    if (!m) return null;
    return { name: (m[1] || '').trim(), text: m[2] };
  }

  function __wx_handle_chat_stream(init, txt, url) {
    if (url.indexOf('/ipc/chat_stream') < 0 || !txt) return;
    var raw = '', skipped = null, saved = false, savedFiles = [], message = '', errReason = '', interruptedEv = false;
    var source = {}; try { source = JSON.parse((init && init.body) || '{}').args || {}; message = source.message || ''; } catch (e) {}
    var lines = txt.split(/\r?\n/);
    for (var i = 0; i < lines.length; i++) {
      var line = lines[i];
      if (!line || line.charAt(0) !== '{') continue;
      var o = null;
      try { o = JSON.parse(line); } catch (e) { continue; }
      var ev = o && o.e;
      if (ev && ev.type === 'delta' && typeof ev.text === 'string') raw += ev.text;
      else if (ev && ev.type === 'done' && typeof ev.full === 'string') { raw = ev.full; source.messageId = ev.messageId; }
      else if (ev && ev.type === 'save_skipped') skipped = ev.reasons || [];
      else if (ev && ev.type === 'saved') { saved = true; savedFiles = ev.files || []; }
      else if (ev && ev.type === 'error') errReason = ev.message || '';
      else if (ev && ev.type === 'cont') __wx_toast('输出触长度上限，自动续写中（第 ' + (ev.n || 1) + ' 次）…', 4200);
      else if (ev && ev.type === 'interrupted') interruptedEv = true;
    }
    if (saved && savedFiles.length) {
      // 落盘去向如实播报：正文任务写的是「正文待审」而非正式正文；并挂接「保留/丢弃」选择条
      __wx_toast('已写入（待审稿，未进正式正文）：' + savedFiles.join('；') + '，可在消息下方选择保留或丢弃', 5200);
      var __sc_book = (function () {
        try {
          var b = init && init.body ? JSON.parse(init.body) : null;
          return (b && b.args && b.args.bookId) || wxCurrentBook() || '';
        } catch (e) { return wxCurrentBook() || ''; }
      })();
      setTimeout(function () {
        var fs2 = savedFiles;
        __wx_attach_retry(function () { return __wx_attach_save_choice(fs2, __sc_book, source); });
      }, 600);
    }
    // 折叠显示：长产出不刷屏对话框，卡片展示字数与落盘去向，可展开/收起
    if (raw.trim().length > 600) {
      setTimeout(function () {
        var ch2 = raw.trim().length, df = savedFiles;
        __wx_attach_retry(function () { return __wx_collapse_long_output(ch2, df, source); });
      }, 600);
    }
    if (skipped && skipped.length) __wx_toast('未自动落盘：' + skipped.join('；'), 5200);
    if (interruptedEv) {
      __wx_toast('生成被中断，未自动落盘：' + (errReason || '手动停止 / 网络 / 思考超时'), 6500);
      // 中断恢复：带章节标题的较长残稿，允许人工一键存入正文待审（走审批队列）
      if (raw.trim().length >= 400) {
        setTimeout(function () {
          var rw = raw.trim();
          var mch = /第\s*([一二三四五六七八九十百零两\d]+)\s*章/.exec(message || '');
          __wx_attach_retry(function () { return __wx_attach_partial_save(rw, mch ? mch[1] : '', source); });
        }, 600);
      }
      __wx_schedule_dir_save(raw, init, source);   // 残稿同样提供写前「保存到书籍目录」入口
      return;
    }
    var quote = __wx_parse_quote(message);
    if (quote && quote.name && quote.text && raw.trim() && !saved) {
      setTimeout(function () {
        var rw = raw.trim(), q2 = quote;
        __wx_attach_retry(function () { return __wx_attach_writeback(rw, q2, source); });
      }, 600);
    }
    // 写前「保存到书籍目录」入口：已自动落盘（saved）与未落盘（含 save_skipped）都提供，
    // 仅挂按钮不写盘；与上方「保留/丢弃」条（事后补救）语义不同，不互相冒充。
    __wx_schedule_dir_save(raw, init, source);
  }

  function __wx_source_body(source) {
    if (!source || !source.messageId || source.bookId !== wxCurrentBook() || source.sessionId !== window.__molanCurrentSessionId?.()) return null;
    return document.querySelector('[data-message-id="' + CSS.escape(source.messageId) + '"] .aimsg__body');
  }
  function __wx_attach_retry(fn) {
    var n = 0;
    (function tick() {
      var ok = false;
      try { ok = fn(); } catch (e) { ok = false; }
      if (ok || ++n >= 10) return;
      setTimeout(tick, 800);
    })();
  }

  function __wx_collapse_long_output(chars, dests, source) {
    var body = __wx_source_body(source);
    if (!body) return false;
    if (body.querySelector('.__wx_fold')) return true;
    var prose = body.querySelector('.aimsg__prose') || body;
    var card = document.createElement('div');
    card.className = '__wx_fold';
    card.style.cssText = 'margin:6px 0;padding:8px 12px;border:1px solid var(--c-border-3,#EFEBE3);border-radius:10px;' +
      'background:var(--c-bg-subtle,#F8F7F4);font-size:12.5px;color:var(--c-text-secondary,#5C6470);' +
      'display:flex;gap:10px;align-items:center;flex-wrap:wrap;font-family:var(--font-ui,inherit)';
    var info = document.createElement('span');
    info.textContent = '📄 已生成 ' + chars + ' 字' + (dests && dests.length ? ' · 已写入 ' + dests.join('；') + '（待审稿，未进正式正文）' : '');
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.textContent = '展开全文';
    btn.style.cssText = 'border:1px solid var(--c-border-3,#EFEBE3);background:var(--c-white,#fff);border-radius:6px;' +
      'padding:2px 8px;font-size:12px;cursor:pointer;color:var(--c-text-secondary,#5C6470)';
    var open = false;
    prose.style.display = 'none';
    btn.addEventListener('click', function () {
      open = !open;
      prose.style.display = open ? '' : 'none';
      btn.textContent = open ? '收起全文' : '展开全文';
    });
    card.appendChild(info);
    card.appendChild(btn);
    body.insertBefore(card, prose);
    return true;
  }

  function __wx_attach_partial_save(raw, chHint, source) {
    var bubble = __wx_source_body(source);
    var host = bubble ? (bubble.parentNode || bubble) : null;
    if (!host) return false;
    if (host.querySelector('.__wx_pb')) return true;
    var wrap = document.createElement('div');
    wrap.className = '__wx_pb';
    wrap.style.cssText = 'margin:6px 0 2px;display:flex;gap:8px;align-items:center';
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.textContent = '⚠ 存残缺稿到正文待审';
    btn.title = '把已生成部分存入正文待审并登记审批队列（留版本快照，不直接进正文）';
    btn.style.cssText = 'border:1px solid var(--c-border-3,#EFEBE3);background:var(--c-white,#fff);color:var(--c-text-secondary,#5C6470);' +
      'border-radius:8px;padding:4px 10px;font-size:12px;cursor:pointer;font-family:var(--font-ui,inherit)';
    btn.addEventListener('click', function () {
      var bookId = source.bookId || '';
      if (!bookId) { __wx_toast('存稿失败：未关联当前书籍'); return; }
      btn.disabled = true; btn.textContent = '存稿中…';
      invoke('save_partial_as_review', { bookId: bookId, content: raw, ch: chHint || '' }).then(function (res) {
        if (res && res.ok) {
          btn.textContent = '✓ 已存正文待审 第' + res.ch + '章，待审阅';
          __wx_toast('残缺稿已存入正文待审并登记审批队列，请在审批队列审阅', 6000);
          try { refreshReactTree({ bookId: bookId }); } catch (e) {}
        } else {
          throw new Error('存稿返回异常');
        }
      }).catch(function (e) {
        __wx_toast('存稿失败：' + ((e && e.message) ? e.message : String(e)), 5200);
        btn.disabled = false; btn.textContent = '⚠ 存残缺稿到正文待审';
      });
    });
    wrap.appendChild(btn);
    host.appendChild(wrap);
    return true;
  }

  // ============ 落盘选择条：自动落盘后给用户「保留 / 丢弃」选择（最小前端闭环） ============
  // 背景：对话产出的落盘决策在服务端（chat_stream 无 noSave/saveTarget 参数，前端拦不住写入）。
  // 正文任务已自动写入「正文待审」并登记审批队列，细纲任务直接写入「细纲」组；本条只做事后选择：
  //   保留 = 维持现状（待审队列里可继续预览/接受/拒绝）；丢弃 = reject_chapter / delete_file，
  // 后端两者均为回收站式删除（files.rs delete_file 先写回收站再删源文件），可恢复。
  // 绝不新增任何写正式正文的通道，也不触发任何生成/续写。
  function __wx_attach_save_choice(files, bookId, source) {
    var rows = [];
    for (var i = 0; i < files.length; i++) {
      var s = String(files[i] || '');
      var k = s.indexOf(' / ');
      if (k > 0) rows.push({ group: s.slice(0, k).trim(), name: s.slice(k + 3).trim() });
    }
    // 仅对「正文待审 / 细纲」提供选择；其它分组（如技能广场）没有可撤回语义，不展示
    var actionable = rows.filter(function (r) { return r.group === '正文待审' || r.group === '细纲'; });
    if (!actionable.length || !bookId) return false;
    var bubble = __wx_source_body(source);
    var host = bubble ? (bubble.parentNode || bubble) : null;
    if (!host) return false;
    if (host.querySelector('.__wx_sc')) return true;
    var wrap = document.createElement('div');
    wrap.className = '__wx_sc';
    wrap.style.cssText = 'margin:6px 0 2px;display:flex;flex-direction:column;gap:4px';
    var head = document.createElement('div');
    head.style.cssText = 'font-size:11.5px;color:var(--c-text-muted,#9AA1AC)';
    head.textContent = '已自动落盘为待审稿（未进正式正文）。保留 = 稍后在审批队列处理；丢弃 = 移入回收站（可恢复）：';
    wrap.appendChild(head);
    var busyRow = false;
    var dismiss = function () { if (wrap.parentNode) wrap.parentNode.removeChild(wrap); };
    rows.forEach(function (r) {
      var row = document.createElement('div');
      row.className = '__wx_sc_row';
      row.style.cssText = 'display:flex;gap:8px;align-items:center;flex-wrap:wrap';
      var label = document.createElement('span');
      label.textContent = '「' + r.group + ' / ' + r.name + '」';
      label.style.cssText = 'font-size:12px;color:var(--c-text-secondary,#5C6470)';
      row.appendChild(label);
      if (r.group !== '正文待审' && r.group !== '细纲') return;
      var mkBtn = function (txt, fn) {
        var b = document.createElement('button');
        b.type = 'button';
        b.textContent = txt;
        b.style.cssText = 'border:1px solid var(--c-border-3,#EFEBE3);background:var(--c-white,#fff);color:var(--c-text-secondary,#5C6470);border-radius:8px;padding:3px 10px;font-size:12px;cursor:pointer;font-family:var(--font-ui,inherit)';
        b.addEventListener('click', function () { if (!busyRow) fn(b); });
        return b;
      };
      row.appendChild(mkBtn('保留', function (b) {
        b.textContent = '✓ 已保留（待审）';
        b.disabled = true;
        row.querySelectorAll('button').forEach(function (x) { if (x !== b) x.disabled = true; });
        setTimeout(dismiss, 2600);
      }));
      row.appendChild(mkBtn('丢弃', function (b) {
        var ok = window.confirm('确定丢弃「' + r.group + ' / ' + r.name + '」吗？\n文件会移入回收站，可在回收站恢复。');
        if (!ok) return;
        busyRow = true;
        b.disabled = true; b.textContent = '丢弃中…';
        var chNum = (function () { var m = /第(\d+)章/.exec(r.name); return m ? parseInt(m[1], 10) : 0; })();
        var req = (r.group === '正文待审')
          ? invoke('reject_chapter', { bookId: bookId, ch: chNum, name: r.name })
          : invoke('delete_file', { bookId: bookId, group: r.group, name: r.name });
        req.then(function () {
          __wx_toast('已丢弃并移入回收站：' + r.group + ' / ' + r.name, 4600);
          try { refreshReactTree({ bookId: bookId }); } catch (e) {}
          row.remove();
          if (!wrap.querySelector('.__wx_sc_row')) dismiss();
        }).catch(function (e) {
          __wx_toast('丢弃失败：' + ((e && e.message) ? e.message : String(e)), 5200);
          b.disabled = false; b.textContent = '丢弃';
        }).then(function () { busyRow = false; });
      }));
      wrap.appendChild(row);
    });
    host.appendChild(wrap);
    return true;
  }

  // ============ 用户主动「保存到书籍目录」：写前选组 / 命名 / 预览 / 可取消，严格不覆盖 ============
  // 语义澄清：服务端 chat_stream 的自动落盘由后端决定（前端拦不住）；本按钮是显式「把这条 AI 回复另存为书籍目录新文件」，写前选定分组/文件名并二次确认后才写盘，取消不写任何文件；与自动落盘后的事后「保留/丢弃」条（__wx_attach_save_choice）互不冒充。
  // 预检仅供提示（scan_tree 不含正文待审组，可能不全）；最终由 save_chat_output 单次原子只新建写入，禁止正式正文和覆盖，服务端仍会拒绝同名目标。
  var __WX_SAVE_GROUPS = ['设定', '细纲', '参考', '正文待审'];
  var __wx_book_auto_save_mode = function (bid) { return invoke('get_settings', {}).then(function (s) { var v = s && s['book_auto_save__' + bid]; return (v === 'ask' || v === 'auto' || v === 'off') ? v : 'ask'; }).catch(function () { return 'ask'; }); }; // 保存模式读取 helper（只读，前端绝不自动保存）：key=book_auto_save__+bookId，默认 ask；ask=询问后保存 auto=显式写作任务由后端自动入待审 off=仅预览仍可手动保存

  // 镜像 molan-core files.rs safe_name：弹窗里显示的最终文件名 == 实际落盘文件名（不多不少）
  function __wx_safe_name(n) {
    var cleaned = String(n == null ? '' : n).replace(/[<>:"/\\|?*\u0000-\u001f]/g, '_');
    var t = cleaned.trim().replace(/^\.+/, '').replace(/\.+$/, '');
    return t;
  }

  // 镜像服务端 write_file IPC 的分组纠偏（handlers/mod.rs）：以「细纲」开头的文件名一律落「细纲」组。
  // 预览与实际写入都必须用这个「最终去向」，避免用户以为存进 A 组、实际落在 B 组。
  function __wx_effective_group(group, name) {
    if (group !== '细纲' && String(name || '').indexOf('细纲') === 0) return '细纲';
    return group;
  }

  // 文件名定稿：safe_name 清洗后非空；无扩展名则补 .md（与书内文件习惯一致）
  function __wx_finalize_name(input) {
    var base = __wx_safe_name(input);
    if (!base || base === '_invalid_') return '';
    if (base.indexOf('.') < 0) base += '.md';
    return base;
  }

  // 默认文件名建议（仅建议，可改）：首行含「第N章」用之；其次取一级标题；否则时间戳
  function __wx_suggest_name(content) {
    var firstLine = String(content || '').split('\n', 1)[0] || '';
    var m = /第\s*[0-9一二三四五六七八九十百零两]+\s*章/.exec(firstLine);
    var base = '';
    if (m) base = m[0].replace(/\s+/g, '');
    if (!base) {
      var h = /^#{1,6}\s*(.+)$/.exec(firstLine);
      if (h) base = h[1].trim().slice(0, 24);
    }
    if (!base) {
      var d = new Date();
      var p2 = function (x) { return (x < 10 ? '0' : '') + x; };
      base = 'AI回复_' + d.getFullYear() + p2(d.getMonth() + 1) + p2(d.getDate()) + '_' + p2(d.getHours()) + p2(d.getMinutes());
    }
    return __wx_finalize_name(base) || 'AI回复.md';
  }

  function __wx_tree_has_file(tree, groupDir, name) {
    var groups = Array.isArray(tree) ? tree : ((tree && tree.groups) || []);
    for (var i = 0; i < groups.length; i++) {
      var g = groups[i] || {};
      if (g.groupDir !== groupDir && g.dir !== groupDir) continue;
      var files = Array.isArray(g.files) ? g.files : [];
      for (var j = 0; j < files.length; j++) {
        if (String((files[j] || {}).name || '') === name) return true;
      }
    }
    return false;
  }

  // 流结束后把「保存到书籍目录」按钮挂到最后一条 AI 气泡下（内容随按钮闭包保存，
  // 旧消息保留各自按钮；React 重渲染由 __wx_attach_retry 的有限重试兜底）
  function __wx_schedule_dir_save(raw, init, source) {
    var content = String(raw || '').trim();
    if (!content) return;
    var bid = (function () {
      try {
        var b = init && init.body ? JSON.parse(init.body) : null;
        return (b && b.args && b.args.bookId) || wxCurrentBook() || '';
      } catch (e) { return wxCurrentBook() || ''; }
    })();
    setTimeout(function () {
      __wx_attach_retry(function () { return __wx_attach_save_to_dir(content, bid, source); });
    }, 600);
  }

  function __wx_attach_save_to_dir(content, bookId, source) {
    if (!content) return true;
    var bubble = __wx_source_body(source);
    var host = bubble ? (bubble.parentNode || bubble) : null;
    if (!host) return false;
    if (host.querySelector('.__wx_sv')) return true;
    var wrap = document.createElement('div');
    wrap.className = '__wx_sv';
    wrap.style.cssText = 'margin:6px 0 2px;display:flex;gap:8px;align-items:center';
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.textContent = '💾 保存到书籍目录';
    btn.title = '把这条 AI 回复另存为书籍目录的新文件：写入前选择分组与文件名并预览确认；只新建、绝不覆盖已有文件（与自动落盘无关）';
    btn.style.cssText = 'border:1px solid var(--c-border-3,#EFEBE3);background:var(--c-white,#fff);color:var(--c-text-secondary,#5C6470);' +
      'border-radius:8px;padding:4px 10px;font-size:12px;cursor:pointer;font-family:var(--font-ui,inherit)';
    btn.addEventListener('click', function () {
      var bid = bookId || wxCurrentBook() || '';
      if (!bid) { __wx_toast('保存失败：未关联当前书籍'); return; }
      __wx_open_save_dialog(bid, content);
    });
    wrap.appendChild(btn);
    __wx_book_auto_save_mode(bookId || wxCurrentBook() || '').then(function (m) { if (!document.body.contains(wrap)) return; var lbl = ({ ask: '保存模式 ask=询问后保存', auto: '保存模式 auto=显式写作任务由后端自动入待审', off: '保存模式 off=仅预览，仍可手动保存' })[m] || ('保存模式 ' + m); var sp = document.createElement('span'); sp.style.cssText = 'font-size:11px;color:var(--c-text-muted,#9AA1AC)'; sp.textContent = lbl; wrap.appendChild(sp); }).catch(function () {});
    host.appendChild(wrap);
    return true;
  }

  function __wx_open_save_dialog(bookId, content) {
    var old = document.getElementById('__wx_save_overlay');
    if (old) old.remove();
    var bookTitle = '';
    var busy = false, checkSeq = 0, preOk = false;
    var curName = '', curEff = '设定';

    var ov = document.createElement('div');
    ov.id = '__wx_save_overlay';
    ov.style.cssText = 'position:fixed;inset:0;background:rgba(0,0,0,.45);z-index:2147483460;display:flex;align-items:center;justify-content:center';
    var box = document.createElement('div');
    box.style.cssText = 'background:var(--c-white,#fff);max-width:660px;width:94vw;max-height:86vh;overflow:auto;border-radius:12px;padding:16px 18px;' +
      'font-size:12.5px;line-height:1.65;color:var(--c-text-main,#1F2328);font-family:var(--font-ui,inherit);box-shadow:0 12px 48px rgba(0,0,0,.25)';

    var title = document.createElement('div');
    title.style.cssText = 'font-size:14.5px;font-weight:700;margin-bottom:2px';
    title.textContent = '保存到书籍目录（写入前确认 · 新建文件 · 绝不覆盖）';
    box.appendChild(title);

    var sub = document.createElement('div');
    sub.style.cssText = 'font-size:11.5px;color:var(--c-text-muted,#9AA1AC);margin-bottom:10px';
    sub.textContent = '点「确认写入」之前不会写任何文件，取消即放弃。本动作与自动落盘无关：若消息下方出现「保留/丢弃」条，那是服务端已自动落盘后的事后选择，不是本弹窗。';
    box.appendChild(sub);

    var destLine = document.createElement('div');
    destLine.style.cssText = 'margin:4px 0 10px;padding:8px 10px;border:1px solid var(--c-border-3,#EFEBE3);border-radius:8px;background:var(--c-card-light-2,#FDFCFA)';
    var destSpan = document.createElement('span');
    destSpan.style.cssText = 'font-weight:700';
    destLine.appendChild(document.createTextNode('将写入：'));
    destLine.appendChild(destSpan);
    box.appendChild(destLine);

    var groupRow = document.createElement('div');
    groupRow.style.cssText = 'display:flex;gap:14px;flex-wrap:wrap;margin:2px 0 6px;align-items:center';
    var groupLabel = document.createElement('span');
    groupLabel.style.cssText = 'color:var(--c-text-secondary,#5C6470)';
    groupLabel.textContent = '目标分组：';
    groupRow.appendChild(groupLabel);
    var groupInputs = [];
    __WX_SAVE_GROUPS.forEach(function (g) {
      var lab = document.createElement('label');
      lab.style.cssText = 'display:inline-flex;gap:4px;align-items:center;cursor:pointer';
      var r = document.createElement('input');
      r.type = 'radio'; r.name = '__wx_sv_group'; r.value = g; r.checked = (g === '设定');
      r.addEventListener('change', onPickChange);
      lab.appendChild(r);
      lab.appendChild(document.createTextNode(g));
      groupRow.appendChild(lab);
      groupInputs.push(r);
    });
    box.appendChild(groupRow);

    var nameRow = document.createElement('div');
    nameRow.style.cssText = 'display:flex;gap:8px;align-items:center;margin:2px 0 4px';
    var nameLabel = document.createElement('span');
    nameLabel.style.cssText = 'color:var(--c-text-secondary,#5C6470)';
    nameLabel.textContent = '文件名：';
    var nameInput = document.createElement('input');
    nameInput.type = 'text';
    nameInput.value = __wx_suggest_name(content);
    nameInput.spellcheck = false;
    nameInput.style.cssText = 'flex:1;min-width:200px;border:1px solid var(--c-border-input,#DCD6CA);border-radius:8px;padding:5px 9px;font-size:12.5px;font-family:var(--font-ui,inherit);color:inherit;background:var(--c-white,#fff)';
    nameInput.addEventListener('input', onPickChange);
    nameRow.appendChild(nameLabel);
    nameRow.appendChild(nameInput);
    box.appendChild(nameRow);

    var hintLine = document.createElement('div');
    hintLine.style.cssText = 'font-size:11.5px;color:var(--c-text-muted,#9AA1AC);margin:0 0 4px';
    box.appendChild(hintLine);

    var statusLine = document.createElement('div');
    statusLine.style.cssText = 'margin:0 0 4px;font-size:12px;color:var(--c-text-secondary,#5C6470)';
    box.appendChild(statusLine);

    var warnLine = document.createElement('div');
    warnLine.style.cssText = 'margin:0 0 4px;font-size:12px;color:#B03A34';
    box.appendChild(warnLine);

    var prevHead = document.createElement('div');
    prevHead.style.cssText = 'margin:8px 0 4px;font-weight:700';
    prevHead.textContent = '内容预览（写入的就是下面这份文本，共 ' + content.length + ' 字）';
    box.appendChild(prevHead);
    var pre = document.createElement('pre');
    pre.style.cssText = 'margin:0;white-space:pre-wrap;word-break:break-word;max-height:220px;overflow:auto;border:1px solid var(--c-border-3,#EFEBE3);border-radius:8px;padding:8px 10px;background:var(--c-bg-subtle,#F8F7F4);font-family:var(--font-prose,inherit);font-size:12px;line-height:1.7';
    pre.textContent = content.length > 800
      ? content.slice(0, 800) + '\n…（其余 ' + (content.length - 800) + ' 字未显示；写入时不截断）'
      : content;
    box.appendChild(pre);

    var btnRow = document.createElement('div');
    btnRow.style.cssText = 'display:flex;gap:10px;justify-content:flex-end;margin-top:12px';
    var cancelBtn = document.createElement('button');
    cancelBtn.type = 'button';
    cancelBtn.textContent = '取消（不写入）';
    cancelBtn.style.cssText = 'border:1px solid var(--c-border-3,#EFEBE3);background:var(--c-white,#fff);border-radius:8px;padding:5px 14px;font-size:12.5px;cursor:pointer;color:var(--c-text-secondary,#5C6470)';
    var okBtn = document.createElement('button');
    okBtn.type = 'button';
    okBtn.textContent = '确认写入（新建，不覆盖）';
    okBtn.style.cssText = 'border:1px solid var(--c-accent,#C7433C);background:var(--c-accent,#C7433C);color:#fff;border-radius:8px;padding:5px 14px;font-size:12.5px;cursor:pointer';
    okBtn.disabled = true;
    btnRow.appendChild(cancelBtn);
    btnRow.appendChild(okBtn);
    box.appendChild(btnRow);

    ov.appendChild(box);
    ov.addEventListener('click', function (e) { if (e.target === ov && !busy) close(); });
    document.addEventListener('keydown', esc);
    document.body.appendChild(ov);

    function esc(e) { if (e && e.key === 'Escape' && !busy) close(); }
    function close() {
      document.removeEventListener('keydown', esc);
      if (ov.parentNode) ov.parentNode.removeChild(ov);
    }
    cancelBtn.addEventListener('click', function () { if (!busy) close(); });

    function getGroup() {
      for (var i = 0; i < groupInputs.length; i++) if (groupInputs[i].checked) return groupInputs[i].value;
      return '设定';
    }
    function setInputsDisabled(dis) {
      nameInput.disabled = dis;
      for (var i = 0; i < groupInputs.length; i++) groupInputs[i].disabled = dis;
    }
    function updateDest() {
      destSpan.textContent = (bookTitle || bookId) + ' / ' + curEff + ' / ' + curName;
      var hints = [];
      if (curEff !== getGroup()) hints.push('文件名以「细纲」开头：按服务端规则实际写入「细纲」组');
      if (getGroup() === '正文待审' && !/第\s*[0-9一二三四五六七八九十百零两]+\s*章/.test(curName)) {
        hints.push('正文待审必须使用「第N章」文件名，否则服务端会拒绝写入（防止无队列稿）');
      }
      hintLine.textContent = hints.join('；');
      hintLine.style.display = hints.length ? '' : 'none';
    }

    function onPickChange() {
      if (busy) return;
      curName = __wx_finalize_name(nameInput.value);
      curEff = __wx_effective_group(getGroup(), curName || ' ');
      if (!curName) {
        preOk = false; okBtn.disabled = true;
        destSpan.textContent = (bookTitle || bookId) + ' / ' + curEff + ' / （文件名无效）';
        hintLine.style.display = 'none';
        statusLine.textContent = '✗ 文件名清洗后为空，请输入有效文件名';
        statusLine.style.color = '#B03A34';
        warnLine.textContent = '';
        return;
      }
      // F2：与服务端同规则（handlers/mod.rs save_chat_output → chapter_num_from_name）：
      // 正文待审必须「第N章」命名，前端先拦，不等写入报错
      if (curEff === '正文待审' && !/第\s*[0-9一二三四五六七八九十百零两]+\s*章/.test(curName)) {
        preOk = false; okBtn.disabled = true;
        destSpan.textContent = (bookTitle || bookId) + ' / ' + curEff + ' / ' + curName;
        hintLine.style.display = 'none';
        statusLine.textContent = '✗ 正文待审必须使用「第N章」文件名（否则服务端拒绝登记审批队列），请改名或换分组';
        statusLine.style.color = '#B03A34';
        warnLine.textContent = '';
        return;
      }
      updateDest();
      scheduleCheck();
    }

    function scheduleCheck() {
      var seq = ++checkSeq;
      preOk = false; okBtn.disabled = true;
      statusLine.textContent = '… 正在检查同名文件…';
      statusLine.style.color = 'var(--c-text-secondary,#5C6470)';
      warnLine.textContent = '';
      setTimeout(function () {
        if (seq !== checkSeq || busy) return;
        runCheck(seq);
      }, 250);
    }

    function runCheck(seq) {
      var eff = curEff, name = curName;
      invoke('scan_tree', { bookId: bookId }).then(function (tree) {
        if (seq !== checkSeq || busy) return;
        var dup = __wx_tree_has_file(tree, eff, name);
        var warnParts = [];
        if (eff === '正文待审' && __wx_tree_has_file(tree, '正文', name)) {
          warnParts.push('「正文」组已存在同名文件：该待审稿即使写入也无法批准转正（审批会拒绝），仅作存档');
        }
        // scan_tree 不含「正文待审」组：用审批队列名单补预检（只覆盖已登记条目，
        // 未登记文件查不到——最终以 save_chat_output 服务端原子检查为准，同名必报错、不写入）
        var pend = (eff === '正文待审')
          ? invoke('list_pending_chapters', { bookId: bookId }).then(function (rows) {
              var arr = rows || [];
              for (var i = 0; i < arr.length; i++) {
                if (String((arr[i] || {}).name || '') === name) return { dup: true };
              }
              return { dup: false };
            }).catch(function () { return { dup: false, unknown: true }; })
          : Promise.resolve({ dup: false });
        return pend.then(function (pr) {
          if (seq !== checkSeq || busy) return;
          if (dup || pr.dup) {
            preOk = false; okBtn.disabled = true;
            statusLine.textContent = '⚠ ' + eff + ' / ' + name + ' 已存在，请改名或换分组（本操作绝不覆盖）';
            statusLine.style.color = '#B03A34';
          } else {
            preOk = true; okBtn.disabled = false;
            statusLine.textContent = (eff === '正文待审')
              ? '✓ 预检未见同名（待审组清单不全，最终以写入前服务端原子检查为准：同名直接报错、不写入）'
              : '✓ ' + eff + ' 分组下没有同名文件，将以新文件写入';
            statusLine.style.color = '#1a7f37';
          }
          warnLine.textContent = warnParts.join('；');
        });
      }).catch(function (e) {
        if (seq !== checkSeq || busy) return;
        // 预检失败不阻断：save_chat_output 服务端原子只新建仍是硬保证
        preOk = true; okBtn.disabled = false;
        statusLine.textContent = '△ 预检失败（' + ((e && e.message) ? e.message : String(e)) + '）；确认写入时仍会强制非覆盖检查';
        statusLine.style.color = '#8a6d3b';
      });
    }

    okBtn.addEventListener('click', function () {
      if (busy || !preOk) return;
      var eff = curEff, name = curName;
      if (!name) return;
      var go = window.confirm('即将新建：' + eff + ' / ' + name + '\n内容约 ' + content.length + ' 字。\n\n本次写入为新建文件，绝不覆盖已有文件（写入前服务端会再做原子校验，同名直接报错）。\n确认写入吗？');
      if (!go) return;
      busy = true;
      setInputsDisabled(true);
      cancelBtn.disabled = true;
      okBtn.disabled = true;
      okBtn.textContent = '复查中…';
      invoke('scan_tree', { bookId: bookId }).then(function (tree) {
        if (__wx_tree_has_file(tree, eff, name)) {
          throw new Error(eff + ' / ' + name + ' 已存在（刚被其他窗口创建？），未写入任何内容，请改名后重试');
        }
        okBtn.textContent = '写入中…';
        return invoke('save_chat_output', { bookId: bookId, group: eff, name: name, content: content }).then(function (res) {
          // F9：必须明确 ok===true 才算成功；契约漂移（如网关吞掉 err 帧）时按失败处理，不假成功
          if (!res || res.ok !== true) throw new Error('服务端未确认写入成功（ok!==true），未当作已保存');
          return res;
        });
      }).then(function () {
        close();
        __wx_toast('已保存到 ' + eff + ' / ' + name + '（新建文件，未覆盖任何已有内容）', 6000);
        try { refreshReactTree({ bookId: bookId }); } catch (e2) {}
      }).catch(function (e) {
        busy = false;
        setInputsDisabled(false);
        cancelBtn.disabled = false;
        okBtn.disabled = false;
        okBtn.textContent = '确认写入（新建，不覆盖）';
        statusLine.textContent = '✗ 写入未完成：' + ((e && e.message) ? e.message : String(e));
        statusLine.style.color = '#B03A34';
        // 不自动重查：保留错误信息可见；用户改名/换组会自然触发下一轮预检
      });
    });

    invoke('list_books', {}).then(function (books) {
      var arr = books || [];
      for (var i = 0; i < arr.length; i++) {
        if (arr[i] && arr[i].id === bookId && arr[i].title) bookTitle = arr[i].title;
      }
      updateDest();
    }).catch(function () {});

    onPickChange();
  }

  // ============ 附加展示（不点就不存在，默认路径零变化） ============
  // 1) 提案 diff 查看：行级 LCS 对比 base/proposed，纯前端渲染
  function __wx_lcs_diff(a, b) {
    var n = a.length, m = b.length;
    if (n * m > 4000000) {
      // 超大文本降级：整段标为变更，避免 DP 爆内存
      return a.map(function (s) { return { t: '-', s: s }; }).concat(b.map(function (s) { return { t: '+', s: s }; }));
    }
    var dp = new Uint32Array((n + 1) * (m + 1));
    for (var i = n - 1; i >= 0; i--) {
      for (var j = m - 1; j >= 0; j--) {
        dp[i * (m + 1) + j] = a[i] === b[j] ? dp[(i + 1) * (m + 1) + j + 1] + 1 : Math.max(dp[(i + 1) * (m + 1) + j], dp[i * (m + 1) + j + 1]);
      }
    }
    var out = []; i = 0; j = 0;
    while (i < n && j < m) {
      if (a[i] === b[j]) { out.push({ t: ' ', s: a[i] }); i++; j++; }
      else if (dp[(i + 1) * (m + 1) + j] >= dp[i * (m + 1) + j + 1]) { out.push({ t: '-', s: a[i] }); i++; }
      else { out.push({ t: '+', s: b[j] }); j++; }
    }
    while (i < n) { out.push({ t: '-', s: a[i] }); i++; }
    while (j < m) { out.push({ t: '+', s: b[j] }); j++; }
    return out;
  }

  function __wx_show_diff(prop) {
    var old = document.getElementById('__wx_diff_overlay'); if (old) old.remove();
    var base = String(prop.baseContent || '').split('\n');
    var propd = String(prop.proposedContent || '').split('\n');
    var rows = __wx_lcs_diff(base, propd);
    var add = 0, del = 0;
    for (var k = 0; k < rows.length; k++) { if (rows[k].t === '+') add++; else if (rows[k].t === '-') del++; }
    var ov = document.createElement('div'); ov.id = '__wx_diff_overlay';
    ov.style.cssText = 'position:fixed;inset:0;background:rgba(0,0,0,.45);z-index:2147483460;display:flex;align-items:center;justify-content:center';
    var box = document.createElement('div');
    box.style.cssText = 'background:#fff;max-width:900px;width:94vw;max-height:84vh;overflow:auto;border-radius:12px;padding:14px 16px;font-size:12.5px;line-height:1.65';
    var head = document.createElement('div');
    head.style.cssText = 'display:flex;justify-content:space-between;align-items:center;margin-bottom:8px';
    head.innerHTML = '<b>' + (prop.fileName || '') + '</b><span style="color:#5C6470">+' + add + ' / -' + del + ' 行 · ' + (prop.summary || '') + '</span>';
    var close = document.createElement('button'); close.textContent = '关闭';
    close.style.cssText = 'border:1px solid #EFEBE3;background:#fff;border-radius:8px;padding:3px 10px;cursor:pointer';
    close.addEventListener('click', function () { ov.remove(); });
    head.appendChild(close);
    var pre = document.createElement('pre');
    pre.style.cssText = 'margin:0;white-space:pre-wrap;word-break:break-all;font-family:ui-monospace,Consolas,monospace;font-size:12px';
    for (var q = 0; q < rows.length; q++) {
      var line = document.createElement('div');
      var r0 = rows[q];
      line.style.color = r0.t === '+' ? '#1a7f37' : (r0.t === '-' ? '#cf222e' : '#57606a');
      line.style.background = r0.t === '+' ? '#e6ffec' : (r0.t === '-' ? '#ffebe9' : 'transparent');
      line.textContent = r0.t + ' ' + r0.s;
      pre.appendChild(line);
    }
    box.appendChild(head); box.appendChild(pre); ov.appendChild(box);
    ov.addEventListener('click', function (e) { if (e.target === ov) ov.remove(); });
    document.body.appendChild(ov);
  }

  function __wx_attach_diff_button(anchor, proposalId, bookId) {
    if (!anchor || !anchor.parentNode) return;
    if (anchor.parentNode.querySelector('.__wx_diffbtn')) return;
    var b = document.createElement('button');
    b.type = 'button'; b.className = '__wx_diffbtn';
    b.textContent = '🔍 查看变更';
    b.title = '查看该提案的行级差异（只读，不影响落盘路径）';
    b.style.cssText = 'border:1px solid var(--c-border-3,#EFEBE3);background:var(--c-white,#fff);color:var(--c-text-secondary,#5C6470);' +
      'border-radius:8px;padding:4px 10px;font-size:12px;cursor:pointer;font-family:var(--font-ui,inherit)';
    b.addEventListener('click', function () {
      invoke('dw_get_proposal', { bookId: bookId, id: proposalId }).then(function (p) {
        if (p && p.id) __wx_show_diff(p); else __wx_toast('提案不存在或已清理');
      }).catch(function (e) { __wx_toast('查看变更失败：' + ((e && e.message) ? e.message : String(e))); });
    });
    anchor.parentNode.insertBefore(b, anchor.nextSibling);
  }

  // 2) 用量面板：设置入口旁的附加按钮，聚合 llm_call_log
  function __wx_show_usage() {
    invoke('llm_usage', {}).then(function (u) {
      var old = document.getElementById('__wx_usage_overlay'); if (old) old.remove();
      var ov = document.createElement('div'); ov.id = '__wx_usage_overlay';
      ov.style.cssText = 'position:fixed;inset:0;background:rgba(0,0,0,.45);z-index:2147483460;display:flex;align-items:center;justify-content:center';
      var box = document.createElement('div');
      box.style.cssText = 'background:#fff;max-width:760px;width:92vw;max-height:80vh;overflow:auto;border-radius:12px;padding:14px 16px;font-size:12.5px;line-height:1.6';
      var html = '<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:8px"><b>模型用量（近 7 天）</b><button id="__wx_usage_close" style="border:1px solid #EFEBE3;background:#fff;border-radius:8px;padding:3px 10px;cursor:pointer">关闭</button></div>';
      html += '<table style="width:100%;border-collapse:collapse"><tr style="text-align:left;color:#57606a"><th style="padding:4px">模型</th><th>用途</th><th>次数</th><th>总 tokens</th></tr>';
      (u.byModel || []).forEach(function (r) {
        html += '<tr><td style="padding:4px;border-top:1px solid #eee">' + (r.model || '-') + '</td><td>' + (r.tag || '-') + '</td><td>' + (r.calls || 0) + '</td><td>' + (r.totalTokens || 0) + '</td></tr>';
      });
      html += '</table><div style="margin-top:10px;color:#57606a">按天：</div><div style="display:flex;flex-wrap:wrap;gap:6px;margin-top:4px">';
      (u.byDay || []).forEach(function (d) {
        var dt = new Date(d.day * 86400000);
        html += '<span style="border:1px solid #eee;border-radius:6px;padding:2px 8px">' + dt.toLocaleDateString('zh-CN') + ' · ' + (d.calls || 0) + ' 次 · ' + (d.totalTokens || 0) + ' tokens</span>';
      });
      html += '</div>';
      box.innerHTML = html;
      ov.appendChild(box);
      ov.addEventListener('click', function (e) { if (e.target === ov) ov.remove(); });
      document.body.appendChild(ov);
      document.getElementById('__wx_usage_close').addEventListener('click', function () { ov.remove(); });
    }).catch(function (e) { __wx_toast('用量加载失败：' + ((e && e.message) ? e.message : String(e))); });
  }

  __wx_attach_retry(function () {
    var top = document.querySelector('.header__actions') || document.querySelector('.mshell__top');
    if (!top) return false;
    if (top.querySelector('.__wx_usage_btn')) return true;
    var b = document.createElement('button');
    b.className = '__wx_usage_btn btn-ghost';
    b.textContent = '用量';
    b.title = '查看近 7 天模型调用用量（附加面板）';
    b.style.cssText = 'margin-left:6px';
    b.addEventListener('click', function () { __wx_show_usage(); });
    top.appendChild(b);
    return true;
  });

  function __wx_attach_writeback(raw, quote, source) {
    var bubble = __wx_source_body(source);
    var host = bubble ? (bubble.parentNode || bubble) : null;
    if (!host) return false;
    if (host.querySelector('.__wx_wb')) return true;
    var wrap = document.createElement('div');
    wrap.className = '__wx_wb';
    wrap.style.cssText = 'margin:6px 0 2px;display:flex;gap:8px;align-items:center';
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.textContent = '↩ 写回原文（生成提案）';
    btn.title = '把引用区间替换为本次产出并生成变更提案，经提案审阅确认后写入';
    btn.style.cssText = 'border:1px solid var(--c-border-3,#EFEBE3);background:var(--c-white,#fff);color:var(--c-text-secondary,#5C6470);' +
      'border-radius:8px;padding:4px 10px;font-size:12px;cursor:pointer;font-family:var(--font-ui,inherit)';
    btn.addEventListener('click', function () { __wx_writeback(btn, raw, quote, source.bookId); });
    wrap.appendChild(btn);
    host.appendChild(wrap);
    return true;
  }

  async function __wx_writeback(btn, raw, quote, bookId) {
    if (!bookId) { __wx_toast('写回失败：未关联当前书籍'); return; }
    var idleText = '↩ 写回原文（生成提案）';
    btn.disabled = true; btn.textContent = '定位引用区间…';
    try {
      var tree = await invoke('scan_tree', { bookId: bookId });
      var groups = Array.isArray(tree) ? tree : ((tree && tree.groups) || []);
      var pref = ['细纲', '正文', '正文待审', '设定'];
      var dirs = groups.map(function (g) { return g.dir || g.name; }).filter(Boolean);
      dirs.sort(function (a, b) {
        var ia = pref.indexOf(a), ib = pref.indexOf(b);
        return (ia < 0 ? 99 : ia) - (ib < 0 ? 99 : ib);
      });
      var hit = null;
      for (var d = 0; d < dirs.length && !hit; d++) {
        var dir = dirs[d];
        var gobj = null;
        for (var gi = 0; gi < groups.length; gi++) { if ((groups[gi].dir || groups[gi].name) === dir) { gobj = groups[gi]; break; } }
        var names = ((gobj && gobj.files) || []).map(function (f) { return f.name || f; });
        if (names.indexOf(quote.name) < 0) continue;
        var content = await invoke('read_file', { bookId: bookId, group: dir, name: quote.name });
        if (typeof content === 'string' && content.indexOf(quote.text) >= 0) hit = { dir: dir, content: content };
      }
      if (!hit) {
        __wx_toast('写回失败：未找到该文件或被引用文本（原文可能已被改动）', 5200);
        btn.disabled = false; btn.textContent = idleText;
        return;
      }
      var idx = hit.content.indexOf(quote.text);
      var proposed = hit.content.slice(0, idx) + raw + hit.content.slice(idx + quote.text.length);
      btn.textContent = '生成提案…';
      var res = await invoke('dw_create_proposal', {
        bookId: bookId, role: 'editor', group: hit.dir, name: quote.name,
        summary: '选区改写写回（对话产出）', proposedContent: proposed
      });
      if (res && res.id) {
        btn.textContent = '✓ 提案已生成，待审阅确认';
        __wx_toast('已生成变更提案：在 DeepWrite 工作台「提案审阅」确认后写入（不直接覆盖原稿）', 6000);
        // 附加：行级 diff 查看入口（只读，不改变任何落盘路径）
        __wx_attach_diff_button(btn, res.id, bookId);
      } else {
        throw new Error('建案返回异常：' + JSON.stringify(res).slice(0, 120));
      }
    } catch (e) {
      __wx_toast('写回失败：' + ((e && e.message) ? e.message : String(e)), 5200);
      btn.disabled = false; btn.textContent = idleText;
    }
  }

  // 对话落盘后刷新文件树：包装 fetch 嗅探 /ipc/chat_stream 流里的 {"e":{"type":"saved"}} 帧。
  // React 自己不刷新树，这里补上（失败绝不影响原请求）。
  (() => {
    if (window.__wx_fetch_sniffed) return;
    window.__wx_fetch_sniffed = true;
    const orig = window.fetch;
    if (typeof orig !== 'function') return;
    window.fetch = function (input, init) {
      const url = (typeof input === 'string') ? input : (input && input.url) || '';
      const p = orig.apply(this, arguments);
      try {
        if (url.indexOf('/ipc/chat_stream') >= 0 || url.indexOf('/ipc/inline_chat') >= 0) {
          p.then((resp) => {
            try {
              const clone = resp.clone();
              clone.text().then((txt) => {
                try {
                  if (txt && txt.indexOf('"saved"') >= 0) {
                    // 稍等一拍，等 React 把消息与树状态处理完。
                    // 带 bookId：落盘发生在哪本书就刷哪棵树（编辑器打开时按书记 dirty）。
                    const bid = (function () {
                      try {
                        const body = init && init.body ? JSON.parse(init.body) : null;
                        return (body && body.args && body.args.bookId) || wxCurrentBook() || '';
                      } catch (e) { return wxCurrentBook() || ''; }
                    })();
                    setTimeout(() => refreshReactTree({ bookId: bid }), 600);
                  }
                  __wx_handle_chat_stream(init, txt, url);
                } catch (e) {}
              }).catch(() => {});
            } catch (e) {}
            return resp;
          }).catch(() => {});
        }
      } catch (e) {}
      return p;
    };
  })();

  // ============ 挂载一：官方设置页「自动写作」导航 + 内置页 ============
  const installAutoStudio = () => {
    const PAGE_ID = '__wx_auto_page';
    const NAV_ID = '__wx_auto_nav';
    let navBtn = null;
    let panel = null;
    let pollTimer = null;
    const stopPoll = () => { if (pollTimer) { clearInterval(pollTimer); pollTimer = null; } };
    // 关窗/卸载也停设置页轮询（S09：非导航关窗未绑清理）
    window.addEventListener('pagehide', stopPoll);
    window.addEventListener('beforeunload', stopPoll);
    const startPoll = () => {
      stopPoll();
      pollTimer = setInterval(() => {
        if (document.hidden || window.__wx_closing) return;
        if (panel) panel.tick();
        // 记忆状态随轮询刷新（真实数据；失败/过期/依赖失效能持续可见）
        if (panel && panel.refreshMemory) panel.refreshMemory();
      }, 2500);
    };

    const ensureCss = () => {
      if (document.getElementById('__wx_auto_css')) return;
      const st = document.createElement('style');
      st.id = '__wx_auto_css';
      st.textContent =
        '.set .set__main{position:relative}' +
        '.set.__wx_auto_active .set__main > *:not(#__wx_auto_page){display:none !important}' +
        '.set:not(.__wx_auto_active) #__wx_auto_page{display:none !important}' +
        '.set.__wx_auto_active button.set__navitem:not(#__wx_auto_nav){background:transparent !important;color:rgb(82,82,91) !important;font-weight:400 !important}' +
        '.set.__wx_auto_active button.set__navitem:not(#__wx_auto_nav) svg{stroke:rgb(82,82,91) !important}' +
        '.set.__wx_auto_active #__wx_auto_nav{background:rgb(236,236,238) !important;color:rgb(37,99,235) !important;font-weight:600 !important}' +
        '.set.__wx_auto_active #__wx_auto_nav svg{stroke:rgb(37,99,235) !important}';
      document.head.appendChild(st);
    };
    const ensureNavBtn = () => {
      const nav = document.querySelector('.set__nav');
      if (!nav || document.getElementById(NAV_ID)) return;
      ensureCss();
      navBtn = document.createElement('button');
      navBtn.id = NAV_ID;
      navBtn.className = 'set__navitem';
      navBtn.type = 'button';
      navBtn.appendChild(WB.icon('pen', 16));
      navBtn.appendChild(document.createTextNode('写作'));
      const firstItem = nav.querySelector('button.set__navitem');
      if (firstItem) firstItem.before(navBtn); else nav.appendChild(navBtn);
      navBtn.addEventListener('click', activate);
    };
    const activate = () => {
      const setEl = document.querySelector('.set');
      const main = document.querySelector('.set__main');
      if (!setEl || !main) return;
      window.dispatchEvent(new CustomEvent('wx-auto-open', { detail: 'settings' }));
      ensureNavBtn();
      if (!panel) {
        panel = createAutoPanel({ agents: true });
        panel.el.id = PAGE_ID;
        panel.el.style.cssText = 'position:absolute;inset:0;display:flex;flex-direction:column;gap:14px;overflow-y:auto;scrollbar-width:thin;padding:18px 22px 24px;background:var(--mln-surface,#fff);color:var(--mln-text,#18181b);border-radius:10px;';
      }
      if (!main.contains(panel.el)) main.appendChild(panel.el);
      setEl.classList.add('__wx_auto_active');
      if (navBtn) navBtn.classList.add('is-active');
      panel.loadAll();
      startPoll();
    };
    const deactivate = () => {
      const setEl = document.querySelector('.set');
      if (setEl) setEl.classList.remove('__wx_auto_active');
      if (navBtn) navBtn.classList.remove('is-active');
      stopPoll();
    };
    document.addEventListener('click', (e) => {
      const t = e.target && e.target.closest ? e.target.closest('button.set__navitem') : null;
      if (t && t.id !== NAV_ID) deactivate();
    }, true);
    // 顶栏「自动写作」按钮 → 打开设置页里的自动写作面板（自动先开设置弹窗）
    // 设置页「写作」导航标题也补一句状态说明，避免用户以为自动连写直接定稿
    window.__wx_auto_open_studio = () => {
      if (document.querySelector('.set')) { activate(); return; }
      const gear = document.querySelector('button[aria-label="设置"]') || document.querySelector('button[title="设置"]');
      if (!gear) return;
      gear.click();
      let tries = 0;
      const t = setInterval(() => {
        tries += 1;
        if (document.querySelector('.set__nav')) { clearInterval(t); activate(); }
        else if (tries > 30) clearInterval(t);
      }, 100);
    };
    window.addEventListener('wx-auto-open', (e) => {
      if (e.detail === 'panel') {
        const setEl = document.querySelector('.set');
        if (setEl && setEl.classList.contains('__wx_auto_active')) deactivate();
      }
    });
    let settingsWasOpen = false;
    let watchTimer = null;
    const watchNow = () => {
      const open = !!document.querySelector('.set__nav');
      if (open) ensureNavBtn();
      if (open && !settingsWasOpen) {
        // 设置弹窗刚打开：广播，让右侧栏面板收起（互斥）
        window.dispatchEvent(new CustomEvent('wx-auto-open', { detail: 'settings' }));
      }
      settingsWasOpen = open;
    };
    const watch = () => {
      if (watchTimer !== null) return;
      watchTimer = setTimeout(() => { watchTimer = null; watchNow(); }, 200);
    };
    if (document.body) {
      new MutationObserver(watch).observe(document.body, { childList: true, subtree: true });
      watch();
    } else {
      document.addEventListener('DOMContentLoaded', () => {
        new MutationObserver(watch).observe(document.body, { childList: true, subtree: true });
        watch();
      });
    }
  };

  // ============ 挂载二：输入坞「全自动生成」入口（原右侧栏面板已下线） ============
  const installAutoDock = () => {
    const WRAP_ID = '__wx_auto_dockwrap';
    const ipc = __wx_ipc_raw;   // F8 去重：原 IPC 通道收敛为共享实现
    let btnEl = null, labelEl = null, busy = false, poll = null;

    const ensureCss = () => {
      if (document.getElementById('__wx_auto_dock_css')) return;
      const st = document.createElement('style');
      st.id = '__wx_auto_dock_css';
      st.textContent = [
        // 隐藏官方「写第N章」组，用同位置、同套官方样式的「全自动」替换
        '.dock__toolbar > .dock__chapter-actions:not(#__wx_auto_dockwrap){display:none !important}',
        '#__wx_auto_dockwrap.is-run .dock__chapter-label{color:var(--c-accent,#C7433C)}',
        // 对话流里的实时进度卡片（紧凑，宽度随消息列）
        '.wxfeed{margin:10px 0 6px;border:1px solid var(--c-border,var(--mln-border,#e4e4e7));border-radius:10px;background:var(--mln-surface,#fff);overflow:hidden;box-shadow:var(--mln-sh-card,0 1px 2px rgba(24,24,27,.04))}',
        '.wxfeed__head{display:flex;align-items:center;gap:7px;padding:6px 10px;border-bottom:1px solid var(--c-border-3,#f0f0f1);background:var(--c-bg-subtle,#fafafa)}',
        '.wxfeed__dot{width:6px;height:6px;border-radius:50%;background:var(--c-accent,#C7433C);flex:none;animation:wxfeedpulse 1.4s ease-in-out infinite}',
        '.wxfeed.is-done .wxfeed__dot{animation:none;background:var(--mln-ok,#16a34a)}',
        '@keyframes wxfeedpulse{0%,100%{opacity:1}50%{opacity:.25}}',
        '.wxfeed__title{font-size:11.5px;font-weight:600;color:var(--c-text-main,#18181b)}',
        '.wxfeed__sp{flex:1}',
        '.wxfeed__btn{height:20px;padding:0 8px;border-radius:6px;border:1px solid var(--c-border,var(--mln-border,#e4e4e7));background:var(--mln-surface,#fff);color:var(--c-text-secondary,var(--mln-text2,#52525b));font-size:11px;cursor:pointer;transition:background .12s}',
        '.wxfeed__btn:hover{background:var(--c-bg-hover,#f4f4f5)}',
        '.wxfeed__stop{border-color:var(--c-accent,#C7433C);color:var(--c-accent,#C7433C)}',
        '.wxfeed__body{padding:4px 10px 7px;max-height:150px;overflow-y:auto}',
        '.wxfeed__line{display:flex;gap:7px;padding:2px 0;font-size:11.5px;line-height:1.55;color:var(--c-text-secondary,#52525b)}',
        '.wxfeed__line .ico{flex:none;width:12px;text-align:center;color:var(--mln-ok,#16a34a)}',
        '.wxfeed__line.is-run .ico{color:var(--c-accent,#C7433C)}',
        '.wxfeed__line.is-err,.wxfeed__line.is-err .ico{color:var(--mln-accent,#dc2626)}'
      ].join('');
      document.head.appendChild(st);
    };

    const latestChapterOf = __wx_latest_chapter_of;   // F8 去重：与写作面板共用
    // 当前书 + 会话：前端把最后状态存在 settings.last_state（JSON 文本）
    const currentState = __wx_current_state;          // F8 去重：与待审浮条共用

    // 轻提示：短暂改按钮文字，不弹 alert
    const flash = (msg) => {
      if (!labelEl) return;
      labelEl.textContent = msg;
      setTimeout(() => { if (labelEl.textContent === msg) tick(); }, 2800);
    };

    // ---------- 对话流里的实时进度卡片 ----------
    const FEED_ID = '__wx_auto_feed';
    let feedEl = null, lastStatus = null, feedDismissed = false;
    let feedScheduled = false, feedBroken = false, feedLastMount = 0;

    const stepIcon = (l, isLast, running) => {
      const step = String(l.step || '');
      if (step === 'error') return { cls: 'is-err', ch: '✕' };
      if (step === 'start') return { cls: '', ch: '▶' };
      if (running && isLast && ['chapter', 'review', 'deai'].indexOf(step) >= 0) return { cls: 'is-run', ch: '●' };
      return { cls: '', ch: '✓' };
    };

    const buildFeed = () => {
      const d = document.createElement('div');
      d.id = FEED_ID;
      d.className = 'wxfeed';
      d.innerHTML =
        '<div class="wxfeed__head"><span class="wxfeed__dot"></span><span class="wxfeed__title"></span>' +
        '<span class="wxfeed__sp"></span>' +
        '<button type="button" class="wxfeed__btn wxfeed__stop">停止</button>' +
        '<button type="button" class="wxfeed__btn wxfeed__resume">续跑</button>' +
        '<button type="button" class="wxfeed__btn wxfeed__close">关闭</button></div>' +
        '<div class="wxfeed__body"></div>';
      d.querySelector('.wxfeed__stop').addEventListener('click', (e) => { e.stopPropagation(); doStop(); });
      d.querySelector('.wxfeed__resume').addEventListener('click', (e) => { e.stopPropagation(); doResume(); });
      d.querySelector('.wxfeed__close').addEventListener('click', (e) => {
        e.stopPropagation();
        feedDismissed = true;
        if (feedEl) { feedEl.remove(); feedEl = null; }
      });
      return d;
    };

    // 真正的渲染（由 renderFeed 防抖调用，绝不在 MutationObserver 里递归改 DOM）
    const mountFeed = () => {
      if (feedBroken) return;
      const s = lastStatus;
      const logs = (s && Array.isArray(s.logs)) ? s.logs : [];
      const running = !!(s && s.running);
      if (!logs.length || (feedDismissed && !running)) {
        if (feedEl) { feedEl.remove(); feedEl = null; }
        return;
      }
      // 已完成的老任务（>30 分钟）不再显示，避免下次打开还挂着旧卡片
      const lastTs = logs[logs.length - 1] && logs[logs.length - 1].ts ? logs[logs.length - 1].ts : 0;
      if (!running && lastTs && Date.now() - lastTs > 30 * 60 * 1000) {
        if (feedEl) { feedEl.remove(); feedEl = null; }
        return;
      }
      const scroller = document.querySelector('.msgscroll') || document.querySelector('main.chat');
      if (!scroller) return;
      // 挂到消息列内部（.msgscroll__inner），与消息同宽同边距，避免比输入框/消息区宽出一圈
      const host = scroller.querySelector('.msgscroll__inner') || scroller;
      if (feedEl && !feedEl.isConnected) feedEl = null;
      if (!feedEl) {
        // 重建节流：即便 React 反复清掉节点，也最多每秒重建一次，绝不空转
        if (Date.now() - feedLastMount < 1000) return;
        feedLastMount = Date.now();
        feedEl = buildFeed();
        const nearBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 160;
        host.appendChild(feedEl);
        if (nearBottom) scroller.scrollTop = scroller.scrollHeight;
      }

      const from = s.from != null ? s.from : 1;
      const to = s.to != null ? s.to : '?';
      const cur = s.current != null ? s.current : 0;
      feedEl.classList.toggle('is-done', !running);
      feedEl.querySelector('.wxfeed__title').textContent = running
        ? '全自动生成 · 第 ' + Math.min(Math.max(cur + 1, from), to) + '/' + to + ' 章'
        : (s.status === 'stopped' ? '全自动生成 · 已停止'
          : (s.status === 'failed' || s.error ? '全自动生成 · 失败' : '全自动生成 · 已完成'));
      feedEl.querySelector('.wxfeed__stop').style.display = running ? '' : 'none';
      // F4：任务已停/中断且可续跑时给出续跑入口（与设置面板同一链路：范围/模式确认 + confirmAuto）
      feedEl.querySelector('.wxfeed__resume').style.display = (!running && s.resumable) ? '' : 'none';
      feedEl.querySelector('.wxfeed__close').style.display = running ? 'none' : '';

      const body = feedEl.querySelector('.wxfeed__body');
      const sig = logs.map((l, i) => {
        const ic = stepIcon(l, i === logs.length - 1, running);
        return ic.ch + '|' + String(l.text || '');
      }).join('\n');
      if (body.dataset.sig === sig) return;
      body.dataset.sig = sig;
      body.innerHTML = '';
      logs.forEach((l, i) => {
        const ic = stepIcon(l, i === logs.length - 1, running);
        const row = document.createElement('div');
        row.className = 'wxfeed__line' + (ic.cls ? ' ' + ic.cls : '');
        const g = document.createElement('span'); g.className = 'ico'; g.textContent = ic.ch;
        const t = document.createElement('span'); t.textContent = String(l.text || '');
        row.appendChild(g); row.appendChild(t);
        body.appendChild(row);
      });
      body.scrollTop = body.scrollHeight;
    };

    // 防抖入口：多处调用也只会在一帧后渲染一次
    const renderFeed = () => {
      if (feedScheduled || feedBroken) return;
      feedScheduled = true;
      setTimeout(() => {
        feedScheduled = false;
        try { mountFeed(); } catch (e) { feedBroken = true; }
      }, 60);
    };

    // busy 期间禁用按钮：双击/连点不再产生第二次启动请求（原实现只靠 busy 提前 return，
    // 但按钮视觉无反馈，用户会以为没点到而连点，失败提示还可能被第二次请求覆盖）。
    const setBusy = (b) => {
      busy = !!b;
      if (btnEl) {
        btnEl.classList.toggle('is-busy', busy);
        const btn = btnEl.querySelector('.dock__next');
        if (btn) btn.disabled = busy;
      }
    };
    const startFull = async (n) => {
      if (busy) return;
      setBusy(true);
      try {
        const st = await currentState();
        if (!st) { flash('先选一本书'); return; }
        const latest = await latestChapterOf(st.bookId);
        // B 契约：fullAuto 只由本次启动显式决定，dock/panel 共用同一份「本次选择」（会话内存），
        // 绝不省略后由后端回退到历史书级 flag。默认 false = 新正文进「正文待审」。
        const fa = wxFullAutoOf(st.bookId);
        if (!wxConfirmWrite(st.bookName || st.bookId, latest + 1, latest + n, fa)) { flash('已取消'); return; }
        const sessionId = await wxSessionForBook(st.bookId);
        await ipc('auto_write_start', {
          bookId: st.bookId, sessionId, fromCh: latest + 1, toCh: latest + n,
          fullAuto: fa, confirmed: true, confirmAuto: fa,
        });
        // 立刻在对话里显示进度卡片（不等第一次轮询）
        feedDismissed = false;
        lastStatus = {
          running: true, from: latest + 1, to: latest + n, current: latest,
          bookId: st.bookId,   // F5：启动即带任务所属书，随后立即停止也不缺 bookId
          logs: [{ step: 'start', text: '已启动：第' + (latest + 1) + '~' + (latest + n) + ' 章' + (fa ? '（全自动定稿）' : '（正文待审）') }],
        };
        renderFeed();
        // 启动后立刻复位：本次选择只对这一次 start 生效
        wxResetFullAuto(st.bookId);
        await tick();
      } catch (e) {
        const msg = String(e.message || e);
        if (labelEl && btnEl) btnEl.title = '启动失败：' + msg;
        flash(msg.indexOf('已有') >= 0 ? '已有任务在跑' : '启动失败');
      } finally { setBusy(false); }
    };
    const doStop = async () => {
      if (busy) return;
      setBusy(true);
      try {
        // 停止按任务作用域：优先停「当前显示的任务所属书」，而不是随便一本书。
        // F5/F9：校验先行——没有任务所属书就绝不发请求（不发空 bookId）；
        // 「已请求停止」日志只在确认要发出请求后才追加，避免未停止却显示已请求。
        const stopBook = lastStatus && lastStatus.bookId;
        if (!stopBook) { flash('未找到当前任务所属书，未停止'); return; }
        if (lastStatus) {
          lastStatus.logs = (lastStatus.logs || []).concat([{ step: 'stop', text: '已请求停止：当前章写完后停下' }]);
          renderFeed();
        }
        const stopped = await ipc('auto_write_stop', { bookId: stopBook });
        if (stopped && stopped.ok === false) { flash(stopped.reason || '停止失败'); return; }
        await tick();
      } catch (e) { flash('停止失败：' + String((e && e.message) || e)); }
      finally { setBusy(false); }
    };
    const doResume = async () => {
      if (busy) return;
      setBusy(true);
      try {
        const st = await currentState();
        if (!st) { flash('先选一本书'); return; }
        const task = await ipc('auto_write_last_task', { bookId: st.bookId });
        const fullAuto = task && (task.fullAuto === 1 || task.fullAuto === true);
        const range = task && task.fromCh ? '第' + task.fromCh + '—' + task.toCh + '章' : '范围未知';
        const mode = fullAuto ? '全自动定稿（审核通过后直接写正文）' : '逐章待审';
        if (!window.confirm('确认续跑《' + (st.bookName || st.bookId) + '》' + range + '？\n模式：' + mode + '。不会覆盖已有稿。')) { flash('已取消'); return; }
        // F4：仅全自动需要二次确认；逐章只确认一次
        if (fullAuto && !window.confirm('再次确认：本次续跑仍会自动定稿，期间不逐章询问。确定继续吗？')) { flash('已取消'); return; }
        await ipc('auto_write_resume', { bookId: st.bookId, confirmed: true, confirmAuto: !!fullAuto });
        await tick();
      } catch (e) { flash('续跑失败：' + String((e && e.message) || e)); }
      finally { setBusy(false); }
    };

    const tick = async () => {
      try {
        const st = await currentState();
        // 按钮文案反映「本次是否全自动定稿」（按书选择）；默认是「写1章」进待审
        if (!st) { if (labelEl && labelEl.textContent !== '先选书') labelEl.textContent = '先选书'; return; }
        const autoLabel = wxFullAutoOf(st.bookId) ? '全自动' : '写1章';
        const s = (await ipc('auto_write_status', { bookId: st.bookId })) || {};
        // 跨书任务：当前书无任务、但全局任务跑在别的书上 → 明确提示归属，不静默隐藏
        if (s.bookId && s.bookId !== st.bookId) {
          lastStatus = null; renderFeed();
          if (btnEl) btnEl.classList.toggle('is-run', !!s.running);
          if (labelEl && s.running) {
            const txt = autoLabel + ' · 任务在别书';
            if (labelEl.textContent !== txt) labelEl.textContent = txt;
          } else if (labelEl && labelEl.textContent !== autoLabel) {
            labelEl.textContent = autoLabel;
          }
          return;
        }
        lastStatus = s;
        renderFeed();
        const running = !!s.running;
        if (btnEl) btnEl.classList.toggle('is-run', running);
        if (labelEl) {
          let txt = autoLabel;
          if (running) {
            const cur = s.current != null ? s.current : (s.curCh != null ? s.curCh : '?');
            txt = autoLabel + ' · ' + cur + '/' + (s.to != null ? s.to : '?');
          } else if (s.status === 'failed' || s.status === 'interrupted') {
            txt = autoLabel + ' · 已中断';
          }
          if (labelEl.textContent !== txt) labelEl.textContent = txt;
        }
      } catch (e) {}
    };

    const ensure = () => {
      const toolbar = document.querySelector('.dock__toolbar');
      if (!toolbar || document.getElementById(WRAP_ID)) return;
      ensureCss();
      const wrap = document.createElement('div');
      wrap.id = WRAP_ID;
      // 借用官方样式；is-busy 让窄栏（分栏后 data-layout=compact）按官方设计显示成图标按钮
      wrap.className = 'dock__chapter-actions is-busy';
      const b = document.createElement('button');
      b.type = 'button'; b.className = 'dock__next';
      // 文案对齐实际语义：本次是否全自动取决于「设置→写作」里该书的勾选；默认进「正文待审」。
      b.title = '生成 1 章：生成 → 审核 → 去 AI 味。默认进入「正文待审」等待你接受；在 设置→写作 勾选「本次全自动定稿」后才会审核通过直接写入正文';
      b.innerHTML =
        '<svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20h9"/><path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4Z"/></svg>' +
        '<span class="dock__chapter-label">写1章</span>';
      b.addEventListener('click', (e) => { e.stopPropagation(); startFull(1); });
      wrap.appendChild(b);
      btnEl = wrap;
      labelEl = b.querySelector('.dock__chapter-label');
      const anchor = toolbar.querySelector('.dock__chapter-actions');
      if (anchor) anchor.insertAdjacentElement('afterend', wrap); else toolbar.appendChild(wrap);
      tick();
      if (!poll) {
        // v2.2 流畅度：运行中 2s 一查；空闲降到 8s；隐藏页跳过
        let idleBeats = 0;
        poll = setInterval(() => {
          // 关窗/卸载即停轮询：避免页面已卸载后仍持续发 IPC（原实现只在导航时清理）
          if (document.hidden || window.__wx_closing) return;
          const running = !!(lastStatus && lastStatus.running);
          if (!running) {
            idleBeats += 1;
            if (idleBeats < 4) return;
            idleBeats = 0;
          } else {
            idleBeats = 0;
          }
          tick();
        }, 2000);
      }
    };

    // 关窗停轮询：beforeunload/pagehide 标记并清理定时器（S09：非导航关窗也要停）
    const stopDockPoll = () => {
      window.__wx_closing = true;
      if (poll) { clearInterval(poll); poll = null; }
    };
    window.addEventListener('pagehide', stopDockPoll);
    window.addEventListener('beforeunload', stopDockPoll);
    // bfcache 返回：清除关闭标记，恢复轮询（否则返回后永远不再 tick）
    window.addEventListener('pageshow', () => { window.__wx_closing = false; try { ensure(); } catch (e) {} });

    // 观察器里绝不做递归 DOM 改动：ensure 只补按钮，renderFeed 防抖且不重挂已连接节点
    let mutTimer = null;
    const onMut = () => {
      if (mutTimer !== null) return;
      mutTimer = setTimeout(() => {
        mutTimer = null;
        try { ensure(); } catch (e) {}
        try { if (window.__wx_pend_reensure) window.__wx_pend_reensure(); } catch (e) {}
        if (lastStatus) renderFeed();
      }, 150);
    };
    if (document.body) {
      new MutationObserver(onMut).observe(document.body, { childList: true, subtree: true });
      try { ensure(); } catch (e) {}
    } else {
      document.addEventListener('DOMContentLoaded', () => {
        new MutationObserver(onMut).observe(document.body, { childList: true, subtree: true });
        try { ensure(); } catch (e) {}
      });
    }
  };

  // ============ 挂载三：输入框上方的待审小浮条（预览 / 接受 / 拒绝） ============
  const installPendingStrip = () => {
    const BAR_ID = '__wx_pending_bar';
    const PV_ID = '__wx_pending_pv';

    const ensureCss = () => {
      if (document.getElementById('__wx_pend_css')) return;
      const st = document.createElement('style');
      st.id = '__wx_pend_css';
      st.textContent = [
        '.wxpend{display:flex;align-items:center;gap:8px;min-height:30px;padding:0 8px 0 10px;margin:0 0 8px;border:1px solid var(--c-border,#e4e4e7);border-radius:10px;background:var(--c-bg-subtle,#fafafa);font-size:11.5px;color:var(--c-text-secondary,#52525b)}',
        '.wxpend__ttl{display:inline-flex;align-items:center;gap:5px;flex:none;font-weight:600;color:var(--c-text-main,#18181b);white-space:nowrap}',
        '.wxpend__list{display:flex;align-items:center;gap:6px;overflow-x:auto;scrollbar-width:none;flex:1;min-width:0;padding:3px 0}',
        '.wxpend__list::-webkit-scrollbar{display:none}',
        '.wxpend__it{display:inline-flex;align-items:center;gap:1px;flex:none;height:24px;padding:0 2px 0 8px;border:1px solid var(--c-border,#e4e4e7);border-radius:999px;background:#fff}',
        '.wxpend__ch{font-weight:600;margin-right:2px;font-variant-numeric:tabular-nums;white-space:nowrap}',
        '.wxpend__b{width:20px;height:20px;display:inline-flex;align-items:center;justify-content:center;border:0;background:transparent;border-radius:50%;cursor:pointer;color:var(--c-text-secondary,#52525b);padding:0;transition:background .12s,color .12s}',
        '.wxpend__b:hover{background:var(--c-row-hover,#f4f4f5)}',
        '.wxpend__b.ok:hover{color:var(--mln-ok,#16a34a);background:rgba(22,163,74,.08)}',
        '.wxpend__b.no:hover{color:var(--mln-accent,#dc2626);background:var(--c-accent-bg,rgba(220,38,38,.08))}',
        '.wxpend__b:disabled{opacity:.4;cursor:default}',
        '.wxpend__pv{position:fixed;z-index:2147483000;max-height:46vh;overflow:auto;background:var(--mln-surface,#fff);border:1px solid var(--c-border,var(--mln-border,#e4e4e7));border-radius:12px;box-shadow:var(--mln-sh-pop,0 12px 32px -8px rgba(24,24,27,.18);padding:12px 14px;white-space:pre-wrap;word-break:break-word;font-size:12.5px;line-height:1.75;color:var(--c-text-main,#18181b)}',
        '.wxpend__pvh{font-size:11px;color:var(--c-text-muted,#a1a1aa);margin-bottom:6px}'
      ].join('');
      document.head.appendChild(st);
    };

    const svg = (d, size) => '<svg xmlns="http://www.w3.org/2000/svg" width="' + (size || 13) + '" height="' + (size || 13) + '" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">' + d + '</svg>';
    const I_EYE = svg('<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7Z"/><circle cx="12" cy="12" r="3"/>');
    const I_OK = svg('<path d="M20 6L9 17l-5-5"/>');
    const I_NO = svg('<path d="M18 6L6 18M6 6l12 12"/>');
    const I_IN = svg('<path d="M22 12h-6l-2 3h-4l-2-3H2"/><path d="M5.5 5.1L2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.5-6.9a2 2 0 0 0-1.8-1.1H7.3a2 2 0 0 0-1.8 1.1Z"/>');

    const currentBook = __wx_current_state;   // F8 去重：与 Dock 共用 last_state 反查

    let barEl = null, pvEl = null, pvIdx = -1, items = [], bookId = '', lastSig = '', busy = false;

    const closePreview = () => { pvIdx = -1; if (pvEl) { pvEl.remove(); pvEl = null; } };

    const showPreview = async (i) => {
      if (pvIdx === i) { closePreview(); return; }
      const c = items[i];
      if (!c) return;
      try {
        const text = await invoke('read_pending_chapter', { bookId: bookId, name: c.name });
        closePreview();
        pvEl = document.createElement('div');
        pvEl.id = PV_ID; pvEl.className = 'wxpend__pv';
        const head = document.createElement('div');
        head.className = 'wxpend__pvh';
        head.textContent = (c.ch > 0 ? '第' + c.ch + '章' : c.name) + (c.words != null ? ' · ' + c.words + ' 字' : '') + (c.deai != null ? ' · AI味 ' + c.deai : '');
        const body = document.createElement('div');
        body.textContent = String(text == null ? '（空）' : text);
        pvEl.appendChild(head); pvEl.appendChild(body);
        document.body.appendChild(pvEl);
        if (barEl) {
          const r = barEl.getBoundingClientRect();
          pvEl.style.left = Math.round(r.left) + 'px';
          pvEl.style.width = Math.round(Math.min(r.width, 760)) + 'px';
          const h = Math.min(pvEl.scrollHeight, Math.round(window.innerHeight * 0.46));
          pvEl.style.top = Math.round(Math.max(8, r.top - h - 8)) + 'px';
        }
        pvIdx = i;
      } catch (e) { closePreview(); }
    };

    const act = async (i, kind) => {
      if (busy) return;
      const c = items[i];
      if (!c) return;
      busy = true;
      try {
        if (kind === 'ok') await invoke('approve_chapter', { bookId: bookId, ch: c.ch, name: c.name });
        else await invoke('reject_chapter', { bookId: bookId, ch: c.ch, name: c.name });
      } catch (e) { __wx_toast('审批未完成：' + (e.message || String(e)), 6500); }
      busy = false;
      closePreview();
      await refresh(true);
    };

    const mkBtn = (html, title, cls, fn) => {
      const b = document.createElement('button');
      b.type = 'button'; b.className = 'wxpend__b' + (cls ? ' ' + cls : '');
      b.title = title; b.innerHTML = html;
      b.addEventListener('click', (e) => { e.stopPropagation(); fn(); });
      return b;
    };

    const render = () => {
      const dock = document.querySelector('.dock');
      if (!dock) return;
      if (!items.length) { if (barEl) { barEl.remove(); barEl = null; } closePreview(); return; }
      if (barEl && !barEl.isConnected) barEl = null;
      if (!barEl) {
        ensureCss();
        barEl = document.createElement('div');
        barEl.id = BAR_ID; barEl.className = 'wxpend';
        const ttl = document.createElement('span'); ttl.className = 'wxpend__ttl'; ttl.innerHTML = I_IN;
        const list = document.createElement('span'); list.className = 'wxpend__list';
        barEl.appendChild(ttl); barEl.appendChild(list);
        dock.insertBefore(barEl, dock.firstElementChild);
      }
      barEl.querySelector('.wxpend__ttl').innerHTML = I_IN + '<span>待审 ' + items.length + '</span>';
      const list = barEl.querySelector('.wxpend__list');
      const sig = JSON.stringify(items.map((x) => [x.ch, x.name, x.words, x.deai]));
      if (list.dataset.sig === sig) return;
      list.dataset.sig = sig;
      list.innerHTML = '';
      items.forEach((c, i) => {
        const it = document.createElement('span');
        it.className = 'wxpend__it';
        const ch = document.createElement('span');
        ch.className = 'wxpend__ch'; ch.textContent = c.ch > 0 ? '第' + c.ch + '章' : c.name;
        it.appendChild(ch);
        it.appendChild(mkBtn(I_EYE, '预览', '', () => showPreview(i)));
        it.appendChild(mkBtn(I_OK, '接受', 'ok', () => act(i, 'ok')));
        it.appendChild(mkBtn(I_NO, '拒绝', 'no', () => act(i, 'no')));
        list.appendChild(it);
      });
    };

    const refresh = async (force) => {
      const st = await currentBook();
      if (!st) { if (items.length) { items = []; lastSig = ''; render(); } return; }
      const requestedBook = st.bookId;
      let list = null;
      try { list = await invoke('list_pending_chapters', { bookId: requestedBook }); } catch (e) { return; }
      if (wxCurrentBook() !== requestedBook) return; bookId = requestedBook;
      list = list || [];
      const sig = bookId + '|' + JSON.stringify(list.map((x) => [x.ch, x.name, x.words, x.deai]));
      if (!force && sig === lastSig) return;
      lastSig = sig;
      items = list;
      if (!items.length) closePreview();
      render();
    };

    const reensure = () => {
      try {
        if (items.length && (!barEl || !barEl.isConnected)) {
          barEl = null;
          render();
        }
      } catch (e) {}
    };
    window.__wx_pend_reensure = reensure;

    document.addEventListener('click', (e) => {
      if (!pvEl) return;
      const t = e.target;
      if (t && t.closest && (t.closest('#' + PV_ID) || t.closest('#' + BAR_ID))) return;
      closePreview();
    }, true);
    window.addEventListener('resize', closePreview);
    window.addEventListener('scroll', closePreview, true);

    let idleBeats = 0;
    setInterval(() => {
      if (document.hidden) return;
      if (!items.length) {
        idleBeats += 1;
        if (idleBeats < 2) return;
        idleBeats = 0;
      } else {
        idleBeats = 0;
      }
      refresh(false);
    }, 5000);
    setTimeout(() => refresh(true), 1200);
  };

  // ============ DeepWrite 项目工作台：角色 / 技能 / 上下文 / 提案审阅 ============
  const installDeepWriteStudio = () => {
    const BTN_ID = '__wx_dw_btn';
    const OVERLAY_ID = '__wx_dw_overlay';
    let scheduled = false;
    let opening = false;
    let renderSeq = 0;
    let lastFocus = null;
    const el = (tag, cls, text) => { const n = document.createElement(tag); if (cls) n.className = cls; if (text != null) n.textContent = text; return n; };
    const closeAll = () => {
      document.querySelectorAll('#' + OVERLAY_ID).forEach((n) => n.remove());
      const b = document.getElementById(BTN_ID);
      if (b) b.setAttribute('aria-expanded', 'false');
      if (lastFocus && lastFocus.isConnected) { try { lastFocus.focus(); } catch (e) {} }
      lastFocus = null;
    };
    // 当前书：只信「页面标题反查」；last_book_id 必须与标题交叉校验。识别不到时返回 null，由选书界面让用户显式选择，绝不猜书。
    const currentBook = async () => {
      try { if (window.__wx_current_book) { const b = await window.__wx_current_book(); if (b && b.id) return b; } } catch (e) {}
      const id = wxCurrentBook() || '';
      if (!id) return null;
      const books = (await invoke('list_books', {})) || [];
      const hit = books.find((b) => b.id === id);
      if (!hit) return null;
      const titleEl = document.querySelector('.chathead__title');
      const shown = titleEl ? (titleEl.textContent || '').trim() : '';
      const full = (hit.title || '').trim();
      if (shown && full && shown.indexOf(full) < 0 && full.indexOf(shown) < 0) return null;
      return hit;
    };
    const ensureCss = () => {
      if (document.getElementById('__wx_dw_css')) return;
      const st = el('style'); st.id = '__wx_dw_css'; st.textContent =
        '#__wx_dw_overlay{position:fixed;inset:0;z-index:2147483100;background:rgba(15,15,18,.48);display:flex;align-items:center;justify-content:center;padding:24px}' +
        '.__dw_shell{width:min(1180px,96vw);height:min(780px,92vh);height:min(780px,92dvh);background:var(--c-card-light,#fff);color:var(--c-text-main,#18181b);border-radius:18px;box-shadow:0 24px 80px rgba(0,0,0,.26);display:flex;flex-direction:column;overflow:hidden}' +
        '.__dw_head{display:flex;align-items:center;gap:12px;padding:16px 20px;border-bottom:1px solid var(--c-border,#e4e4e7)}.__dw_title{font-size:18px;font-weight:700}.__dw_sub{color:var(--c-text-secondary,#71717a);font-size:13px;flex:1}' +
        '.__dw_tabs{display:flex;gap:6px;padding:10px 18px;border-bottom:1px solid var(--c-border,#e4e4e7)}.__dw_tabs button,.__dw_btn{border:0;border-radius:9px;padding:8px 12px;cursor:pointer;background:var(--c-row-hover,#f4f4f5);color:var(--c-text-main,#27272a)}.__dw_tabs button.is-on,.__dw_btn.primary{background:var(--c-accent,#C7433C);color:#fff}' +
        '.__dw_body{padding:18px;overflow:auto;flex:1}.__dw_grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:14px}.__dw_card{border:1px solid var(--c-border,#e4e4e7);border-radius:12px;padding:14px;background:var(--c-card-light,#fff);color:var(--c-text-main,#18181b)}' +
        'button.__dw_card{cursor:pointer;text-align:left}button.__dw_card:hover{background:var(--c-row-hover,#fafafa)}button.__dw_card:focus-visible{outline:2px solid var(--c-accent,#C7433C);outline-offset:2px}' +
        '.__dw_row{display:flex;gap:10px;align-items:center;margin:8px 0;flex-wrap:wrap}.__dw_row input[type=text],.__dw_card textarea{width:100%;box-sizing:border-box;border:1px solid var(--c-border-input,#d4d4d8);border-radius:8px;padding:9px;background:var(--c-card-light,#fff);color:var(--c-text-main,#18181b)}.__dw_card textarea{min-height:110px;resize:vertical}' +
        '.__dw_skill{display:flex;gap:10px;align-items:flex-start;padding:10px 0;border-bottom:1px solid var(--c-border,#eee)}.__dw_muted{color:var(--c-text-secondary,#71717a);font-size:13px}.__dw_error{color:#dc2626;white-space:pre-wrap;margin:8px 0}.__dw_ok{color:#15803d;background:rgba(21,128,61,.08);border:1px solid rgba(21,128,61,.25);border-radius:8px;padding:8px 10px;margin:0 0 10px}' +
        '.__dw_pick{display:block;width:100%;text-align:left;margin:6px 0}.__dw_pick strong{display:block}' +
        '.__dw_diff{display:grid;grid-template-columns:1fr 1fr;gap:12px}.__dw_diffbox{white-space:pre-wrap;word-break:break-word;max-height:420px;overflow:auto;background:var(--c-center-bg,#f8fafc);padding:12px;border-radius:9px;border:1px solid var(--c-border,#e4e4e7);font-size:12.5px;line-height:1.75}.__dw_diffbox .__dw_add{background:rgba(21,128,61,.12)}.__dw_diffbox .__dw_del{background:rgba(220,38,38,.10)}.__dw_diffbox div{min-height:1.4em}' +
        '#__wx_dw_btn{position:fixed;right:18px;top:calc(14px + env(safe-area-inset-top,0px));z-index:2147482000;display:flex;align-items:center;gap:6px;border:1px solid var(--c-border-btn,#d4d4d8);border-radius:10px;padding:8px 12px;background:var(--c-card-light,#fff);color:var(--c-text-main,#27272a);box-shadow:0 4px 18px rgba(0,0,0,.12);cursor:pointer;font:13px sans-serif}' +
        '@media(max-width:760px),(max-height:520px){#__wx_dw_overlay{padding:env(safe-area-inset-top,0px) env(safe-area-inset-right,0px) env(safe-area-inset-bottom,0px) env(safe-area-inset-left,0px)}.__dw_shell{width:100vw;height:100vh;height:100dvh;border-radius:0}.__dw_grid,.__dw_diff{grid-template-columns:1fr}}';
      document.head.appendChild(st);
    };
    const renderDiff = (parent, baseText, propText) => {
      const a = String(baseText == null ? '' : baseText).split('\n');
      const b = String(propText == null ? '' : propText).split('\n');
      const CAP = 3000;
      const countMap = (arr) => { const m = new Map(); for (const x of arr) m.set(x, (m.get(x) || 0) + 1); return m; };
      const bMap = countMap(b); const aMap = countMap(a);
      const grid = el('div', '__dw_diff');
      const mkCol = (label, lines, otherMap, cls, len) => {
        const col = el('div');
        col.appendChild(el('strong', null, label + '（' + len + ' 字符）'));
        const box = el('div', '__dw_diffbox'); box.setAttribute('aria-label', label);
        const show = lines.slice(0, CAP);
        for (const line of show) {
          const n = otherMap.get(line) || 0;
          let clsName = null;
          if (n > 0) { otherMap.set(line, n - 1); } else { clsName = cls; }
          box.appendChild(el('div', clsName, line === '' ? ' ' : line));
        }
        if (lines.length > CAP) box.appendChild(el('div', '__dw_muted', '…（超过 ' + CAP + ' 行，已截断显示）'));
        if (len === 0) box.appendChild(el('div', '__dw_muted', '（空内容）'));
        col.appendChild(box); return col;
      };
      grid.appendChild(mkCol('当前原稿', a, bMap, '__dw_del', String(baseText == null ? '' : baseText).length));
      grid.appendChild(mkCol('建议改稿', b, aMap, '__dw_add', String(propText == null ? '' : propText).length));
      parent.appendChild(grid);
    };
    const open = async () => {
      if (opening) return;
      if (document.getElementById(OVERLAY_ID)) { closeAll(); return; }
      opening = true;
      try {
        ensureCss();
        lastFocus = document.activeElement;
        renderSeq += 1;
        const overlay = el('div'); overlay.id = OVERLAY_ID;
        overlay.setAttribute('role', 'dialog'); overlay.setAttribute('aria-modal', 'true'); overlay.setAttribute('aria-label', 'DeepWrite 工作台');
        const shell = el('section', '__dw_shell'); overlay.appendChild(shell);
        const head = el('header', '__dw_head');
        const title = el('div', '__dw_title', 'DeepWrite 工作台');
        const sub = el('div', '__dw_sub', '');
        const close = el('button', '__dw_btn', '关闭'); close.addEventListener('click', closeAll);
        head.append(title, sub, close); shell.appendChild(head);
        const body = el('main', '__dw_body'); shell.appendChild(body);
        overlay.addEventListener('mousedown', (e) => { if (e.target === overlay) closeAll(); });
        document.body.appendChild(overlay);
        const b0 = document.getElementById(BTN_ID); if (b0) b0.setAttribute('aria-expanded', 'true');
        setTimeout(() => { try { close.focus(); } catch (e) {} }, 0);
        const mount = (book) => {
          sub.textContent = '当前项目：' + (book.title || book.id);
          const tabs = el('nav', '__dw_tabs'); shell.insertBefore(tabs, body);
          const showError = (e) => { body.replaceChildren(el('div', '__dw_error', '加载失败：' + (e && e.message || e))); };
          const activate = (btn) => Array.from(tabs.children).forEach((x) => x.classList.toggle('is-on', x === btn));
          const stale = (my) => my !== renderSeq || !overlay.isConnected;
          const agentTab = el('button', null, '智能体'); const skillTab = el('button', null, '技能'); const contextTab = el('button', null, '上下文'); const proposalTab = el('button', null, '提案审阅');
          tabs.append(agentTab, skillTab, contextTab, proposalTab);
          const renderAgents = async () => {
            const my = ++renderSeq; activate(agentTab); body.replaceChildren(el('div', '__dw_muted', '正在加载…'));
            try {
              const agents = (await invoke('dw_list_agents', { bookId: book.id })) || [];
              if (stale(my)) return;
              const grid = el('div', '__dw_grid');
              for (const a of agents) {
                const c = el('section', '__dw_card');
                const h = el('div', '__dw_row');
                const enabled = el('input'); enabled.type = 'checkbox'; enabled.checked = Number(a.enabled) !== 0; enabled.setAttribute('aria-label', (a.name || a.role) + ' 启用');
                const role = el('strong', null, (a.name || '') + ' · ' + (a.role || '')); h.append(enabled, role);
                const name = el('input'); name.type = 'text'; name.value = a.name || ''; name.setAttribute('aria-label', (a.role || '') + ' 名称');
                const prompt = el('textarea'); prompt.value = a.systemPrompt || ''; prompt.setAttribute('aria-label', (a.role || '') + ' 系统提示词');
                const save = el('button', '__dw_btn primary', '保存');
                const msg = el('span', '__dw_muted');
                save.addEventListener('click', async () => {
                  if (!name.value.trim() || !prompt.value.trim()) { msg.className = '__dw_error'; msg.textContent = '名称和提示词不能为空'; return; }
                  save.disabled = true;
                  try {
                    await invoke('dw_save_agent', { bookId: book.id, role: a.role, name: name.value, systemPrompt: prompt.value, model: a.model || '', enabled: enabled.checked });
                    a.name = name.value;
                    msg.className = '__dw_muted'; msg.textContent = '已保存';
                    setTimeout(() => { if (msg.textContent === '已保存') msg.textContent = ''; }, 3000);
                  } catch (e) { msg.className = '__dw_error'; msg.textContent = '失败：' + (e.message || e); }
                  finally { save.disabled = false; }
                });
                c.append(h, name, prompt);
                const foot = el('div', '__dw_row'); foot.append(save, msg); c.append(foot); grid.append(c);
              }
              if (!stale(my)) body.replaceChildren(grid);
            } catch (e) { if (!stale(my)) showError(e); }
          };
          const renderSkills = async () => {
            const my = ++renderSeq; activate(skillTab); body.replaceChildren(el('div', '__dw_muted', '正在加载…'));
            try {
              const pair = await Promise.all([invoke('list_skills', {}), invoke('dw_list_skill_bindings', { bookId: book.id })]);
              if (stale(my)) return;
              const all = (pair[0] || []).filter((s) => Number(s.enabled) !== 0);
              const bound = pair[1] || [];
              const ids = new Set(bound.filter((x) => Number(x.enabled) !== 0).map((x) => x.id));
              const wrap = el('div', '__dw_card');
              wrap.appendChild(el('div', '__dw_muted', '勾选后的技能按任务路由加入本书的策划、正文和润色阶段；严格 JSON 审核与记忆协议不会注入这些技能。空模板技能不会生效。'));
              const status = el('div', '__dw_muted'); status.setAttribute('aria-live', 'polite');
              for (const s of all) {
                const row = el('label', '__dw_skill');
                const cb = el('input'); cb.type = 'checkbox'; cb.checked = ids.has(s.id);
                const emptyTpl = !(s.promptTemplate || '').trim();
                const text = el('div');
                text.append(el('strong', null, s.name || s.id), el('div', '__dw_muted', (s.description || '') + (emptyTpl ? '（模板为空，绑定后不会生效）' : '')));
                if (emptyTpl) cb.disabled = true;
                cb.addEventListener('change', async () => {
                  cb.disabled = true;
                  try {
                    if (cb.checked) await invoke('dw_bind_skill', { bookId: book.id, skillId: s.id, enabled: true });
                    else await invoke('dw_unbind_skill', { bookId: book.id, skillId: s.id });
                    status.className = '__dw_muted'; status.textContent = (cb.checked ? '已绑定：' : '已解绑：') + (s.name || s.id);
                  } catch (e) { cb.checked = !cb.checked; status.className = '__dw_error'; status.textContent = '操作失败：' + (e.message || e); }
                  finally { cb.disabled = emptyTpl; }
                });
                row.append(cb, text); wrap.append(row);
              }
              wrap.append(status);
              if (!stale(my)) body.replaceChildren(wrap);
            } catch (e) { if (!stale(my)) showError(e); }
          };
          const renderContext = async () => {
            const my = ++renderSeq; activate(contextTab); body.replaceChildren(el('div', '__dw_muted', '正在生成安全上下文（AI隐藏文件会自动排除）…'));
            try {
              const c = await invoke('dw_context_bundle', { bookId: book.id, maxChars: 120000 });
              if (stale(my)) return;
              const card = el('div', '__dw_card');
              card.append(el('h3', null, '上下文预算'), el('div', '__dw_muted', '已用 ' + ((c.limits && c.limits.usedChars) || 0) + ' / ' + ((c.limits && c.limits.maxChars) || 0) + ' 字符；文档 ' + ((c.documents || []).length) + ' 份；绑定技能 ' + ((c.skills || []).length) + ' 个。'));
              const list = el('div');
              for (const d of (c.documents || [])) { list.append(el('div', '__dw_skill', (d.group || '') + ' / ' + (d.name || '') + (d.truncated ? '（已截断）' : ''))); }
              card.append(list);
              if (!stale(my)) body.replaceChildren(card);
            } catch (e) { if (!stale(my)) showError(e); }
          };
          const renderProposals = async (note) => {
            const my = ++renderSeq; activate(proposalTab); body.replaceChildren(el('div', '__dw_muted', '正在加载…'));
            try {
              const ps = (await invoke('dw_list_proposals', { bookId: book.id, status: 'pending' })) || [];
              if (stale(my)) return;
              const wrap = el('div');
              if (note) wrap.appendChild(el('div', '__dw_ok', note));
              if (!ps.length) { wrap.appendChild(el('div', '__dw_card', '当前没有待审提案。')); body.replaceChildren(wrap); return; }
              const grid = el('div', '__dw_grid');
              for (const p of ps) {
                const c = el('button', '__dw_card'); c.type = 'button';
                c.append(el('strong', null, (p.summary || '未命名提案')), el('div', '__dw_muted', (p.groupName || '') + ' / ' + (p.fileName || '') + ' · ' + (p.role || '')));
                c.addEventListener('click', () => renderProposal(p.id));
                grid.append(c);
              }
              wrap.appendChild(grid);
              if (!stale(my)) body.replaceChildren(wrap);
            } catch (e) { if (!stale(my)) showError(e); }
          };
          const renderProposal = (id) => {
            const run = async () => {
              const my = ++renderSeq; activate(proposalTab); body.replaceChildren(el('div', '__dw_muted', '正在读取提案…'));
              try {
                const p = await invoke('dw_get_proposal', { bookId: book.id, id: id });
                if (stale(my)) return;
                const card = el('div', '__dw_card');
                card.append(el('h3', null, p.summary || '变更提案'), el('div', '__dw_muted', '《' + (book.title || book.id) + '》 · ' + (p.groupName || '') + ' / ' + (p.fileName || '')));
                if (p.error) card.appendChild(el('div', '__dw_error', '上次应用错误：' + p.error));
                renderDiff(card, p.baseContent || '', p.proposedContent || '');
                const actions = el('div', '__dw_row');
                const accept = el('button', '__dw_btn primary', '接受并写入');
                const reject = el('button', '__dw_btn', '拒绝');
                const back = el('button', '__dw_btn', '返回');
                accept.addEventListener('click', async () => {
                  if (!window.confirm('确认接受提案并写入《' + (book.title || book.id) + '》的 ' + (p.groupName || '') + '/' + (p.fileName || '') + '？')) return;
                  accept.disabled = true; reject.disabled = true;
                  try {
                    await invoke('dw_accept_proposal', { bookId: book.id, id: id });
                    try { refreshReactTree({ force: true, bookId: book.id }); } catch (e) {}
                    await renderProposals('已接受并写入：' + (p.groupName || '') + '/' + (p.fileName || ''));
                  } catch (e) { window.alert('接受失败：' + (e.message || e)); accept.disabled = false; reject.disabled = false; }
                });
                reject.addEventListener('click', async () => {
                  const reason = window.prompt('拒绝原因（可选）', '人工拒绝') || '';
                  try { await invoke('dw_reject_proposal', { bookId: book.id, id: id, reason: reason }); await renderProposals('已拒绝该提案，文件未修改。'); }
                  catch (e) { window.alert('拒绝失败：' + (e.message || e)); }
                });
                back.addEventListener('click', () => renderProposals());
                actions.append(accept, reject, back); card.append(actions);
                if (!stale(my)) { body.replaceChildren(card); try { accept.focus(); } catch (e) {} }
              } catch (e) { if (!stale(my)) showError(e); }
            };
            run();
          };
          agentTab.addEventListener('click', renderAgents); skillTab.addEventListener('click', renderSkills); contextTab.addEventListener('click', renderContext); proposalTab.addEventListener('click', () => renderProposals());
          renderAgents();
        };
        const renderPicker = async () => {
          const my = ++renderSeq;
          sub.textContent = '请选择要操作的书';
          body.replaceChildren(el('div', '__dw_muted', '正在加载书架…'));
          try {
            const books = (await invoke('list_books', {})) || [];
            if (my !== renderSeq || !overlay.isConnected) return;
            if (!books.length) { body.replaceChildren(el('div', '__dw_error', '书架为空：请先创建或导入一本书。')); return; }
            const wrap = el('div');
            wrap.appendChild(el('div', '__dw_muted', '当前页面没有打开具体书籍。请选择一本书进入 DeepWrite 工作台：'));
            for (const b of books) {
              const item = el('button', '__dw_card __dw_pick'); item.type = 'button'; item.dataset.bookId = b.id;
              item.append(el('strong', null, b.title || b.id), el('div', '__dw_muted', (b.genre || '') + ' · ' + (b.chapterCount != null ? b.chapterCount + ' 章' : '')));
              item.addEventListener('click', () => { renderSeq += 1; mount(b); });
              wrap.appendChild(item);
            }
            body.replaceChildren(wrap);
          } catch (e) { if (my === renderSeq && overlay.isConnected) body.replaceChildren(el('div', '__dw_error', '书架加载失败：' + (e && e.message || e))); }
        };
        const book = await currentBook();
        if (book) { mount(book); } else { await renderPicker(); }
      } catch (e) {
        window.alert('DeepWrite 打开失败：' + (e && e.message || e));
      } finally {
        opening = false;
      }
    };
    window.__wx_dw_open = open;
    document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && document.getElementById(OVERLAY_ID)) closeAll(); }, true);
    const ensureBtn = () => { scheduled = false; if (document.getElementById(BTN_ID)) return; const b = el('button'); b.id = BTN_ID; b.type = 'button'; b.title = 'DeepWrite 项目智能体、技能和变更提案'; b.setAttribute('aria-haspopup', 'dialog'); b.setAttribute('aria-expanded', 'false'); b.append(WB.icon('users', 15), el('span', null, 'DeepWrite')); b.addEventListener('click', () => { open(); }); document.documentElement.appendChild(b); };
    const schedule = () => { if (scheduled) return; scheduled = true; setTimeout(ensureBtn, 300); };
    const boot = () => { new MutationObserver(schedule).observe(document.body, { childList: true, subtree: true }); ensureBtn(); setInterval(ensureBtn, 3000); };
    if (document.body) { boot(); } else { document.addEventListener('DOMContentLoaded', boot, { once: true }); }
  };

  // 生产会话恢复：数据通常仍在 sessions/messages，提供显式可见入口，用户确认后才切换。
  const installSessionRecovery = () => {
    const ID = '__wx_session_recovery';
    const open = async () => {
      try {
        const settings = await invoke('get_settings', {});
        const state = JSON.parse(settings.last_state || '{}');
        const bookId = state.bookId;
        if (!bookId) return window.alert('请先选择一本书');
        const rows = await invoke('list_sessions', { bookId });
        if (!Array.isArray(rows)) throw new Error('会话列表不可用，已停止以免误判');
        const old = document.getElementById(ID + '_dialog'); if (old) old.remove();
        const overlay = document.createElement('div'); overlay.id = ID + '_dialog';
        overlay.style.cssText = 'position:fixed;inset:0;z-index:2147483646;background:rgba(0,0,0,.45);display:flex;align-items:center;justify-content:center';
        const panel = document.createElement('div');
        panel.style.cssText = 'background:#fff;color:#18181b;border-radius:12px;padding:18px;width:min(560px,90vw);max-height:70vh;overflow:auto;box-shadow:0 12px 32px #0003';
        const title = document.createElement('h3'); title.textContent = '恢复本书会话'; panel.appendChild(title);
        const note = document.createElement('p'); note.textContent = '当前书 ' + bookId + ' · 共 ' + rows.length + ' 个会话。以下记录来自已保存的会话，不会自动切换。选择后会先核对消息，再刷新页面。请先保存当前未发送的输入。'; panel.appendChild(note);
        if (!rows.length) { const empty = panel.appendChild(document.createElement('p')); empty.textContent = '当前书 ' + bookId + '：0 个会话，暂无可恢复记录。'; const go = panel.appendChild(document.createElement('button')); go.type = 'button'; go.textContent = '创建写作会话'; go.style.cssText = 'margin:8px 0 0;padding:8px 14px;border:1px solid var(--c-border-3,#EFEBE3);border-radius:8px;background:var(--c-white,#fff);cursor:pointer;font-size:12.5px'; go.addEventListener('click', async () => { try { if (!window.confirm('为当前书 ' + bookId + ' 新建一个「写作会话」？')) return; const c = await invoke('create_session', { bookId, title: '写作会话' }); await invoke('set_setting', { key: 'last_state', value: JSON.stringify(Object.assign({}, state, { bookId, session: c.id, view: 'chat' })) }); window.location.reload(); } catch (e2) { window.alert('创建失败：' + (e2.message || e2)); } }); } else rows.forEach(row => {
          const btn = document.createElement('button'); btn.type = 'button';
          btn.textContent = (row.title || '未命名会话') + ' · ' + (row.msgCount || 0) + ' 条消息';
          btn.style.cssText = 'display:block;width:100%;text-align:left;padding:10px;margin:6px 0;border:1px solid #ddd;border-radius:8px;background:#fff;cursor:pointer';
          btn.addEventListener('click', async () => {
            try {
              const messages = await invoke('list_messages', { sessionId: row.id, bookId });
              if (!Array.isArray(messages)) throw new Error('无法读取该会话消息');
              if (!window.confirm('恢复「' + (row.title || '未命名会话') + '」？已核对 ' + messages.length + ' 条消息。页面会刷新，未发送的输入会丢失。')) return;
              await invoke('set_setting', { key: 'last_state', value: JSON.stringify({ ...state, bookId, session: row.id, view: 'chat' }) });
              window.location.reload();
            } catch (e) { window.alert('恢复失败：' + (e.message || e)); }
          }); panel.appendChild(btn);
        });
        const close = document.createElement('button'); close.textContent = '取消'; close.addEventListener('click', () => overlay.remove()); panel.appendChild(close);
        overlay.appendChild(panel); overlay.addEventListener('click', e => { if (e.target === overlay) overlay.remove(); }); document.body.appendChild(overlay);
      } catch (e) { window.alert('读取会话失败：' + (e.message || e)); }
    };
    const ensure = () => {
      if (document.getElementById(ID)) return;
      const host = document.querySelector('.header__actions') || document.querySelector('.mshell__top') || document.querySelector('.librail__bookmain');
      if (!host) return;
      const btn = document.createElement('button'); btn.id = ID; btn.type = 'button'; btn.textContent = '恢复会话'; btn.title = '查看并恢复当前书的历史会话';
      btn.style.cssText = 'margin-left:6px;padding:5px 10px;border:1px solid #ddd;border-radius:8px;background:#fff;cursor:pointer';
      btn.addEventListener('click', open); host.appendChild(btn);
    };
    const boot = () => { ensure(); setInterval(ensure, 3000); };
    if (document.body) boot(); else document.addEventListener('DOMContentLoaded', boot, { once: true });
  };

  // 兜底：注入代码出问题也绝不能拖垮整个页面
  try { installAutoStudio(); } catch (e) { console.warn('[molan] studio', e); }
  try { installAutoDock(); } catch (e) { console.warn('[molan] dock', e); }
  try { installPendingStrip(); } catch (e) { console.warn('[molan] pend', e); }
  try { installSessionRecovery(); } catch (e) { console.warn('[molan] sessions', e); }
  try { installDeepWriteStudio(); } catch (e) { console.warn('[molan] deepwrite', e); }
})();