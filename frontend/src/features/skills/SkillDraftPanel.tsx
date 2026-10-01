import { useEffect, useState } from 'react';
import { Icon } from '../../components/Icon';
import { Spinner } from '../../components/Status';
import { api } from '../../lib/api';
import type { TaskId } from '../../lib/contracts';
import { errorText } from '../../lib/ipc';
import { TASK_LABELS } from '../../state/workspace';
import { ArtifactCard } from '../artifacts/ArtifactCard';
import { useSkillDrafts } from './drafts';

const TASKS: TaskId[] = ['body', 'outline', 'plot', 'revise', 'review', 'humanize', 'summary', 'chat'];

/**
 * 技能工坊「AI 起草」：模型生成的模板先成为「技能草稿」卡片（已生成，尚未保存），
 * 作者可编辑为新修订，再明确点「保存为技能」——草稿不会自动进入技能库。
 */
export function SkillDraftPanel({ onClose }: { onClose: () => void }) {
  const { drafts, load, upsert } = useSkillDrafts();
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [task, setTask] = useState<TaskId>('body');
  const [usage, setUsage] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    load().catch((e) => setError(errorText(e)));
  }, [load]);

  const generate = async () => {
    if (!name.trim()) return setError('请先填写技能名');
    setBusy(true);
    setError(null);
    try {
      upsert(await api.skillDraft({ name: name.trim(), description: description.trim(), task, usage: usage.trim() }));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="stack" style={{ padding: 20, maxWidth: 760 }}>
      <div className="row row--between">
        <h2 className="page__title">AI 起草技能</h2>
        <button className="btn btn--sm btn--ghost" onClick={onClose}>
          关闭
        </button>
      </div>
      <p className="small muted">模型生成的模板先作为草稿卡片保留，确认或修改后再「保存为技能」。会调用当前启用的模型渠道。</p>
      <form
        className="stack"
        onSubmit={(e) => {
          e.preventDefault();
          void generate();
        }}
      >
        <div className="row row--wrap">
        <label className="field grow">
          <span className="label">技能名</span>
          <input className="input" value={name} onChange={(e) => setName(e.target.value)} placeholder="如：对白潜台词" aria-label="技能名" />
        </label>
        <label className="field">
          <span className="label">适用任务</span>
          <select className="select" value={task} onChange={(e) => setTask(e.target.value as TaskId)}>
            {TASKS.map((t) => (
              <option key={t} value={t}>
                {TASK_LABELS[t]}
              </option>
            ))}
          </select>
        </label>
        </div>
        <label className="field">
          <span className="label">描述</span>
          <input className="input" value={description} onChange={(e) => setDescription(e.target.value)} placeholder="这个技能要解决什么问题" />
        </label>
        <label className="field">
          <span className="label">用法场景（可选）</span>
          <input className="input" value={usage} onChange={(e) => setUsage(e.target.value)} placeholder="例如：打斗场面、对话密集的章节" />
        </label>
        <div className="row">
          <button className="btn btn--primary" type="submit" disabled={busy}>
            {busy ? <Icon name="refresh" size={15} className="spin" /> : <Icon name="spark" size={15} />}
            生成草稿
          </button>
        </div>
      </form>
      {error ? <p className="notice notice--bad">{error}</p> : null}
      <span className="section-title">草稿</span>
      {!drafts ? (
        <Spinner label="读取草稿" />
      ) : drafts.length === 0 ? (
        <p className="empty small">还没有技能草稿。</p>
      ) : (
        <div className="stack">
          {drafts.map((a) => (
            <ArtifactCard key={a.id} a={a} />
          ))}
        </div>
      )}
    </div>
  );
}
