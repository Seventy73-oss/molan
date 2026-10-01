/**
 * Paper Studio 契约 v2（与 Rust 端 contracts/fixtures/*.json 同源；契约测试逐个校验）。
 * 状态与是否已保存一律来自服务端回执/投影，前端不自行推断「已保存」。
 */

export const CONTRACT_VERSION = 2;

export type TaskId =
  | 'chat'
  | 'plot'
  | 'outline'
  | 'body'
  | 'revise'
  | 'review'
  | 'humanize'
  | 'summary'
  | 'distill';

export interface TaskInfo {
  id: TaskId;
  label: string;
  role: string;
  artifactKind: string | null;
  rewritesTarget: boolean;
}

export interface AppInfo {
  app: string;
  ui: string;
  contract: number;
  version: string;
  authRequired: boolean;
  tasks: TaskInfo[];
  writeOps: WriteOp[];
}

// ---------- 书 / 文件 ----------

export interface Book {
  id: string;
  title: string;
  genre: string | null;
  pov: string | null;
  status: string | null;
  coverChar: string | null;
  wordCount: number;
  chapterCount: number;
  createdAt: number | null;
  updatedAt: number | null;
}

export interface TreeFile {
  name: string;
  size: string;
  ts: number;
  locked?: boolean;
  aiOff?: boolean;
}

export interface TreeGroup {
  key: string;
  label: string;
  dir: string;
  groupDir: string;
  icon: string;
  custom?: boolean;
  files: TreeFile[];
}

export interface DocRead {
  exists: boolean;
  bookId: string;
  group: string;
  name: string;
  content: string;
  hash: string | null;
  chars: number;
  utf16Len: number;
  revision: number | null;
  locked: boolean;
  aiOff: boolean;
}

export type WriteOp = 'create' | 'replace' | 'append' | 'insert' | 'replace_range';
export type CommitState = 'committed' | 'noop' | 'conflict' | 'failed';

export interface WriteError {
  code: string;
  message: string;
  currentHash?: string;
}

export interface WriteReceipt {
  writeId: string;
  idempotencyKey: string;
  bookId: string;
  group: string;
  name: string;
  op: WriteOp;
  actor: 'user' | 'ai';
  commit: CommitState;
  beforeHash: string | null;
  afterHash: string | null;
  revision: number | null;
  index: 'ok' | 'failed' | 'skipped';
  indexError: string | null;
  error: WriteError | null;
  replayed: boolean;
  recovered: boolean;
  chars: number;
  ts: number;
  source: unknown;
  /** doc_write 命令附带的服务端写入耗时（ms）。 */
  durationMs?: number;
}

// ---------- 技能计划 / 上下文 ----------

export interface PlannedSkill {
  id: string;
  name: string;
  kind: string;
  usageMode: string;
  origin: string;
  targets: string[];
  rev: number;
  contentHash: string;
  role: 'primary' | 'support';
  source: 'explicit' | 'book_primary' | 'book_support' | 'auto_match';
  templateChars: number;
  description?: string;
}

export interface ExcludedSkill {
  id: string;
  name: string;
  source: string;
  code: string;
  reason: string;
}

export interface StylePlan {
  key: string;
  label: string;
  source: string;
  contentHash: string;
  chars: number;
  note: string;
  fromOverride: boolean;
}

export interface HumanizePlan {
  method: string;
  label: string;
  contentHash: string;
  chars: number;
  note: string;
  fromOverride: boolean;
}

export interface SkillPlanView {
  task: TaskId;
  taskLabel: string;
  bookId?: string;
  genre?: string | null;
  skills: PlannedSkill[];
  excluded: ExcludedSkill[];
  notes: string[];
  style: StylePlan;
  humanize: HumanizePlan;
  planHash: string;
  styleOverride: string | null;
  humanizeOverride: string | null;
}

export interface ContextBlock {
  label: string;
  source: string;
  chars: number;
  hash?: string;
  truncated?: boolean;
  omitted?: number;
  required?: boolean;
  skipped?: string;
}

export interface ContextPreview {
  blocks: ContextBlock[];
  blockers: string[];
  totalChars: number;
  ch: number;
}

export interface Recommendation {
  skillId: string;
  name: string;
  kind: string;
  action: 'set_primary' | 'add_support';
  reason: string;
}

export interface TaskPreview {
  task: TaskId;
  taskLabel: string;
  role: string;
  plan: SkillPlanView;
  context: ContextPreview;
  recommend: Recommendation[];
  note: string;
}

// ---------- 产物 ----------

export type ArtifactState =
  | 'generating'
  | 'generated'
  | 'interrupted'
  | 'failed'
  | 'discarded'
  | 'base_changed'
  | 'saved'
  | 'partial'
  | 'pending_review'
  | 'confirmed'
  | 'approved'
  | 'rejected'
  | 'stale'
  | 'conflict';

export interface ArtifactAction {
  id: string;
  label: string;
  primary: boolean;
}

export interface ArtifactLocation {
  group: string | null;
  name: string | null;
  ch: number | null;
}

export interface ArtifactItem {
  index: number;
  title: string | null;
  group: string | null;
  name: string | null;
  chars: number;
  hash: string;
  state: ArtifactState;
  stateLabel: string;
  note: string;
  location: ArtifactLocation | null;
  content: string | null;
}

export interface ArtifactDelivery {
  id: string;
  rev: number;
  item: number;
  action: string;
  status: CommitState | string;
  group: string;
  name: string;
  ch: number | null;
  op: string;
  afterHash: string;
  writeId: string;
  createdAt: number;
  detail: Record<string, unknown>;
}

export interface ArtifactTarget {
  ch?: number;
  group?: string;
  name?: string;
  baseHash?: string;
  start?: number;
  end?: number;
  selectionText?: string;
}

export interface ArtifactProvenance {
  planHash?: string;
  model?: string;
  manifestId?: string;
  runId?: string;
  skills?: { id: string; name: string; rev: number; role: string; source: string }[];
  style?: { key: string; label: string };
  humanize?: { method: string; label: string };
  excluded?: ExcludedSkill[];
}

export interface ArtifactView {
  id: string;
  bookId?: string;
  sessionId: string;
  messageId: string;
  runId?: string;
  kind: string;
  kindLabel: string;
  task?: TaskId;
  taskLabel?: string;
  title: string;
  scope: 'document' | 'fragment';
  format?: string;
  lifecycle?: string;
  rev: number;
  contentHash?: string;
  chars: number;
  origin?: string;
  content: string;
  truncated: boolean;
  items: ArtifactItem[];
  item?: ArtifactItem | null;
  state: ArtifactState;
  stateLabel: string;
  summary: string;
  target: ArtifactTarget;
  provenance: ArtifactProvenance | null;
  actions: ArtifactAction[];
  deliveries: ArtifactDelivery[];
  createdAt: number;
  updatedAt?: number;
  legacy: boolean;
  legacyRef?: Record<string, unknown>;
}

export interface DeliverResult {
  delivery: {
    ok: boolean;
    action: string;
    status: string;
    receipt?: WriteReceipt;
    result?: Record<string, unknown>;
    deliveryId?: string;
    replayed?: boolean;
  };
  artifact: ArtifactView;
  /** 交付（写入 / 提交 / 定稿）的服务端耗时（ms）。 */
  ms?: number;
}

// ---------- 运行 ----------

export type RunStateName = 'running' | 'completed' | 'interrupted' | 'failed' | 'budget_exhausted';

export interface RunState {
  runId: string;
  requestId: string;
  sessionId: string;
  status: string;
  state: RunStateName;
  stateLabel: string;
  code: string;
  live: boolean;
  task: string;
  mode: string;
  model: string;
  error: string;
  toolRound: number;
  maxToolRounds: number;
  usedTokens: number;
  budgetTokens: number;
  usageEstimated: boolean;
  planHash: string;
  manifestId: string;
  metrics?: RunMetrics | null;
  createdAt: number;
  updatedAt: number;
}

/** 运行指标：首字时间从收到请求起算；用量来源 upstream=上游报告，estimated=按字数估算。 */
export interface RunMetrics {
  totalMs: number;
  firstTokenMs: number | null;
  modelCalls: {
    firstTokenMs: number | null;
    firstTextMs: number | null;
    totalMs: number;
    promptChars: number;
    outputChars: number;
    outcome: string;
    usage: unknown;
    usageSource: 'upstream' | 'estimated';
  }[];
  tools: { name: string; ms: number; cached: boolean; parallel: boolean; ok: boolean }[];
  toolMs: number;
  cacheHits: number;
  retries: number;
  usageEstimated: boolean;
}

/** 指标一行摘要（运行卡与历史消息共用）。 */
export function metricsLine(m: RunMetrics | null | undefined): string {
  if (!m || !Array.isArray(m.modelCalls)) return '';
  const parts: string[] = [];
  if (typeof m.firstTokenMs === 'number') parts.push(`首字 ${fmtMs(m.firstTokenMs)}`);
  parts.push(`模型 ${m.modelCalls.length} 次`);
  if (m.tools?.length) parts.push(`工具 ${m.tools.length} 次${m.cacheHits ? `（缓存命中 ${m.cacheHits}）` : ''}`);
  if (m.retries) parts.push(`重试 ${m.retries} 次`);
  parts.push(`总耗时 ${fmtMs(m.totalMs)}`);
  if (m.usageEstimated) parts.push('用量含估算');
  return parts.join(' · ');
}

function fmtMs(ms: number): string {
  return ms >= 1000 ? `${(ms / 1000).toFixed(1)}s` : `${ms}ms`;
}

// ---------- 会话 / 消息 ----------

export interface Session {
  id: string;
  bookId: string;
  title: string;
  preview?: string;
  msgCount?: number;
  updatedAt?: number;
}

export interface StepRecord {
  type: string;
  callId?: string;
  name?: string;
  status?: string;
  summary?: string;
  artifact?: Record<string, unknown> | null;
}

export interface Message {
  id: string;
  sessionId: string;
  role: 'user' | 'assistant' | 'system';
  content: string;
  context: Record<string, unknown>;
  steps: StepRecord[];
  result: Record<string, unknown> | null;
  createdAt: number;
  interrupted: boolean;
}

// ---------- 运行时校验（契约测试与关键入口使用） ----------

type Guard = (v: unknown) => string | null;

const isObj = (v: unknown): v is Record<string, unknown> => !!v && typeof v === 'object' && !Array.isArray(v);

function shape(fields: Record<string, 'string' | 'number' | 'boolean' | 'object' | 'array' | 'any' | 'string|null' | 'number|null' | 'object|null'>): Guard {
  return (v) => {
    if (!isObj(v)) return '不是对象';
    for (const [k, t] of Object.entries(fields)) {
      const x = v[k];
      const ok = (() => {
        switch (t) {
          case 'any':
            return k in v;
          case 'array':
            return Array.isArray(x);
          case 'object':
            return isObj(x);
          case 'string|null':
            return x === null || typeof x === 'string';
          case 'number|null':
            return x === null || typeof x === 'number';
          case 'object|null':
            return x === null || isObj(x);
          default:
            return typeof x === t;
        }
      })();
      if (!ok) return `字段 ${k} 应为 ${t}`;
    }
    return null;
  };
}

export const checkWriteReceipt = shape({
  writeId: 'string',
  idempotencyKey: 'string',
  bookId: 'string',
  group: 'string',
  name: 'string',
  op: 'string',
  commit: 'string',
  beforeHash: 'string|null',
  afterHash: 'string|null',
  revision: 'number|null',
  index: 'string',
  error: 'object|null',
  replayed: 'boolean',
  recovered: 'boolean',
});

export const checkDocRead = shape({
  exists: 'boolean',
  content: 'string',
  hash: 'string|null',
  utf16Len: 'number',
  revision: 'number|null',
  locked: 'boolean',
  aiOff: 'boolean',
});

export const checkArtifactView = shape({
  id: 'string',
  kind: 'string',
  kindLabel: 'string',
  title: 'string',
  scope: 'string',
  rev: 'number',
  content: 'string',
  items: 'array',
  state: 'string',
  stateLabel: 'string',
  summary: 'string',
  target: 'object',
  actions: 'array',
  deliveries: 'array',
  legacy: 'boolean',
});

export const checkRunState = shape({
  runId: 'string',
  status: 'string',
  state: 'string',
  stateLabel: 'string',
  live: 'boolean',
  usedTokens: 'number',
  usageEstimated: 'boolean',
});

export const checkTaskPreview = (v: unknown): string | null => {
  const top = shape({ task: 'string', plan: 'object', context: 'object', recommend: 'array' })(v);
  if (top) return top;
  const o = v as TaskPreview;
  return (
    shape({ skills: 'array', excluded: 'array', style: 'object', humanize: 'object', planHash: 'string' })(o.plan) ??
    shape({ blocks: 'array', blockers: 'array', totalChars: 'number' })(o.context)
  );
};

export const ARTIFACT_STATES: ArtifactState[] = [
  'generating',
  'generated',
  'interrupted',
  'failed',
  'discarded',
  'base_changed',
  'saved',
  'partial',
  'pending_review',
  'confirmed',
  'approved',
  'rejected',
  'stale',
  'conflict',
];
