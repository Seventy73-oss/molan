import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { Icon } from '../../components/Icon';
import { Markdown } from '../../components/Markdown';
import { Pill, Spinner } from '../../components/Status';
import { metricsLine, type ArtifactView, type Message, type RunMetrics } from '../../lib/contracts';
import { isTerminal, statusText, toolLabel, type RunView } from '../../state/run';
import { TASK_LABELS, useWs } from '../../state/workspace';
import { ArtifactCard } from '../artifacts/ArtifactCard';
import { Composer } from './Composer';

export function AssistantPanel() {
  const { sessions, sessionId, selectSession, newSession, messages, artifacts, run, remoteRun, sessionLoading, reloadSession, stop } = useWs();
  const scroller = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  const byMessage = useMemo(() => {
    const m = new Map<string, ArtifactView[]>();
    for (const a of artifacts) {
      const k = a.messageId || '';
      m.set(k, [...(m.get(k) ?? []), a]);
    }
    return m;
  }, [artifacts]);
  const knownIds = new Set(messages.map((m) => m.id));
  const orphanArtifacts = artifacts.filter((a) => !a.messageId || !knownIds.has(a.messageId));
  const liveRun = run && run.sessionId === sessionId && (!isTerminal(run.status) || !run.messageId || !knownIds.has(run.messageId)) ? run : null;

  // 仅当作者停留在底部时跟随新内容滚动（自动滚动只影响视图，永远不触发写入）
  useLayoutEffect(() => {
    const el = scroller.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [messages.length, liveRun?.text.length, liveRun?.timeline.length, artifacts.length]);

  return (
    <div className="assistant">
      <div className="assistant__sessions">
        <select className="select select--sm grow" value={sessionId ?? ''} onChange={(e) => void selectSession(e.target.value)} aria-label="选择会话">
          {!sessionId ? <option value="">（新会话）</option> : null}
          {sessions.map((s) => (
            <option key={s.id} value={s.id}>
              {s.title || '未命名会话'}
            </option>
          ))}
        </select>
        <button className="icon-btn" onClick={() => void newSession()} aria-label="新建会话" title="新建会话">
          <Icon name="plus" />
        </button>
        <button className="icon-btn" onClick={() => void reloadSession()} aria-label="刷新会话" title="以服务端为准刷新">
          <Icon name="refresh" />
        </button>
      </div>

      {remoteRun && !liveRun && remoteRun.state === 'running' && remoteRun.live ? (
        <div className="notice notice--pending" role="status">
          <Icon name="spark" size={16} />
          <span className="grow">该会话有任务正在运行（可能在另一个窗口发起）。完成后刷新即可看到结果。</span>
          <button className="btn btn--sm" onClick={() => void stop()}>
            停止
          </button>
          <button className="btn btn--sm" onClick={() => void reloadSession()}>
            刷新
          </button>
        </div>
      ) : null}
      {remoteRun && !liveRun && remoteRun.state === 'interrupted' && /重启|ORPHANED/.test(`${remoteRun.error}${remoteRun.code}`) ? (
        <div className="notice" role="status">
          <Icon name="info" size={16} />
          <span className="grow">上一次运行未正常结束：{remoteRun.error || remoteRun.stateLabel}。已产生的产物仍在下方，可检查后重新发起。</span>
        </div>
      ) : null}

      <div
        className="assistant__stream"
        ref={scroller}
        onScroll={(e) => {
          const el = e.currentTarget;
          stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
        }}
        aria-live="polite"
      >
        {sessionLoading ? <Spinner label="读取会话" /> : null}
        {!sessionLoading && messages.length === 0 && !liveRun ? <AssistantEmpty /> : null}
        {messages.map((m) => (
          <MessageItem key={m.id} m={m} artifacts={byMessage.get(m.id) ?? []} />
        ))}
        {orphanArtifacts.filter((a) => !liveRun?.artifacts.some((x) => x.id === a.id)).map((a) => (
          <ArtifactCard key={a.id} a={a} />
        ))}
        {liveRun ? <RunCard run={liveRun} /> : null}
      </div>
      <Composer />
    </div>
  );
}

function AssistantEmpty() {
  const setComposer = useWs((s) => s.setComposer);
  const tips: { label: string; task: 'chat' | 'outline' | 'plot' | 'review' }[] = [
    { label: '聊聊这本书现在的状态', task: 'chat' },
    { label: '起草下一章细纲', task: 'outline' },
    { label: '推演接下来的剧情', task: 'plot' },
  ];
  return (
    <div className="empty">
      <Icon name="spark" size={28} />
      <p className="empty__title">告诉助手你想完成什么</p>
      <p className="small">选择任务类型 → 确认技能与上下文 → 发送。产物会以卡片形式交付，由你决定保存到哪里。</p>
      <div className="row row--wrap" style={{ justifyContent: 'center' }}>
        {tips.map((t) => (
          <button key={t.label} className="chip" onClick={() => setComposer({ task: t.task })}>
            {t.label}
          </button>
        ))}
      </div>
    </div>
  );
}

const MessageItem = memo(function MessageItem({ m, artifacts }: { m: Message; artifacts: ArtifactView[] }) {
  const [open, setOpen] = useState(false);
  if (m.role === 'user') {
    const task = (m.context?.task as string) || '';
    return (
      <div className="msg msg--user">
        {task && task !== 'chat' && task !== 'agent' ? <span className="tag">{TASK_LABELS[task as keyof typeof TASK_LABELS] ?? task}</span> : null}
        <div className="plain-text">{m.content}</div>
      </div>
    );
  }
  const result = (m.result ?? {}) as Record<string, unknown>;
  const steps = m.steps ?? [];
  const long = m.content.length > 1600;
  const hasArtifacts = artifacts.length > 0;
  // 最终文本已经落成产物（origin=model）时不重复整段显示，只给指引
  const textIsArtifact = artifacts.some((a) => a.origin === 'model');
  const status = result.status as string | undefined;
  return (
    <div className="msg msg--assistant">
      {steps.length ? (
        <details className="timeline">
          <summary className="small muted">执行了 {steps.length} 个步骤</summary>
          <ul>
            {steps.map((s, i) => (
              <li key={i} className={`timeline__item timeline__item--${s.status ?? 'info'}`}>
                <Icon name={s.status === 'error' ? 'alert' : 'check'} size={13} />
                <span className="grow">{toolLabel(s.name ?? s.type)}</span>
                {s.summary ? <span className="faint small ellipsis">{s.summary}</span> : null}
              </li>
            ))}
          </ul>
        </details>
      ) : null}
      {m.content ? (
        textIsArtifact || (hasArtifacts && long) ? (
          <p className="small muted">最终文本已形成下方产物卡片（{m.content.length} 字），由你决定保存去向。</p>
        ) : (
          <div className={long && !open ? 'msg__clamp' : undefined}>
            <Markdown text={m.content} />
          </div>
        )
      ) : null}
      {long && !hasArtifacts ? (
        <button className="btn btn--sm btn--ghost" onClick={() => setOpen((o) => !o)}>
          {open ? '收起' : '展开全文'}
        </button>
      ) : null}
      {metricsLine(result.metrics as RunMetrics | undefined) ? <p className="small faint">{metricsLine(result.metrics as RunMetrics)}</p> : null}
      {m.interrupted || (status && status !== 'done') ? (
        <Pill tone={status === 'error' ? 'bad' : 'pending'} icon="alert">
          {status === 'interrupted' || m.interrupted ? '已中断' : status === 'budget_exhausted' ? '预算耗尽' : status === 'tools_unsupported' ? '模型不支持工具' : '未完成'}
          {typeof result.error === 'string' && result.error ? `：${result.error}` : ''}
        </Pill>
      ) : null}
      {artifacts.map((a) => (
        <ArtifactCard key={a.id} a={a} />
      ))}
    </div>
  );
});

function RunCard({ run }: { run: RunView }) {
  const setComposer = useWs((s) => s.setComposer);
  const running = !isTerminal(run.status);
  return (
    <div className="msg msg--assistant run">
      <div className="row row--wrap">
        <Pill tone={running ? 'info' : run.status === 'done' ? 'ok' : 'pending'} icon={running ? 'refresh' : run.status === 'done' ? 'check' : 'alert'}>
          {run.taskLabel} · {statusText(run)}
        </Pill>
        {run.model ? <span className="small faint">{run.model}</span> : null}
        {run.plan ? (
          <span className="small faint ellipsis">
            技能：{run.plan.skills.map((s) => s.name).join('、') || '无'}
          </span>
        ) : null}
      </div>
      {run.timeline.length ? (
        <ul className="timeline timeline--live">
          {run.timeline.map((t) => (
            <li key={t.key} className={`timeline__item timeline__item--${t.status ?? 'info'}`}>
              <Icon name={t.status === 'running' ? 'refresh' : t.status === 'error' ? 'alert' : t.kind === 'retry' ? 'refresh' : 'check'} size={13} className={t.status === 'running' ? 'spin' : undefined} />
              <span className="grow">{t.title}</span>
              {t.detail ? <span className="faint small ellipsis" title={t.detail}>{t.detail}</span> : null}
            </li>
          ))}
        </ul>
      ) : null}
      {run.text ? <div className="plain-text run__text">{run.text}</div> : running ? <Spinner label={run.status === 'starting' ? '冻结技能与上下文…' : '等待模型输出…'} /> : null}
      {run.error ? (
        <div className={`notice ${run.status === 'interrupted' ? 'notice--pending' : 'notice--bad'}`} role="alert">
          <Icon name="alert" size={16} />
          <span className="grow break">{run.error.message}</span>
          {run.status === 'tools_unsupported' ? (
            <button className="btn btn--sm" onClick={() => setComposer({ mode: 'direct' })}>
              改用直接生成
            </button>
          ) : null}
        </div>
      ) : null}
      {run.metrics && !running ? <p className="small faint">{metricsLine(run.metrics)}</p> : null}
      {run.artifacts.map((a) => (
        <ArtifactCard key={a.id} a={a} />
      ))}
    </div>
  );
}

/** 刷新后若有本地记录的运行但服务端仍在跑，轮询直至终态再刷新。 */
export function useRemoteRunPoll() {
  const remoteRun = useWs((s) => s.remoteRun);
  useEffect(() => {
    if (!remoteRun?.live) return;
    const t = setInterval(() => void useWs.getState().reloadSession(), 4000);
    return () => clearInterval(t);
  }, [remoteRun?.live]);
}
