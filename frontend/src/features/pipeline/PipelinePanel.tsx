import { useCallback, useEffect, useState } from 'react';
import { ConfirmDialog } from '../../components/Dialog';
import { Icon } from '../../components/Icon';
import { Pill, Spinner } from '../../components/Status';
import { toast } from '../../components/Toasts';
import { api, type AutoStatus, type PipelineChapter, type PipelineState } from '../../lib/api';
import type { ArtifactView } from '../../lib/contracts';
import { errorText, newRequestId } from '../../lib/ipc';
import { useWs } from '../../state/workspace';

const BODY: Record<string, { tone: 'ok' | 'pending' | 'neutral' | 'bad'; text: string }> = {
  approved: { tone: 'ok', text: '已定稿' },
  pending: { tone: 'pending', text: '待审' },
  missing: { tone: 'neutral', text: '未写' },
  none: { tone: 'neutral', text: '未写' },
};
const OUTLINE: Record<string, { tone: 'ok' | 'pending' | 'neutral' | 'bad'; text: string }> = {
  confirmed: { tone: 'ok', text: '已确认' },
  saved: { tone: 'pending', text: '待确认' },
  stale: { tone: 'bad', text: '确认失效' },
  none: { tone: 'neutral', text: '无' },
};
const MEMORY: Record<string, { tone: 'ok' | 'pending' | 'neutral' | 'bad'; text: string }> = {
  valid: { tone: 'ok', text: '已同步' },
  pending: { tone: 'pending', text: '排队中' },
  failed: { tone: 'bad', text: '失败' },
  stale: { tone: 'pending', text: '需重建' },
  missing: { tone: 'neutral', text: '—' },
  none: { tone: 'neutral', text: '—' },
};

const STAGE: Record<string, string> = {
  setup: '先建立设定资料',
  outline: '先写全书大纲',
  chapter_outline: '起草本章细纲',
  outline_confirm: '确认本章细纲',
  chapter_body: '起草本章正文',
  review: '处理待审章节',
  memory_fix: '修复章节记忆',
};

export function PipelinePanel() {
  const { bookId, sessionId, setComposer, setPanel, openDoc, refreshTree, refreshPendingCount, upsertArtifact } = useWs();
  const [state, setState] = useState<PipelineState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [drafting, setDrafting] = useState<{ ch: number; step: string; chars: number; requestId: string } | null>(null);

  const load = useCallback(async () => {
    if (!bookId) return;
    try {
      setState(await api.pipeline(bookId));
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, [bookId]);

  useEffect(() => {
    void load();
  }, [load]);

  if (!bookId) return null;

  const confirmOutline = async (ch: number) => {
    try {
      const r = await api.confirmOutline(bookId, ch);
      toast.ok(`第${ch}章细纲已确认（版本 ${r.hash.slice(0, 8)}）`);
      await load();
    } catch (e) {
      toast.bad(errorText(e));
    }
  };

  const draft = async (ch: number) => {
    const requestId = newRequestId('draft');
    setDrafting({ ch, step: '前置检查', chars: 0, requestId });
    try {
      await api.draftChapter(bookId, ch, requestId, useWs.getState().sessionId, useWs.getState().composer.selection, '', (e) => {
        if (e.type === 'step') setDrafting((d) => (d ? { ...d, step: String(e.title ?? '') } : d));
        if (e.type === 'progress' && Number(e.chars) >= 0) setDrafting((d) => (d ? { ...d, chars: Number(e.chars) } : d));
        if (e.type === 'artifact' && e.artifact) upsertArtifact(e.artifact as ArtifactView);
      });
      toast.ok(`第${ch}章草稿已进入「正文待审」（尚未定稿）`);
      void refreshPendingCount();
      void refreshTree();
    } catch (e) {
      toast.bad(`第${ch}章起草未完成：${errorText(e)}`);
    } finally {
      setDrafting(null);
      await load();
    }
  };

  const nextAction = (n: NonNullable<PipelineState['next']>) => {
    const ch = n.chapter ?? 0;
    switch (n.stage) {
      case 'chapter_outline':
        setComposer({ task: 'outline', target: { ch, label: `第${ch}章` } });
        setPanel('assistant');
        break;
      case 'outline_confirm':
        void confirmOutline(ch);
        break;
      case 'chapter_body':
        void draft(ch);
        break;
      case 'review':
        setPanel('pending');
        break;
      case 'memory_fix':
        void api.rebuildMemory(bookId, ch || undefined).then((r) => toast.info(r.note ?? '已提交重建'), (e) => toast.bad(errorText(e)));
        break;
      default:
        setComposer({ task: 'chat', target: null });
        setPanel('assistant');
    }
  };

  void sessionId;
  return (
    <div className="panel-body pipeline">
      <div className="row row--between" style={{ padding: '10px 14px 4px' }}>
        <span className="section-title">生产线</span>
        <button className="icon-btn icon-btn--sm" onClick={() => void load()} aria-label="刷新生产线">
          <Icon name="refresh" size={15} />
        </button>
      </div>
      {error ? <p className="notice notice--bad" style={{ margin: 12 }}>{error}</p> : null}
      {!state ? (
        <Spinner label="读取生产线" />
      ) : (
        <>
          <div className="pipe-summary">
            <div>
              <strong>{state.counts.approved}</strong>
              <span className="small faint">已定稿</span>
            </div>
            <div>
              <strong>{state.counts.pending}</strong>
              <span className="small faint">待审</span>
            </div>
            <div>
              <strong>{state.counts.total}</strong>
              <span className="small faint">章节</span>
            </div>
          </div>
          {state.next ? (
            <div className={`pipe-next${state.next.blocked ? ' is-blocked' : ''}`}>
              <div className="stack stack--tight grow">
                <span className="small faint">下一步</span>
                <strong>
                  {STAGE[state.next.stage] ?? state.next.stage}
                  {state.next.chapter ? ` · 第${state.next.chapter}章` : ''}
                </strong>
                {state.nextContext?.length ? <span className="small faint ellipsis">将携带：{state.nextContext.map((c) => c.label).join('、')}</span> : null}
              </div>
              <button className="btn btn--primary btn--sm" onClick={() => nextAction(state.next!)} disabled={!!drafting}>
                去处理
              </button>
            </div>
          ) : null}
          {state.blockers.length ? (
            <ul className="pipe-blockers">
              {state.blockers.map((b, i) => (
                <li key={i} className="small">
                  <Icon name="alert" size={13} /> {b.type === 'memory' ? `第${b.chapter}章记忆${MEMORY[b.status ?? '']?.text ?? b.status}` : b.type === 'outline' ? `第${b.chapter}章细纲确认已失效` : `${b.type}${b.chapter ? ` · 第${b.chapter}章` : ''}`}
                </li>
              ))}
            </ul>
          ) : null}
          {drafting ? (
            <div className="notice notice--pending" role="status">
              <Icon name="refresh" size={16} className="spin" />
              <span className="grow">
                第{drafting.ch}章：{drafting.step}
                {drafting.chars ? ` · ${drafting.chars} 字` : ''}
              </span>
              <button className="btn btn--sm" onClick={() => void api.abort(drafting.requestId)}>
                停止
              </button>
            </div>
          ) : null}
          <ul className="pipe-list" aria-label="章节状态">
            {state.chapters.map((c) => (
              <ChapterRow key={c.n} c={c} busy={!!drafting} onConfirm={() => void confirmOutline(c.n)} onDraft={() => void draft(c.n)} onOpen={(g, n) => void openDoc(g, n)} onOutline={() => {
                setComposer({ task: 'outline', target: { ch: c.n, label: `第${c.n}章` } });
                setPanel('assistant');
              }} />
            ))}
            <li className="pipe-row pipe-row--add">
              <button className="btn btn--sm btn--ghost" onClick={() => {
                const n = (state.chapters.at(-1)?.n ?? 0) + 1;
                setComposer({ task: 'outline', target: { ch: n, label: `第${n}章` } });
                setPanel('assistant');
              }}>
                <Icon name="plus" size={14} />
                起草下一章细纲
              </button>
            </li>
          </ul>
          <AutoWrite onChanged={() => void load()} />
        </>
      )}
    </div>
  );
}

function ChapterRow({ c, busy, onConfirm, onDraft, onOpen, onOutline }: { c: PipelineChapter; busy: boolean; onConfirm: () => void; onDraft: () => void; onOpen: (g: string, n: string) => void; onOutline: () => void }) {
  const o = OUTLINE[c.outlineStatus ?? (c.outline === 'present' ? 'saved' : 'none')] ?? OUTLINE.none;
  const b = BODY[c.body] ?? BODY.none;
  const m = MEMORY[c.memory] ?? MEMORY.none;
  return (
    <li className="pipe-row">
      <strong className="pipe-row__ch">第{c.n}章</strong>
      <div className="pipe-row__chips">
        <Pill tone={o.tone}>细纲 {o.text}</Pill>
        <Pill tone={b.tone}>正文 {b.text}</Pill>
        <Pill tone={m.tone}>记忆 {m.text}</Pill>
      </div>
      <div className="pipe-row__actions">
        {c.outlineStatus === 'saved' ? (
          <button className="btn btn--sm" onClick={onConfirm} disabled={busy}>
            确认细纲
          </button>
        ) : null}
        {c.outlineStatus === 'none' || !c.outlineStatus ? (
          <button className="btn btn--sm btn--ghost" onClick={onOutline} disabled={busy}>
            写细纲
          </button>
        ) : null}
        {c.outlineStatus === 'confirmed' && (c.body === 'missing' || c.body === 'none') ? (
          <button className="btn btn--sm" onClick={onDraft} disabled={busy}>
            起草正文
          </button>
        ) : null}
        {c.body === 'approved' ? (
          <button className="btn btn--sm btn--ghost" onClick={() => onOpen('正文', `第${c.n}章.md`)}>
            打开
          </button>
        ) : null}
      </div>
    </li>
  );
}

function AutoWrite({ onChanged }: { onChanged: () => void }) {
  const { bookId, sessionId, composer, newSession } = useWs();
  const [status, setStatus] = useState<AutoStatus | null>(null);
  const [from, setFrom] = useState(1);
  const [to, setTo] = useState(1);
  const [full, setFull] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [open, setOpen] = useState(false);

  const poll = useCallback(async () => {
    if (!bookId) return;
    try {
      setStatus(await api.autoStatus(bookId));
    } catch {
      /* 状态查询失败不影响写作 */
    }
  }, [bookId]);

  useEffect(() => {
    void poll();
  }, [poll]);
  useEffect(() => {
    if (!status?.running) return;
    const t = setInterval(() => {
      void poll();
      onChanged();
    }, 3000);
    return () => clearInterval(t);
  }, [status?.running, poll, onChanged]);

  if (!bookId) return null;
  const running = !!status?.running && status.bookId === bookId;
  const start = async () => {
    setConfirm(false);
    let sid = sessionId;
    if (!sid) {
      await newSession('自动写作');
      sid = useWs.getState().sessionId;
    }
    try {
      await api.autoStart({ bookId, sessionId: sid!, fromCh: from, toCh: full ? to : from, fullAuto: full, confirmAuto: full, skillSelection: composer.selection });
      toast.ok(full ? `全自动写作已开始：第${from}~${to}章` : `已开始写第${from}章（进入待审）`);
      await poll();
    } catch (e) {
      toast.bad(errorText(e));
    }
  };
  return (
    <section className="autowrite">
      <button className="autowrite__head" onClick={() => setOpen((o) => !o)} aria-expanded={open}>
        <Icon name={open ? 'chevronDown' : 'chevronRight'} size={14} />
        <span className="grow">批量自动写作</span>
        {running ? <Pill tone="info" icon="refresh">运行中 · 第{status?.curCh ?? status?.current ?? '?'}章</Pill> : status?.resumable ? <Pill tone="pending">可续跑</Pill> : null}
      </button>
      {open ? (
        <div className="stack" style={{ padding: '0 14px 14px' }}>
          <p className="small muted">复用单章写作服务：已有正文或待审稿的章节会跳过，不越界写后续章节。技能计划在开始时冻结，运行中改技能不影响本次任务。</p>
          <div className="row row--wrap">
            <label className="field">
              <span className="label">从第</span>
              <input className="input input--num" type="number" min={1} value={from} onChange={(e) => setFrom(Math.max(1, Number(e.target.value)))} />
            </label>
            {full ? (
              <label className="field">
                <span className="label">到第</span>
                <input className="input input--num" type="number" min={from} value={to} onChange={(e) => setTo(Math.max(from, Number(e.target.value)))} />
              </label>
            ) : null}
          </div>
          <label className="checkbox">
            <input type="checkbox" checked={full} onChange={(e) => setFull(e.target.checked)} />
            全自动（审核通过直接写入正文；未开启时只写一章并进入待审）
          </label>
          <div className="row row--wrap">
            {running ? (
              <button className="btn btn--danger" onClick={async () => { const r = await api.autoStop(bookId); toast.info(r.reason ?? '已请求停止'); await poll(); }}>
                停止
              </button>
            ) : (
              <>
                <button className="btn btn--primary" onClick={() => setConfirm(true)}>
                  开始
                </button>
                {status?.resumable ? (
                  <button className="btn" onClick={async () => { try { await api.autoResume(bookId, false); await poll(); } catch (e) { toast.bad(errorText(e)); } }}>
                    续跑
                  </button>
                ) : null}
              </>
            )}
          </div>
          {status?.error ? <p className="notice notice--bad small">{status.error}</p> : null}
          {status?.logs?.length ? (
            <ol className="autowrite__logs">
              {status.logs.slice(-12).map((l, i) => (
                <li key={i} className="small">
                  <span className="tag">{l.step}</span> {l.text}
                </li>
              ))}
            </ol>
          ) : null}
        </div>
      ) : null}
      {confirm ? (
        <ConfirmDialog
          title={full ? '开始全自动写作？' : '开始写这一章？'}
          body={full ? `将依次生成第${from}~${to}章；审核通过的章节会直接写入「正文」，未通过的进入待审。可随时停止。` : `将生成第${from}章正文并放入「正文待审」，需要你审阅后定稿。`}
          confirmLabel="开始"
          onCancel={() => setConfirm(false)}
          onConfirm={() => void start()}
        />
      ) : null}
    </section>
  );
}
