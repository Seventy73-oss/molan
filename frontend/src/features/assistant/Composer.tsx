import { useEffect, useRef, useState } from 'react';
import { Icon } from '../../components/Icon';
import { api } from '../../lib/api';
import type { TaskId, TaskPreview } from '../../lib/contracts';
import { errorText } from '../../lib/ipc';
import { isTerminal } from '../../state/run';
import { TASK_LABELS, useWs } from '../../state/workspace';
import { PlanPreview } from './PlanPreview';
import { SkillPicker } from './SkillPicker';

const TASKS: { id: TaskId; hint: string }[] = [
  { id: 'chat', hint: '交流想法，不写入任何文件' },
  { id: 'plot', hint: '推演后续剧情走向（产物可另存）' },
  { id: 'outline', hint: '起草章节细纲（保存后需确认）' },
  { id: 'body', hint: '按已确认细纲起草正文，进入待审' },
  { id: 'revise', hint: '改写选区或整篇（产出修改稿，由你决定写回）' },
  { id: 'review', hint: '审稿报告，不改原文' },
  { id: 'humanize', hint: '去 AI 味改写' },
  { id: 'summary', hint: '总结（产物可另存）' },
];

const PLACEHOLDER: Record<TaskId, string> = {
  chat: '和助手聊聊这本书…',
  plot: '想推演什么？例如：第 5 章后主角如何反击',
  outline: '对这一章细纲有什么要求？（可留空直接起草）',
  body: '本章正文的特别要求（可留空，按细纲写）',
  revise: '怎么改？例如：节奏更紧凑，减少形容词',
  review: '审读重点？例如：设定一致性、爽点节奏',
  humanize: '有特别要求吗？（可留空）',
  summary: '总结什么？',
  distill: '蒸馏要求',
};

export function Composer() {
  const { composer, setComposer, send, stop, run, bookId, doc } = useWs();
  const [text, setText] = useState('');
  const [preview, setPreview] = useState<TaskPreview | null>(null);
  const [previewErr, setPreviewErr] = useState<string | null>(null);
  const [showPlan, setShowPlan] = useState(false);
  const [picker, setPicker] = useState(false);
  const composing = useRef(false);
  const busy = !!run && !isTerminal(run.status);
  const needsCh = composer.task === 'body' || composer.task === 'outline';
  const ch = composer.target?.ch;

  // 计划预览（防抖）：任务/目标/技能变化时向服务端解析实际生效计划
  useEffect(() => {
    if (!bookId) return;
    let alive = true;
    const t = setTimeout(async () => {
      try {
        const target: Record<string, unknown> = { ...(composer.target ?? {}) };
        delete target.label;
        const p = await api.taskPreview(bookId, composer.task, composer.selection, target, composer.files);
        if (alive) {
          setPreview(p);
          setPreviewErr(null);
        }
      } catch (e) {
        if (alive) setPreviewErr(errorText(e));
      }
    }, 350);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [bookId, composer.task, composer.target, composer.selection, composer.files]);

  const submit = () => {
    if (busy) return;
    const msg = text.trim() || (composer.task !== 'chat' ? `${TASK_LABELS[composer.task]}${ch ? `：第${ch}章` : ''}` : '');
    if (!msg) return;
    setText('');
    void send(msg);
  };

  const blockers = preview?.context.blockers ?? [];
  const plan = preview?.plan;
  const primary = plan?.skills.find((s) => s.role === 'primary');
  const supports = plan?.skills.filter((s) => s.role === 'support') ?? [];

  return (
    <div className="composer">
      <div className="composer__top">
        <div className="composer__tasks" role="radiogroup" aria-label="任务类型">
          {TASKS.map((t) => (
            <button
              key={t.id}
              role="radio"
              aria-checked={composer.task === t.id}
              className={`chip${composer.task === t.id ? ' chip--active' : ''}`}
              title={t.hint}
              onClick={() => {
                const keepTarget = composer.target && (t.id === 'revise' || t.id === 'humanize' || t.id === 'review' || t.id === 'summary' || !composer.target.start);
                setComposer({ task: t.id, target: keepTarget ? composer.target : null });
              }}
            >
              {TASK_LABELS[t.id]}
            </button>
          ))}
        </div>

        <div className="composer__context">
          {composer.target && (composer.target.name || composer.target.start !== undefined || !needsCh) ? (
            <span className="chip chip--active composer__target" title={composer.target.label}>
              <Icon name="target" size={14} />
              <span className="ellipsis">{composer.target.label}</span>
              <button className="icon-btn icon-btn--sm" onClick={() => setComposer({ target: null })} aria-label="清除目标">
                <Icon name="x" size={13} />
              </button>
            </span>
          ) : null}
          {needsCh ? (
            <label className="chip composer__ch">
              第
              <input
                type="number"
                min={1}
                inputMode="numeric"
                value={ch ?? ''}
                placeholder="N"
                aria-label="目标章号"
                onChange={(e) => {
                  const n = Number(e.target.value);
                  setComposer({ target: n > 0 ? { ...(composer.target?.start === undefined ? composer.target ?? {} : {}), ch: n, label: `第${n}章` } : null });
                }}
              />
              章
            </label>
          ) : null}
          {!composer.target && doc && composer.task !== 'chat' && composer.task !== 'body' && composer.task !== 'outline' ? (
            <button
              className="chip"
              onClick={() => setComposer({ target: { group: doc.group, name: doc.name, baseHash: doc.baseHash ?? undefined, label: `文档：${doc.name}` } })}
            >
              <Icon name="file" size={14} />
              以当前文档为目标
            </button>
          ) : null}
          <button className={`chip${primary || supports.length || composer.selection.styleKey ? ' chip--active' : ''}`} onClick={() => setPicker(true)} aria-haspopup="dialog">
            <Icon name="layers" size={14} />
            {primary ? <span className="ellipsis">{primary.name}</span> : '技能'}
            {supports.length ? <span className="faint">+{supports.length}</span> : null}
          </button>
          <button className="chip" onClick={() => setShowPlan((s) => !s)} aria-expanded={showPlan}>
            <Icon name={showPlan ? 'chevronDown' : 'chevronRight'} size={14} />
            生效计划
            {plan?.excluded.length ? <span className="faint">（{plan.excluded.length} 项未用）</span> : null}
          </button>
        </div>

        {showPlan ? <PlanPreview preview={preview} error={previewErr} /> : null}
        {blockers.length ? (
          <div className="notice notice--pending" role="status">
            <Icon name="alert" size={16} />
            <div className="stack stack--tight">
              {blockers.map((b) => (
                <span key={b}>{b}</span>
              ))}
            </div>
          </div>
        ) : null}
      </div>

      <div className="composer__box">
        <textarea
          className="composer__input"
          rows={3}
          value={text}
          placeholder={PLACEHOLDER[composer.task]}
          aria-label="给创作助手的指令"
          onChange={(e) => setText(e.target.value)}
          onCompositionStart={() => (composing.current = true)}
          onCompositionEnd={() => (composing.current = false)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey && !composing.current && !e.nativeEvent.isComposing) {
              e.preventDefault();
              submit();
            }
          }}
        />
        <div className="composer__actions">
          <label className="checkbox small muted" title="直接生成：不调用工具，适合不支持工具调用的模型">
            <input type="checkbox" checked={composer.mode === 'direct'} onChange={(e) => setComposer({ mode: e.target.checked ? 'direct' : 'agent' })} />
            直接生成
          </label>
          <span className="grow" />
          {busy ? (
            <button className="btn btn--danger" onClick={() => void stop()}>
              <Icon name="stop" size={15} />
              停止
            </button>
          ) : (
            <button className="btn btn--primary" onClick={submit} disabled={needsCh && !ch}>
              <Icon name="send" size={15} />
              发送
            </button>
          )}
        </div>
      </div>
      {picker ? <SkillPicker onClose={() => setPicker(false)} preview={preview} /> : null}
    </div>
  );
}
