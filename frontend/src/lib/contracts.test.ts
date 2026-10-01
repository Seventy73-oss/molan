// @vitest-environment node
// 契约测试：Rust 端 v2_tests 生成的规范化 fixture 必须满足前端类型的运行时校验。
// Rust 端改了返回形状 → fixture 变化 → 这里失败，提醒同步前端类型。
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
import {
  ARTIFACT_STATES,
  CONTRACT_VERSION,
  checkArtifactView,
  checkDocRead,
  checkRunState,
  checkTaskPreview,
  checkWriteReceipt,
  type AppInfo,
} from './contracts';

// vitest 在 frontend/ 下运行：契约 fixture 位于仓库根 contracts/fixtures
const dir = resolve(process.cwd(), '../contracts/fixtures');
const load = (name: string) => JSON.parse(readFileSync(resolve(dir, `${name}.json`), 'utf8'));

describe('contracts v2 fixtures', () => {
  it('app_info', () => {
    const v = load('app_info') as AppInfo;
    expect(v.contract).toBe(CONTRACT_VERSION);
    expect(v.tasks.map((t) => t.id)).toEqual([
      'chat', 'plot', 'outline', 'body', 'revise', 'review', 'humanize', 'summary', 'distill',
    ]);
    expect(v.writeOps).toEqual(['create', 'replace', 'append', 'insert', 'replace_range']);
  });

  it('write receipts', () => {
    expect(checkWriteReceipt(load('write_receipt_committed'))).toBeNull();
    const c = load('write_receipt_conflict');
    expect(checkWriteReceipt(c)).toBeNull();
    expect(c.commit).toBe('conflict');
    expect(c.error.code).toBe('BASE_CHANGED');
  });

  it('doc_read', () => {
    expect(checkDocRead(load('doc_read'))).toBeNull();
  });

  it('artifact views', () => {
    const g = load('artifact_generated');
    expect(checkArtifactView(g)).toBeNull();
    expect(ARTIFACT_STATES).toContain(g.state);
    expect(g.actions[0].id).toBe('save_outline');
    const d = load('artifact_deliver_confirmed');
    expect(checkArtifactView(d.artifact)).toBeNull();
    expect(d.artifact.state).toBe('confirmed');
    expect(d.delivery.ok).toBe(true);
  });

  it('task_preview', () => {
    const t = load('task_preview');
    expect(checkTaskPreview(t)).toBeNull();
    expect(t.plan.excluded[0].code).toBe('REPLACED');
  });

  it('run_state', () => {
    const r = load('run_state');
    expect(checkRunState(r)).toBeNull();
    expect(r.state).toBe('completed');
  });
});
