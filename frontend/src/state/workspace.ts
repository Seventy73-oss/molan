/**
 * 工作台状态（作品作用域）。所有异步加载都带作用域令牌：切书/切文档后迟到的旧响应一律丢弃，
 * 不会覆盖新界面。文档保存状态机：clean → dirty → saving → saved/conflict/failed；
 * 保存的是发起时的快照，保存期间的新编辑保持 dirty，不会被清掉。
 */
import { create } from 'zustand';
import { api, type SkillSelection } from '../lib/api';
import type { ArtifactView, Book, DocRead, Message, RunState, Session, TaskId, TreeGroup, WriteReceipt } from '../lib/contracts';
import { clearDraft, judgeDraft, loadDraft, saveDraft, type DraftVerdict } from '../lib/drafts';
import { errorText, newRequestId } from '../lib/ipc';
import { isTerminal, reduceRun, startRun, type RunView } from './run';

export type SaveState = 'clean' | 'dirty' | 'saving' | 'saved' | 'conflict' | 'failed';

export interface DocState {
  group: string;
  name: string;
  server: DocRead | null;
  text: string;
  baseHash: string | null;
  save: SaveState;
  error?: string;
  conflict?: { serverText: string; serverHash: string | null };
  draft?: Extract<DraftVerdict, { kind: 'restorable' | 'diverged' }>;
  readOnly: boolean;
  loading: boolean;
  lastReceipt?: WriteReceipt;
}

export interface Target {
  ch?: number;
  group?: string;
  name?: string;
  baseHash?: string;
  start?: number;
  end?: number;
  selectionText?: string;
  label: string;
}

export interface Composer {
  task: TaskId;
  target: Target | null;
  selection: SkillSelection;
  mode: 'agent' | 'direct';
  files: { group: string; name: string }[];
}

export type Panel = 'assistant' | 'pending' | 'pipeline' | 'context';

interface WsState {
  bookId: string | null;
  book: Book | null;
  tree: TreeGroup[];
  treeError?: string;
  doc: DocState | null;
  sessions: Session[];
  sessionId: string | null;
  messages: Message[];
  artifacts: ArtifactView[];
  sessionLoading: boolean;
  run: RunView | null;
  remoteRun: RunState | null;
  panel: Panel;
  composer: Composer;
  pendingCount: number;
  leftOpen: boolean;
  rightOpen: boolean;
  focus: boolean;
  version: number;

  openBook: (bookId: string, books?: Book[]) => Promise<void>;
  closeBook: () => void;
  refreshTree: () => Promise<void>;
  refreshPendingCount: () => Promise<void>;
  openDoc: (group: string, name: string) => Promise<void>;
  closeDoc: () => void;
  setText: (text: string) => void;
  saveDoc: (opts?: { force?: boolean }) => Promise<WriteReceipt | null>;
  reloadDoc: () => Promise<void>;
  restoreDraft: () => void;
  discardDraft: () => void;
  selectSession: (sessionId: string) => Promise<void>;
  newSession: (title?: string) => Promise<void>;
  reloadSession: () => Promise<void>;
  send: (message: string) => Promise<void>;
  stop: () => Promise<void>;
  upsertArtifact: (a: ArtifactView) => void;
  setPanel: (p: Panel) => void;
  setComposer: (c: Partial<Composer>) => void;
  toggle: (k: 'leftOpen' | 'rightOpen' | 'focus', v?: boolean) => void;
}

const DEFAULT_COMPOSER: Composer = { task: 'chat', target: null, selection: {}, mode: 'agent', files: [] };

let scope = 0; // 作品作用域令牌
let docScope = 0; // 文档作用域令牌
let sessionScope = 0;
let draftTimer: ReturnType<typeof setTimeout> | undefined;

export const TASK_LABELS: Record<TaskId, string> = {
  chat: '聊天',
  plot: '剧情推演',
  outline: '细纲',
  body: '正文',
  revise: '修改',
  review: '审稿',
  humanize: '去AI味',
  summary: '总结',
  distill: '蒸馏',
};

export const useWs = create<WsState>((set, get) => ({
  bookId: null,
  book: null,
  tree: [],
  doc: null,
  sessions: [],
  sessionId: null,
  messages: [],
  artifacts: [],
  sessionLoading: false,
  run: null,
  remoteRun: null,
  panel: 'assistant',
  composer: DEFAULT_COMPOSER,
  pendingCount: 0,
  leftOpen: true,
  rightOpen: true,
  focus: false,
  version: 0,

  openBook: async (bookId, books) => {
    const my = ++scope;
    docScope++;
    sessionScope++;
    const known = books?.find((b) => b.id === bookId) ?? null;
    set({ bookId, book: known, tree: [], doc: null, sessions: [], sessionId: null, messages: [], artifacts: [], run: null, remoteRun: null, composer: DEFAULT_COMPOSER, treeError: undefined });
    try {
      const [book, tree, sessions] = await Promise.all([
        known ? Promise.resolve(known) : api.listBooks().then((bs) => bs.find((b) => b.id === bookId) ?? null),
        api.tree(bookId),
        api.sessions(bookId),
      ]);
      if (my !== scope) return;
      set({ book, tree, sessions });
      void get().refreshPendingCount();
      if (sessions[0]) await get().selectSession(sessions[0].id);
    } catch (e) {
      if (my === scope) set({ treeError: errorText(e) });
    }
  },

  closeBook: () => {
    scope++;
    docScope++;
    sessionScope++;
    set({ bookId: null, book: null, tree: [], doc: null, sessions: [], sessionId: null, messages: [], artifacts: [], run: null });
  },

  refreshTree: async () => {
    const { bookId } = get();
    if (!bookId) return;
    const my = scope;
    try {
      const tree = await api.tree(bookId);
      if (my === scope) set({ tree, treeError: undefined });
    } catch (e) {
      if (my === scope) set({ treeError: errorText(e) });
    }
  },

  refreshPendingCount: async () => {
    const { bookId } = get();
    if (!bookId) return;
    const my = scope;
    try {
      const [p, props] = await Promise.all([api.pending(bookId), api.proposals(bookId).catch(() => [])]);
      if (my === scope) set({ pendingCount: p.length + props.length });
    } catch {
      /* 计数失败不影响写作 */
    }
  },

  openDoc: async (group, name) => {
    const { bookId, doc } = get();
    if (!bookId) return;
    if (doc && doc.save === 'dirty') {
      // 切换前先保存作者的未保存编辑（失败则保留在本地草稿并提示，不丢稿）
      await get().saveDoc();
    }
    const my = ++docScope;
    const readOnly = group === '正文待审';
    set({ doc: { group, name, server: null, text: '', baseHash: null, save: 'clean', readOnly, loading: true } });
    try {
      const d = await api.readDoc(bookId, group, name);
      if (my !== docScope) return;
      const verdict = judgeDraft(loadDraft(bookId, d.group, d.name), d.hash, d.content);
      set({
        doc: {
          group: d.group,
          name: d.name,
          server: d,
          text: d.content,
          baseHash: d.hash,
          save: 'clean',
          readOnly: readOnly || d.group === '正文待审',
          loading: false,
          draft: verdict.kind === 'restorable' || verdict.kind === 'diverged' ? verdict : undefined,
        },
      });
    } catch (e) {
      if (my === docScope) set({ doc: { group, name, server: null, text: '', baseHash: null, save: 'failed', error: errorText(e), readOnly: true, loading: false } });
    }
  },

  closeDoc: () => {
    docScope++;
    set({ doc: null });
  },

  setText: (text) => {
    const { doc, bookId } = get();
    if (!doc || doc.readOnly || !bookId) return;
    const dirty = text !== (doc.server?.content ?? '') || doc.baseHash !== (doc.server?.hash ?? null);
    set({ doc: { ...doc, text, save: doc.save === 'saving' ? 'saving' : dirty ? 'dirty' : 'clean', draft: undefined } });
    clearTimeout(draftTimer);
    draftTimer = setTimeout(() => {
      const cur = get().doc;
      if (!cur || !get().bookId) return;
      if (cur.text === (cur.server?.content ?? '')) clearDraft(bookId, cur.group, cur.name);
      else saveDraft({ bookId, group: cur.group, name: cur.name, baseHash: cur.baseHash, text: cur.text, savedAt: Date.now() });
    }, 700);
  },

  saveDoc: async (opts = {}) => {
    const { doc, bookId } = get();
    if (!doc || !bookId || doc.readOnly || doc.loading) return null;
    if (doc.save === 'saving') return null;
    if (doc.save !== 'dirty' && !opts.force) return null;
    const snapshot = doc.text;
    const base = opts.force && doc.conflict ? doc.conflict.serverHash : doc.baseHash;
    const my = docScope;
    set({ doc: { ...doc, save: 'saving', error: undefined } });
    try {
      const exists = !!doc.server?.exists;
      const r = await api.writeDoc({
        bookId,
        group: doc.group,
        name: doc.name,
        op: exists ? 'replace' : 'create',
        content: snapshot,
        baseHash: exists ? base : null,
        idempotencyKey: newRequestId('save'),
      });
      if (my !== docScope) return r;
      const cur = get().doc!;
      if (r.commit === 'committed' || r.commit === 'noop') {
        const server: DocRead = {
          ...(cur.server ?? ({} as DocRead)),
          exists: true,
          bookId,
          group: r.group,
          name: r.name,
          content: snapshot,
          hash: r.afterHash,
          revision: r.revision,
          chars: r.chars,
          utf16Len: snapshot.length,
          locked: cur.server?.locked ?? false,
          aiOff: cur.server?.aiOff ?? false,
        };
        const stillDirty = cur.text !== snapshot;
        if (!stillDirty) clearDraft(bookId, r.group, r.name);
        set({
          doc: {
            ...cur,
            server,
            baseHash: r.afterHash,
            save: stillDirty ? 'dirty' : 'saved',
            conflict: undefined,
            lastReceipt: r,
            error: r.index === 'failed' ? `已保存，但索引登记失败：${r.indexError ?? ''}` : undefined,
          },
        });
        if (!exists) void get().refreshTree();
        return r;
      }
      if (r.commit === 'conflict' && r.error?.code === 'BASE_CHANGED') {
        const fresh = await api.readDoc(bookId, doc.group, doc.name);
        if (my !== docScope) return r;
        set({ doc: { ...get().doc!, save: 'conflict', conflict: { serverText: fresh.content, serverHash: fresh.hash }, error: r.error.message } });
        return r;
      }
      set({ doc: { ...get().doc!, save: r.commit === 'conflict' ? 'conflict' : 'failed', error: r.error?.message ?? '保存失败' } });
      return r;
    } catch (e) {
      if (my === docScope) set({ doc: { ...get().doc!, save: 'failed', error: errorText(e) } });
      return null;
    }
  },

  reloadDoc: async () => {
    const { doc, bookId } = get();
    if (!doc || !bookId) return;
    const my = docScope;
    const d = await api.readDoc(bookId, doc.group, doc.name);
    if (my !== docScope) return;
    clearDraft(bookId, d.group, d.name);
    set({ doc: { ...doc, server: d, text: d.content, baseHash: d.hash, save: 'clean', conflict: undefined, error: undefined, draft: undefined } });
  },

  restoreDraft: () => {
    const { doc } = get();
    if (!doc?.draft || (doc.draft.kind !== 'restorable' && doc.draft.kind !== 'diverged')) return;
    const text = doc.draft.draft.text;
    if (doc.draft.kind === 'restorable') {
      set({ doc: { ...doc, text, save: 'dirty', draft: undefined } });
    } else {
      // 基线已变：作为冲突处理，交给作者比较后决定
      set({ doc: { ...doc, text, save: 'conflict', conflict: { serverText: doc.server?.content ?? '', serverHash: doc.server?.hash ?? null }, draft: undefined } });
    }
  },

  discardDraft: () => {
    const { doc, bookId } = get();
    if (!doc || !bookId) return;
    clearDraft(bookId, doc.group, doc.name);
    set({ doc: { ...doc, draft: undefined } });
  },

  selectSession: async (sessionId) => {
    const { bookId } = get();
    if (!bookId) return;
    const my = ++sessionScope;
    set({ sessionId, sessionLoading: true, messages: [], artifacts: [], remoteRun: null, run: get().run?.sessionId === sessionId ? get().run : null });
    try {
      const [messages, artifacts, remoteRun] = await Promise.all([
        api.messages(bookId, sessionId),
        api.artifacts(bookId, sessionId),
        api.runStatus(bookId, sessionId).catch(() => null),
      ]);
      if (my !== sessionScope) return;
      set({ messages, artifacts, remoteRun, sessionLoading: false });
    } catch (e) {
      if (my === sessionScope) set({ sessionLoading: false, treeError: errorText(e) });
    }
  },

  newSession: async (title = '创作会话') => {
    const { bookId } = get();
    if (!bookId) return;
    const my = scope;
    const s = await api.createSession(bookId, title);
    if (my !== scope) return; // 创建期间已切换作品：新会话属于旧作品，不得进入当前作品的列表
    set({ sessions: [s, ...get().sessions] });
    await get().selectSession(s.id);
  },

  reloadSession: async () => {
    const { sessionId } = get();
    if (sessionId) await get().selectSession(sessionId);
  },

  send: async (message) => {
    const { bookId, sessionId, composer, run } = get();
    if (!bookId || !message.trim()) return;
    if (run && !isTerminal(run.status)) return; // 防重入：同一会话一次一个运行（服务端另有硬约束）
    const myBook = scope; // 作品作用域：任何 await 之后若已切书，旧作品的结果不得写入当前视图
    let sid = sessionId;
    if (!sid) {
      await get().newSession(message.slice(0, 18));
      if (myBook !== scope) return;
      sid = get().sessionId;
    }
    if (!sid) return;
    const requestId = newRequestId('run');
    const target: Record<string, unknown> = {};
    if (composer.target) {
      for (const k of ['ch', 'group', 'name', 'baseHash', 'start', 'end', 'selectionText'] as const) {
        if (composer.target[k] !== undefined) target[k] = composer.target[k];
      }
    }
    const optimistic: Message = {
      id: `local-${requestId}`,
      sessionId: sid,
      role: 'user',
      content: message,
      context: { task: composer.task, target },
      steps: [],
      result: null,
      createdAt: Date.now(),
      interrupted: false,
    };
    set({ run: startRun({ requestId, sessionId: sid, task: composer.task, taskLabel: TASK_LABELS[composer.task] }), messages: [...get().messages, optimistic] });
    const mySession = sessionScope;
    try {
      const r = await api.agentTurn(
        { bookId, sessionId: sid, requestId, message, task: composer.task, target, skillSelection: composer.selection, contextFiles: composer.files, mode: composer.mode },
        (e) => {
          const cur = get().run;
          if (myBook !== scope || !cur || cur.requestId !== requestId) return;
          const next = reduceRun(cur, e);
          set({ run: next });
          if (e.type === 'artifact' && e.artifact) get().upsertArtifact(e.artifact as ArtifactView);
        },
      );
      const cur = get().run;
      if (myBook === scope && cur?.requestId === requestId && r && (r.code === 'CONTEXT_BLOCKED' || r.status === 'session_busy')) {
        set({ run: { ...cur, status: r.status === 'session_busy' ? 'session_busy' : 'error', error: cur.error ?? { code: r.code, message: r.error ?? '无法开始' } } });
      }
    } catch (e) {
      const cur = get().run;
      if (myBook === scope && cur?.requestId === requestId) set({ run: { ...cur, status: 'error', error: { message: errorText(e) } } });
    } finally {
      // 以服务端为准刷新：消息、产物投影、运行状态（仍在同一作品、同一会话时）
      if (myBook === scope && mySession === sessionScope && get().sessionId === sid) {
        const my = ++sessionScope;
        const [messages, artifacts, remoteRun] = await Promise.all([
          api.messages(bookId, sid).catch(() => get().messages),
          api.artifacts(bookId, sid).catch(() => get().artifacts),
          api.runStatus(bookId, sid).catch(() => null),
        ]);
        if (my === sessionScope) {
          const cur = get().run;
          set({ messages, artifacts, remoteRun, run: cur && cur.requestId === requestId ? { ...cur, status: isTerminal(cur.status) ? cur.status : 'done' } : cur });
        }
      }
      if (myBook === scope) {
        void get().refreshTree();
        void get().refreshPendingCount();
      }
    }
  },

  stop: async () => {
    const run = get().run;
    if (run && !isTerminal(run.status)) {
      try {
        await api.abort(run.requestId);
      } catch {
        /* 停止请求失败时运行仍在服务端：由状态查询如实反映 */
      }
    } else if (get().remoteRun?.live && get().remoteRun?.requestId) {
      await api.abort(get().remoteRun!.requestId).catch(() => undefined);
    }
  },

  upsertArtifact: (a) => {
    const list = get().artifacts;
    const i = list.findIndex((x) => x.id === a.id);
    const next = i < 0 ? [...list, a] : list.map((x) => (x.id === a.id ? a : x));
    const run = get().run;
    set({
      artifacts: next,
      run: run ? { ...run, artifacts: run.artifacts.map((x) => (x.id === a.id ? a : x)) } : run,
      version: get().version + 1,
    });
  },

  setPanel: (panel) => set({ panel, rightOpen: true }),
  setComposer: (c) => set({ composer: { ...get().composer, ...c } }),
  toggle: (k, v) => set({ [k]: v ?? !get()[k] } as Partial<WsState>),
}));
