import { useEffect, useMemo, useState } from 'react';
import { Dialog } from '../../components/Dialog';
import { Icon } from '../../components/Icon';
import { Spinner } from '../../components/Status';
import { toast } from '../../components/Toasts';
import { api, type Skill } from '../../lib/api';
import type { TaskPreview } from '../../lib/contracts';
import { errorText } from '../../lib/ipc';
import { TASK_LABELS, useWs } from '../../state/workspace';

const HUMANIZE = [
  { v: '', label: '按作品设置' },
  { v: 'official:standard', label: '官方标准去味' },
  { v: 'official:deep', label: '官方深度去味' },
  { v: 'none', label: '本次不去味' },
];

/**
 * 技能选择：本次使用（只影响这一次）与「设为本书默认」是两个独立操作。
 * 不适用当前任务的技能仍可见但标明原因，服务端最终解析才是执行依据。
 */
export function SkillPicker({ onClose, preview }: { onClose: () => void; preview: TaskPreview | null }) {
  const { composer, setComposer, bookId } = useWs();
  const task = composer.task;
  const [skills, setSkills] = useState<Skill[] | null>(null);
  const [q, setQ] = useState('');
  const [primary, setPrimary] = useState(composer.selection.primarySkillId ?? '');
  const [supports, setSupports] = useState<string[]>(composer.selection.supportSkillIds ?? []);
  const [style, setStyle] = useState(composer.selection.styleKey ?? '');
  const [humanize, setHumanize] = useState(composer.selection.humanize ?? '');
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.skills().then(setSkills).catch((e) => toast.bad(errorText(e)));
  }, []);

  const applies = (s: Skill) => {
    const t = s.targets?.length ? s.targets : [];
    if (t.length) return t.includes(task);
    return true; // 无 targets 的技能由服务端兜底规则判断，预览会给出结论
  };
  const list = useMemo(() => {
    const s = q.trim();
    return (skills ?? [])
      .filter((x) => x.enabled && x.kind !== 'style')
      .filter((x) => !s || x.name.includes(s) || x.description.includes(s))
      .sort((a, b) => Number(applies(b)) - Number(applies(a)) || a.name.localeCompare(b.name, 'zh-Hans-CN'));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [skills, q, task]);
  const styles = (skills ?? []).filter((x) => x.enabled && x.kind === 'style');
  const recommend = preview?.recommend ?? [];

  const applyNow = () => {
    setComposer({
      selection: {
        primarySkillId: primary || undefined,
        supportSkillIds: supports.length ? supports : undefined,
        styleKey: style || undefined,
        humanize: humanize || undefined,
      },
    });
    onClose();
  };

  const setDefault = async () => {
    if (!bookId) return;
    setBusy(true);
    try {
      await api.setBookPrimary(bookId, task, primary);
      await api.setBookSupports(bookId, task, supports);
      toast.ok(`已设为本书「${TASK_LABELS[task]}」任务的默认技能（不影响其他作品）`);
      setComposer({ selection: { styleKey: style || undefined, humanize: humanize || undefined } });
      onClose();
    } catch (e) {
      toast.bad(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      title={`为「${TASK_LABELS[task]}」选择技能`}
      onClose={onClose}
      wide
      footer={
        <>
          <button className="btn btn--ghost" onClick={() => { setPrimary(''); setSupports([]); setStyle(''); setHumanize(''); }}>
            清空本次选择
          </button>
          <span className="grow" />
          <button className="btn" onClick={() => void setDefault()} disabled={busy}>
            设为本书默认
          </button>
          <button className="btn btn--primary" onClick={applyNow}>
            仅本次使用
          </button>
        </>
      }
    >
      {recommend.length ? (
        <div className="stack stack--tight">
          <span className="section-title">推荐（依据技能元数据，不调用模型）</span>
          {recommend.map((r) => (
            <div key={r.skillId} className="recommend">
              <Icon name="spark" size={15} />
              <span className="grow small">{r.reason}</span>
              <button
                className="btn btn--sm"
                onClick={() => {
                  if (r.action === 'set_primary') setPrimary(r.skillId);
                  else setSupports((s) => (s.includes(r.skillId) ? s : [...s, r.skillId]));
                }}
              >
                {r.action === 'set_primary' ? '作为本次主技能' : '加入辅助'}
              </button>
            </div>
          ))}
        </div>
      ) : null}
      <label className="search">
        <Icon name="search" size={16} />
        <input className="search__input" placeholder="搜索技能" value={q} onChange={(e) => setQ(e.target.value)} />
      </label>
      {!skills ? (
        <Spinner label="读取技能库" />
      ) : (
        <div className="skill-table" role="table" aria-label="技能列表">
          <div className="skill-table__head" role="row">
            <span role="columnheader">技能</span>
            <span role="columnheader">主技能</span>
            <span role="columnheader">辅助</span>
          </div>
          {list.map((s) => {
            const ok = applies(s);
            return (
              <div key={s.id} className={`skill-table__row${ok ? '' : ' is-dim'}`} role="row">
                <span role="cell" className="stack stack--tight" style={{ gap: 2, minWidth: 0 }}>
                  <span className="ellipsis">
                    {s.name} <span className="faint small">r{s.rev}</span>
                  </span>
                  <span className="small faint ellipsis">
                    {ok ? s.description || s.kind : `适用于 ${s.targets.map((t) => TASK_LABELS[t as keyof typeof TASK_LABELS] ?? t).join('/')}，当前任务不会注入`}
                  </span>
                </span>
                <span role="cell">
                  <input type="radio" name="primary" checked={primary === s.id} onChange={() => setPrimary(s.id)} aria-label={`把 ${s.name} 设为主技能`} />
                </span>
                <span role="cell">
                  <input
                    type="checkbox"
                    checked={supports.includes(s.id)}
                    onChange={(e) => setSupports((x) => (e.target.checked ? [...x, s.id] : x.filter((y) => y !== s.id)))}
                    aria-label={`把 ${s.name} 作为辅助`}
                  />
                </span>
              </div>
            );
          })}
        </div>
      )}
      <div className="row row--wrap">
        <label className="field grow">
          <span className="label">本次文风</span>
          <select className="select" value={style} onChange={(e) => setStyle(e.target.value)}>
            <option value="">按作品设置</option>
            <option value="off">本次不使用文风</option>
            <option value="auto">按题材默认文风</option>
            {styles.map((s) => (
              <option key={s.id} value={`style:${s.id}`}>
                文风卡：{s.name}
              </option>
            ))}
          </select>
        </label>
        <label className="field grow">
          <span className="label">本次去AI味</span>
          <select className="select" value={humanize} onChange={(e) => setHumanize(e.target.value)}>
            {HUMANIZE.map((h) => (
              <option key={h.v} value={h.v}>
                {h.label}
              </option>
            ))}
          </select>
        </label>
      </div>
      <p className="hint">「仅本次使用」不改作品默认；「设为本书默认」只更新本书该任务的主/辅助技能。选中后可在「生效计划」里看到服务端实际解析结果与未使用原因。</p>
    </Dialog>
  );
}
