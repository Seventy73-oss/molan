/** 类型化 IPC 封装：组件只调用这里，不直接拼命令名。 */
import { invoke, stream, type IpcEvent, type StreamOptions } from './ipc';
import type {
  AppInfo,
  ArtifactView,
  Book,
  DeliverResult,
  DocRead,
  Message,
  RunState,
  Session,
  TaskId,
  TaskPreview,
  TreeGroup,
  WriteOp,
  WriteReceipt,
} from './contracts';

export interface SkillSelection {
  primarySkillId?: string;
  supportSkillIds?: string[];
  styleKey?: string;
  humanize?: string;
}

export interface Skill {
  id: string;
  name: string;
  description: string;
  kind: string;
  enabled: boolean;
  builtinKey: string;
  usageMode: string;
  origin: string;
  promptTemplate: string;
  targets: string[];
  rev: number;
  contentHash: string;
}

export interface Channel {
  id: string;
  label: string;
  baseUrl: string;
  modelsUrl?: string;
  model: string;
  models: string[];
  builtin?: boolean;
}

export interface PendingChapter {
  ch: number;
  name: string;
  status: string;
  contentHash?: string;
  chars?: number;
  words?: number;
  outline?: string;
  dependencyStatus?: string | null;
  preview?: string;
  /** 最近一次审稿结论；state 由服务端按被审文本 hash 与当前稿件比较得出。 */
  review?: ReviewState;
}

export interface ReviewState {
  state: 'current' | 'stale' | 'none';
  ok?: boolean;
  flagged?: boolean;
  issues?: string[];
  note?: string;
  bodyHash?: string;
  createdAt?: number;
}

export interface Proposal {
  id: string;
  groupName: string;
  fileName: string;
  summary: string;
  status: string;
  role: string;
  createdAt: number;
  error?: string | null;
  baseContent?: string;
  proposedContent?: string;
}

export const api = {
  appInfo: () => invoke<AppInfo>('app_info'),

  // 书库
  listBooks: () => invoke<Book[]>('list_books'),
  createBook: (title: string, genre: string, pov: string) => invoke<Book & { ok?: boolean; err?: string }>('create_book', { title, genre, pov }),
  updateBookMeta: (bookId: string, title: string, genre: string, status: string) =>
    invoke('update_book_meta', { bookId, title, genre, status }),
  deleteBook: (bookId: string) => invoke('delete_book', { bookId }),
  listBookTrash: () => invoke<Book[]>('list_trash'),
  restoreBook: (id: string) => invoke('restore_trash', { id }),
  importBook: (title: string, genre: string, files: { name: string; content: string }[]) =>
    invoke<{ bookId: string }>('import_book', { title, genre, files }),
  exportBook: (bookId: string, format: 'zip' | 'txt' | 'all') =>
    invoke<{ ok: boolean; base64?: string; name?: string; mime?: string; message?: string }>('web_export', { bookId, format }),

  // 文件
  tree: (bookId: string) => invoke<TreeGroup[]>('scan_tree', { bookId }),
  readDoc: (bookId: string, group: string, name: string) => invoke<DocRead>('doc_read', { bookId, group, name }),
  writeDoc: (p: {
    bookId: string;
    group: string;
    name: string;
    op: WriteOp;
    content: string;
    baseHash?: string | null;
    start?: number;
    end?: number;
    expected?: string;
    idempotencyKey: string;
  }) => invoke<WriteReceipt>('doc_write', p as unknown as Record<string, unknown>),
  docHistory: (bookId: string, group: string, name: string) => invoke<WriteReceipt[]>('doc_history', { bookId, group, name }),
  createFile: (bookId: string, group: string, name: string) => invoke('create_file', { bookId, group, name }),
  renameFile: (bookId: string, group: string, name: string, newName: string) => invoke('rename_file', { bookId, group, name, newName }),
  deleteFile: (bookId: string, group: string, name: string) => invoke<{ ok: boolean; trashId: string }>('delete_file', { bookId, group, name }),
  createFolder: (bookId: string, name: string) => invoke('create_folder', { bookId, name }),
  setFileFlag: (bookId: string, group: string, name: string, flag: 'locked' | 'aiOff', value: boolean) =>
    invoke('set_file_ai_flag', { bookId, group, name, flag, value }),
  listVersions: (bookId: string, group: string, name: string) => invoke<{ ts: number; size: number }[]>('list_versions', { bookId, group, name }),
  readVersion: (bookId: string, group: string, name: string, ts: number) =>
    invoke<{ content: string }>('read_version', { bookId, group, name, ts }),
  listFileTrash: (bookId: string) => invoke<{ id: string; group: string; name: string; deletedAt: number }[]>('list_trash', { bookId }),
  restoreFile: (bookId: string, id: string) => invoke<{ ok: boolean; err?: string }>('restore_trash', { bookId, id }),

  // 会话与消息
  sessions: (bookId: string) => invoke<Session[]>('list_sessions', { bookId }),
  createSession: (bookId: string, title: string) => invoke<Session>('create_session', { bookId, title }),
  renameSession: (bookId: string, sessionId: string, title: string) => invoke('rename_session', { bookId, sessionId, title }),
  messages: (bookId: string, sessionId: string) => invoke<Message[]>('list_messages', { bookId, sessionId }),

  // 任务与运行
  taskPreview: (bookId: string, task: TaskId, skillSelection: SkillSelection, target: Record<string, unknown>, contextFiles: { group: string; name: string }[] = []) =>
    invoke<TaskPreview>('task_preview', { bookId, task, skillSelection, target, contextFiles }),
  agentTurn: (
    args: {
      bookId: string;
      sessionId: string;
      requestId: string;
      message: string;
      task: TaskId;
      target: Record<string, unknown>;
      skillSelection: SkillSelection;
      contextFiles?: { group: string; name: string }[];
      mode?: 'agent' | 'direct';
    },
    onEvent: (e: IpcEvent) => void,
    opts?: StreamOptions,
  ) => stream<{ ok: boolean; runId: string; status: string; messageId?: string; code?: string; error?: string }>('agent_turn', args, onEvent, opts),
  abort: (requestId: string) => invoke('abort_chat', { requestId }),
  runStatus: (bookId: string, sessionId: string, requestId?: string) => invoke<RunState | null>('run_status', { bookId, sessionId, requestId }),

  // 产物
  artifacts: (bookId: string, sessionId?: string) => invoke<ArtifactView[]>('artifact_list', { bookId, sessionId }),
  artifact: (bookId: string, artifactId: string) => invoke<ArtifactView>('artifact_get', { bookId, artifactId }),
  reviewPending: (bookId: string, ch: number) => invoke<{ review: ReviewState }>('review_pending', { bookId, ch }),
  skillDraft: (p: { name: string; description: string; task: string; usage: string }) => invoke<ArtifactView>('skill_draft', p),
  skillDrafts: () => invoke<ArtifactView[]>('skill_drafts', {}),
  skillDraftRevise: (artifactId: string, baseRev: number, content: string) => invoke<ArtifactView>('skill_draft_revise', { artifactId, baseRev, content }),
  skillDraftSave: (artifactId: string, idempotencyKey: string) =>
    invoke<{ delivery: { ok: boolean; replayed?: boolean; result?: { skillId?: string; name?: string } }; artifact: ArtifactView }>('skill_draft_save', { artifactId, idempotencyKey }),
  skillDraftDiscard: (artifactId: string) => invoke<ArtifactView>('skill_draft_discard', { artifactId }),
  reviseArtifact: (bookId: string, artifactId: string, baseRev: number, content: string) =>
    invoke<ArtifactView>('artifact_revise', { bookId, artifactId, baseRev, content }),
  deliver: (p: {
    bookId: string;
    artifactId: string;
    action: string;
    idempotencyKey: string;
    item?: number;
    group?: string;
    name?: string;
    op?: WriteOp;
    baseHash?: string | null;
    start?: number;
    end?: number;
    expected?: string;
    ch?: number;
  }) => invoke<DeliverResult>('artifact_deliver', p as unknown as Record<string, unknown>),

  // 旧形状卡片的兼容动作（服务端仍从持久化消息取内容）
  legacySaveDoc: (bookId: string, messageId: string) => invoke('save_doc', { bookId, messageId }),
  bookSetupContext: (sourceBookId: string, messageId: string) => invoke<Record<string, unknown>>('get_book_setup_context', { sourceBookId, messageId }),
  saveBookSetup: (p: Record<string, unknown>) => invoke<Record<string, unknown>>('save_book_setup_selection', { ...p, confirmed: true }),
  confirmOutline: (bookId: string, ch: number, expectedHash?: string) => invoke<{ ok: boolean; hash: string }>('confirm_outline', { bookId, ch, expectedHash }),

  // 待审 / 提案
  pending: (bookId: string) => invoke<PendingChapter[]>('list_pending_chapters', { bookId }),
  approve: (bookId: string, name: string, expectedHash?: string) =>
    invoke<{ ok: boolean; finalName: string; alreadyApproved?: boolean }>('approve_chapter', { bookId, name, expectedHash }),
  reject: (bookId: string, ch: number, name: string) => invoke<{ ok: boolean; trashId: string }>('reject_chapter', { bookId, ch, name }),
  proposals: (bookId: string) => invoke<Proposal[]>('dw_list_proposals', { bookId, status: 'pending' }),
  proposal: (bookId: string, id: string) => invoke<Proposal>('dw_get_proposal', { bookId, id }),
  acceptProposal: (bookId: string, id: string) => invoke<{ ok: boolean; indexError?: string }>('dw_accept_proposal', { bookId, id }),
  rejectProposal: (bookId: string, id: string, reason: string) => invoke('dw_reject_proposal', { bookId, id, reason }),

  // 生产线 / 记忆 / 单章 / 自动写作
  pipeline: (bookId: string) => invoke<PipelineState>('get_pipeline_state', { bookId }),
  memoryStatus: (bookId: string) => invoke<Record<string, unknown>>('memory_status', { bookId }),
  rebuildMemory: (bookId: string, ch?: number) => invoke<{ note?: string }>('rebuild_memory', { bookId, ch }),
  draftChapter: (bookId: string, ch: number, requestId: string, sessionId: string | null, skillSelection: SkillSelection, instruction: string, onEvent: (e: IpcEvent) => void) =>
    stream<Record<string, unknown>>('draft_chapter', { bookId, ch, requestId, sessionId: sessionId ?? '', skillSelection, instruction }, onEvent),
  autoStatus: (bookId: string) => invoke<AutoStatus>('auto_write_status', { bookId }),
  autoStart: (p: { bookId: string; sessionId: string; fromCh: number; toCh: number; fullAuto: boolean; confirmAuto: boolean; skillSelection: SkillSelection }) =>
    invoke('auto_write_start', { ...p, confirmed: true }),
  autoStop: (bookId: string) => invoke<{ ok: boolean; reason?: string }>('auto_write_stop', { bookId }),
  autoResume: (bookId: string, confirmAuto: boolean) => invoke('auto_write_resume', { bookId, confirmed: true, confirmAuto }),

  // 技能
  skills: () => invoke<Skill[]>('list_skills'),
  createSkill: (s: Partial<Skill>) => invoke<Skill>('create_skill', s as Record<string, unknown>),
  updateSkill: (s: Partial<Skill> & { id: string }) => invoke<Skill>('update_skill', s as Record<string, unknown>),
  setSkillEnabled: (id: string, on: boolean) => invoke('set_skill_enabled', { id, on }),
  deleteSkill: (id: string) => invoke('delete_skill', { id }),
  skillRevisions: (id: string) => invoke<{ rev: number; contentHash: string; ts: number; sourceEvent: string; promptTemplate: string }[]>('skill_revisions', { id }),
  setBookPrimary: (bookId: string, taskKind: TaskId, skillId: string) => invoke('set_book_primary_skill', { bookId, taskKind, skillId }),
  setBookSupports: (bookId: string, taskKind: TaskId, skillIds: string[]) => invoke('set_book_support_skills', { bookId, taskKind, skillIds }),
  bookBindings: (bookId: string) => invoke<Record<string, { primary: string; supports: string[] }>>('book_skill_bindings', { bookId }),
  genreStyles: () => invoke<{ key: string; label: string }[]>('list_genre_styles'),
  bookStyle: (bookId: string) => invoke<unknown>('get_book_style', { bookId }),
  setBookStyle: (bookId: string, key: string) => invoke('set_book_style', { bookId, key }),
  setBookHumanize: (bookId: string, value: string) => invoke('set_book_humanize', { bookId, value }),

  // 设置
  settings: () => invoke<Record<string, string>>('get_settings'),
  setSetting: (key: string, value: string) => invoke('set_setting', { key, value }),
  setSettings: (entries: Record<string, string>) => invoke('set_settings', { entries }),
  hasChannelKey: (id: string) => invoke<boolean>('has_channel_key', { id }),
  setChannelKey: (id: string, key: string) => invoke('set_channel_key', { id, key }),
  deleteChannelKey: (id: string) => invoke('delete_channel_key', { id }),
  listChannelModels: (id: string) => invoke<string[]>('list_models_channel', { id }),
  testChannel: (id: string, model?: string) => invoke<{ ok: boolean; output: string; totalMs?: number }>('channel_test', { id, model }),
  agentProfiles: () => invoke<{ profiles: Record<string, { channelId: string; model: string }>; channels: { id: string; label: string; model: string }[] }>('get_agent_profiles'),
  setAgentProfile: (task: string, channelId: string, model: string) => invoke('set_agent_profile', { task, channelId, model }),
  usage: () => invoke<{ byModel: { model: string; tag: string; calls: number; totalTokens: number }[]; byDay: { day: number; calls: number; totalTokens: number }[] }>('llm_usage'),

  // 书源
  bookSources: () => invoke<{ id: string; name: string }[]>('list_book_sources'),
  searchBooks: (sourceId: string, keyword: string) => invoke<{ title: string; author?: string; url: string; intro?: string }[]>('search_books', { sourceId, keyword }),
  bookCatalog: (sourceId: string, bookUrl: string) => invoke<{ chapters: { title: string; url: string }[] }>('fetch_book_catalog', { sourceId, bookUrl }),
  chapterTexts: (sourceId: string, chapters: { title: string; url: string }[], onEvent: (e: IpcEvent) => void) =>
    stream<string>('fetch_chapter_texts', { sourceId, chapters }, onEvent),
};

export interface PipelineChapter {
  n: number;
  body: string;
  memory: string;
  outline: string;
  outlineStatus?: string;
  approvalVerified?: boolean;
}

export interface PipelineState {
  hasOutline: boolean;
  hasSetup: boolean;
  chapters: PipelineChapter[];
  counts: { total: number; approved: number; pending: number; unverifiedFormal?: number };
  next: { stage: string; chapter?: number; blocked?: boolean } | null;
  blockers: { type: string; chapter?: number; status?: string }[];
  nextContext?: { label: string }[];
  summaryDue?: boolean;
}

export interface AutoStatus {
  running: boolean;
  status?: string;
  bookId?: string;
  curCh?: number;
  current?: number;
  totalCh?: number;
  to?: number;
  from?: number;
  resumable?: boolean;
  error?: string;
  logs?: { step: string; text: string; ts: number }[];
}

/** base64 → 下载（应用内，不经第三方）。 */
export function downloadBase64(name: string, mime: string, b64: string) {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  const url = URL.createObjectURL(new Blob([bytes], { type: mime || 'application/octet-stream' }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 2000);
}
