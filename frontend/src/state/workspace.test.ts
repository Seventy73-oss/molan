/**
 * 作品作用域回归：发送时尚未有会话 → 先创建会话；若创建期间作者切到另一本书，
 * 旧作品的会话、乐观消息与运行状态都不得进入新作品的视图。
 */
import { describe, expect, it, vi } from 'vitest';

const pendingSession: { resolve: (v: { id: string; title: string }) => void } = { resolve: () => undefined };

vi.mock('../lib/api', () => ({
  api: {
    tree: vi.fn(async () => []),
    sessions: vi.fn(async () => []),
    listBooks: vi.fn(async () => []),
    createSession: vi.fn(
      () =>
        new Promise((r) => {
          pendingSession.resolve = r;
        }),
    ),
    messages: vi.fn(async () => []),
    artifacts: vi.fn(async () => []),
    runStatus: vi.fn(async () => null),
    agentTurn: vi.fn(async () => ({ status: 'done' })),
    pending: vi.fn(async () => []),
    proposals: vi.fn(async () => []),
  },
}));

import { api } from '../lib/api';
import { useWs } from './workspace';

describe('作品作用域', () => {
  it('切书期间创建的旧作品会话与消息不进入新作品', async () => {
    await useWs.getState().openBook('A', [{ id: 'A', title: '甲' } as never]);
    const sending = useWs.getState().send('旧作品的消息');
    await useWs.getState().openBook('B', [{ id: 'B', title: '乙' } as never]);
    pendingSession.resolve({ id: 'S-A', title: '旧会话' });
    await sending;
    const s = useWs.getState();
    expect(s.bookId).toBe('B');
    expect(s.sessions.map((x) => x.id)).not.toContain('S-A');
    expect(s.sessionId).toBeNull();
    expect(s.messages.some((m) => m.content === '旧作品的消息')).toBe(false);
    expect(s.run).toBeNull();
    expect(api.agentTurn).not.toHaveBeenCalled();
  });
});
