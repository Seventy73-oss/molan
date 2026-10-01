/**
 * Agent 运行事件 reducer（纯函数）：把 agent_turn 的 NDJSON 事件折叠成一次运行的视图。
 * 只描述「运行进度」；产物是否已保存由产物卡片的服务端投影决定，不由运行状态推断。
 */
import type { ArtifactView, ContextBlock, RunMetrics, SkillPlanView } from '../lib/contracts';
import type { IpcEvent } from '../lib/ipc';

export type RunStatus =
  | 'starting'
  | 'running'
  | 'done'
  | 'interrupted'
  | 'error'
  | 'budget_exhausted'
  | 'tools_unsupported'
  | 'session_busy'
  | 'already_running';

export interface TimelineEntry {
  key: string;
  kind: 'tool' | 'step' | 'notice' | 'retry' | 'error' | 'review';
  title: string;
  detail?: string;
  status?: 'running' | 'ok' | 'error' | 'info';
  name?: string;
}

export interface RunView {
  requestId: string;
  sessionId: string;
  task: string;
  taskLabel: string;
  status: RunStatus;
  runId?: string;
  model?: string;
  mode?: string;
  planHash?: string;
  plan?: SkillPlanView;
  contextBlocks?: ContextBlock[];
  manifestId?: string;
  text: string;
  reasoningChars: number;
  timeline: TimelineEntry[];
  artifacts: ArtifactView[];
  error?: { code?: string; message: string };
  messageId?: string;
  usedTokens?: number;
  metrics?: RunMetrics;
  progressChars?: number;
  startedAt: number;
}

export function startRun(p: { requestId: string; sessionId: string; task: string; taskLabel: string; now?: number }): RunView {
  return {
    requestId: p.requestId,
    sessionId: p.sessionId,
    task: p.task,
    taskLabel: p.taskLabel,
    status: 'starting',
    text: '',
    reasoningChars: 0,
    timeline: [],
    artifacts: [],
    startedAt: p.now ?? Date.now(),
  };
}

const TOOL_LABELS: Record<string, string> = {
  scan_book_tree: '查看资料目录',
  read_book_file: '读取文件',
  get_pipeline_state: '查看生产线状态',
  list_pending_chapters: '查看待审队列',
  get_chapter_context: '读取已定稿记忆',
  list_skills: '查看技能库',
  get_effective_skills: '查看生效技能',
  create_change_proposal: '创建修改提案',
  draft_chapter_outline: '保存细纲草稿',
  draft_chapter_body: '起草正文（章节服务）',
  confirm_chapter_outline: '确认细纲',
  finalize_chapter_draft: '定稿',
};

export function toolLabel(name: string): string {
  return TOOL_LABELS[name] ?? name;
}

const str = (v: unknown) => (typeof v === 'string' ? v : v == null ? '' : String(v));

function upsertTimeline(list: TimelineEntry[], entry: TimelineEntry): TimelineEntry[] {
  const i = list.findIndex((x) => x.key === entry.key);
  if (i < 0) return [...list, entry];
  const next = list.slice();
  next[i] = { ...next[i], ...entry };
  return next;
}

function upsertArtifact(list: ArtifactView[], a: ArtifactView): ArtifactView[] {
  const i = list.findIndex((x) => x.id === a.id);
  if (i < 0) return [...list, a];
  const next = list.slice();
  next[i] = a;
  return next;
}

const TERMINAL: RunStatus[] = ['done', 'interrupted', 'error', 'budget_exhausted', 'tools_unsupported', 'session_busy', 'already_running'];

export function isTerminal(s: RunStatus): boolean {
  return TERMINAL.includes(s);
}

export function reduceRun(run: RunView, e: IpcEvent): RunView {
  const type = str(e.type);
  switch (type) {
    case 'meta': {
      // 章节服务的嵌套 meta（task=draft_chapter）只作为子步骤，不覆盖本次运行信息
      if (e.task === 'draft_chapter') {
        return { ...run, timeline: upsertTimeline(run.timeline, { key: 'draft-meta', kind: 'step', title: `章节服务：第${str(e.ch)}章正文生成中`, detail: str(e.model), status: 'running' }) };
      }
      return {
        ...run,
        status: run.status === 'starting' ? 'running' : run.status,
        runId: str(e.runId) || run.runId,
        model: str(e.model) || run.model,
        mode: str(e.mode) || run.mode,
        planHash: str(e.planHash) || run.planHash,
        taskLabel: str(e.taskLabel) || run.taskLabel,
      };
    }
    case 'plan':
      return { ...run, plan: e.plan as SkillPlanView };
    case 'context':
      return { ...run, contextBlocks: (e.blocks as ContextBlock[]) ?? [], manifestId: str(e.manifestId) };
    case 'delta':
      return { ...run, status: 'running', text: run.text + str(e.text) };
    case 'reasoning':
      return { ...run, reasoningChars: run.reasoningChars + str(e.text).length };
    case 'progress': {
      const chars = Number(e.chars);
      if (!Number.isFinite(chars) || chars < 0) return run; // 心跳
      return { ...run, progressChars: chars };
    }
    case 'tool': {
      const name = str(e.name);
      const callId = str(e.callId) || name;
      const status = str(e.status);
      const detail = str(e.summary) || undefined;
      if (status === 'running') {
        // 同一 callId 可能跨轮复用：按已完成次数区分，running 总是开新条目
        const n = run.timeline.filter((t) => t.kind === 'tool' && t.key.startsWith(`tool:${callId}:`)).length;
        return {
          ...run,
          timeline: [...run.timeline, { key: `tool:${callId}:${n}`, kind: 'tool', name, title: toolLabel(name), status: 'running' }],
        };
      }
      const st: TimelineEntry['status'] = status === 'ok' || status === 'error' ? status : 'info';
      const idx = [...run.timeline].reverse().findIndex((t) => t.kind === 'tool' && t.key.startsWith(`tool:${callId}:`) && t.status === 'running');
      if (idx < 0) {
        return { ...run, timeline: [...run.timeline, { key: `tool:${callId}:x${run.timeline.length}`, kind: 'tool', name, title: toolLabel(name), status: st, detail }] };
      }
      const real = run.timeline.length - 1 - idx;
      const timeline = run.timeline.slice();
      timeline[real] = { ...timeline[real], status: st, detail };
      return { ...run, timeline };
    }
    case 'step':
      return {
        ...run,
        timeline: upsertTimeline(run.timeline, { key: `step:${str(e.index)}:${str(e.title)}`, kind: 'step', title: str(e.title), status: 'info' }),
      };
    case 'review':
      return {
        ...run,
        timeline: upsertTimeline(run.timeline, {
          key: 'review',
          kind: 'review',
          title: e.ok ? '剧情/设定审核通过' : '审核发现问题，已转人工复核',
          detail: Array.isArray(e.issues) ? (e.issues as unknown[]).map(str).join('；') : undefined,
          status: e.ok ? 'ok' : 'error',
        }),
      };
    case 'notice':
      return {
        ...run,
        timeline: upsertTimeline(run.timeline, { key: `notice:${str(e.code)}`, kind: 'notice', title: str(e.message), status: 'info' }),
      };
    case 'artifact':
      return { ...run, artifacts: upsertArtifact(run.artifacts, e.artifact as ArtifactView) };
    case 'error': {
      const code = str(e.code);
      if (code === 'RETRY') {
        return {
          ...run,
          timeline: upsertTimeline(run.timeline, { key: `retry:${run.timeline.length}`, kind: 'retry', title: str(e.message), status: 'info' }),
        };
      }
      const status: RunStatus =
        code === 'TOOLS_UNSUPPORTED'
          ? 'tools_unsupported'
          : code === 'BUDGET_EXHAUSTED'
            ? 'budget_exhausted'
            : code === 'SESSION_BUSY'
              ? 'session_busy'
              : code === 'CANCELLED'
                ? 'interrupted'
                : 'error';
      return { ...run, status, error: { code: code || undefined, message: str(e.message) } };
    }
    case 'interrupted':
      return { ...run, status: 'interrupted', error: run.error ?? { message: str(e.reason) || '已中断' } };
    case 'done': {
      const s = str(e.status) as RunStatus;
      const status: RunStatus = s && (TERMINAL as string[]).includes(s) ? s : run.error ? run.status : 'done';
      return {
        ...run,
        status: run.status === 'tools_unsupported' || run.status === 'budget_exhausted' ? run.status : status,
        // done.full 是最终文本（与落库一致）；存在时以它为准
        text: typeof e.full === 'string' && e.full ? e.full : run.text,
        messageId: str(e.messageId) || run.messageId,
        runId: str(e.runId) || run.runId,
        usedTokens: typeof e.usedTokens === 'number' ? e.usedTokens : run.usedTokens,
        metrics: e.metrics && typeof e.metrics === 'object' ? (e.metrics as RunMetrics) : run.metrics,
      };
    }
    default:
      return run;
  }
}

export function statusText(run: RunView): string {
  switch (run.status) {
    case 'starting':
      return '准备中';
    case 'running':
      return run.progressChars ? `生成中 · ${run.progressChars} 字` : '运行中';
    case 'done':
      return '已完成';
    case 'interrupted':
      return '已中断';
    case 'error':
      return '失败';
    case 'budget_exhausted':
      return '预算耗尽';
    case 'tools_unsupported':
      return '模型不支持工具';
    case 'session_busy':
      return '会话忙';
    case 'already_running':
      return '该请求已在运行';
  }
}
