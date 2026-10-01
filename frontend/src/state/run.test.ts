import { describe, expect, it } from 'vitest';
import { isTerminal, reduceRun, startRun, type RunView } from './run';

const run0 = (): RunView => startRun({ requestId: 'r1', sessionId: 's1', task: 'outline', taskLabel: '细纲', now: 0 });

function fold(events: Record<string, unknown>[]): RunView {
  return events.reduce<RunView>((r, e) => reduceRun(r, e), run0());
}

describe('reduceRun', () => {
  it('folds a typical tool round then text then done', () => {
    const r = fold([
      { type: 'meta', task: 'outline', runId: 'run-1', model: 'm', mode: 'agent', planHash: 'h' },
      { type: 'plan', plan: { skills: [], excluded: [] } },
      { type: 'context', manifestId: 'mid', blocks: [{ label: '作品概况', source: 'book', chars: 10 }] },
      { type: 'tool', callId: 'c1', name: 'scan_book_tree', status: 'running' },
      { type: 'tool', callId: 'c1', name: 'scan_book_tree', status: 'ok', summary: '[...]' },
      { type: 'step', index: 1, title: '完成第 1 轮工具调用' },
      { type: 'delta', text: '# 第2章' },
      { type: 'delta', text: '细纲' },
      { type: 'artifact', artifact: { id: 'a1', state: 'generated' } },
      { type: 'done', status: 'done', full: '# 第2章细纲', messageId: 'm1', usedTokens: 12 },
    ]);
    expect(r.status).toBe('done');
    expect(r.runId).toBe('run-1');
    expect(r.text).toBe('# 第2章细纲');
    expect(r.timeline.filter((t) => t.kind === 'tool')).toHaveLength(1);
    expect(r.timeline[0]).toMatchObject({ status: 'ok', title: '查看资料目录' });
    expect(r.artifacts.map((a) => a.id)).toEqual(['a1']);
    expect(r.contextBlocks).toHaveLength(1);
    expect(isTerminal(r.status)).toBe(true);
  });

  it('pairs reused call ids per round and keeps tool errors', () => {
    const r = fold([
      { type: 'tool', callId: 'mock-call-1', name: 'read_book_file', status: 'running' },
      { type: 'tool', callId: 'mock-call-1', name: 'read_book_file', status: 'error', summary: '缺少参数' },
      { type: 'tool', callId: 'mock-call-1', name: 'scan_book_tree', status: 'running' },
      { type: 'tool', callId: 'mock-call-1', name: 'scan_book_tree', status: 'ok' },
    ]);
    const tools = r.timeline.filter((t) => t.kind === 'tool');
    expect(tools).toHaveLength(2);
    expect(tools[0]).toMatchObject({ status: 'error', detail: '缺少参数' });
    expect(tools[1]).toMatchObject({ status: 'ok', name: 'scan_book_tree' });
  });

  it('retry is informational, tools_unsupported and budget are terminal states', () => {
    let r = fold([{ type: 'error', code: 'RETRY', message: '重试 1/2' }]);
    expect(r.status).toBe('starting');
    expect(r.timeline[0].kind).toBe('retry');
    r = fold([
      { type: 'error', code: 'TOOLS_UNSUPPORTED', message: '不支持' },
      { type: 'done', status: 'tools_unsupported' },
    ]);
    expect(r.status).toBe('tools_unsupported');
    expect(r.error?.code).toBe('TOOLS_UNSUPPORTED');
    r = fold([{ type: 'error', code: 'BUDGET_EXHAUSTED', message: '耗尽' }, { type: 'done', status: 'budget_exhausted' }]);
    expect(r.status).toBe('budget_exhausted');
  });

  it('heartbeat progress does not change text, nested draft meta becomes a step', () => {
    const r = fold([
      { type: 'meta', task: 'body', runId: 'run-9' },
      { type: 'progress', chars: -1 },
      { type: 'meta', task: 'draft_chapter', ch: 3, model: 'x' },
      { type: 'progress', chars: 1200 },
    ]);
    expect(r.runId).toBe('run-9');
    expect(r.progressChars).toBe(1200);
    expect(r.timeline[0].title).toContain('第3章');
    expect(r.text).toBe('');
  });

  it('interrupted keeps partial text and reason', () => {
    const r = fold([
      { type: 'delta', text: '残片' },
      { type: 'interrupted', reason: '作者已停止', partialChars: 2 },
      { type: 'done', status: 'interrupted', full: '残片' },
    ]);
    expect(r.status).toBe('interrupted');
    expect(r.text).toBe('残片');
    expect(r.error?.message).toBe('作者已停止');
  });
});

describe('运行指标', () => {
  it('done 事件携带的指标进入运行视图，并能生成一行摘要', async () => {
    const { metricsLine } = await import('../lib/contracts');
    const metrics = {
      totalMs: 2400, firstTokenMs: 820, toolMs: 12, cacheHits: 1, retries: 1, usageEstimated: true,
      modelCalls: [{ firstTokenMs: 800, firstTextMs: 900, totalMs: 1200, promptChars: 500, outputChars: 20, outcome: 'toolCalls', usage: null, usageSource: 'estimated' }, { firstTokenMs: 10, firstTextMs: 10, totalMs: 300, promptChars: 900, outputChars: 300, outcome: 'ok', usage: null, usageSource: 'estimated' }],
      tools: [{ name: 'read_book_file', ms: 5, cached: false, parallel: true, ok: true }, { name: 'read_book_file', ms: 0, cached: true, parallel: false, ok: true }],
    };
    const r = fold([{ type: 'done', status: 'done', full: 'x', metrics }]);
    expect(r.metrics?.cacheHits).toBe(1);
    expect(metricsLine(r.metrics)).toBe('首字 820ms · 模型 2 次 · 工具 2 次（缓存命中 1） · 重试 1 次 · 总耗时 2.4s · 用量含估算');
    expect(metricsLine(undefined)).toBe('');
    expect(metricsLine({} as never)).toBe(''); // 旧消息没有指标时不显示
  });
});
