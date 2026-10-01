import { useCallback, useEffect, useMemo, useState } from 'react';
import { navigate, navigateFrom } from '../../app/router';
import { ConfirmDialog } from '../../components/Dialog';
import { DiffView } from '../../components/DiffView';
import { Icon } from '../../components/Icon';
import { Pill, Spinner } from '../../components/Status';
import { toast } from '../../components/Toasts';
import { api, type Skill } from '../../lib/api';
import type { TaskId } from '../../lib/contracts';
import { errorText } from '../../lib/ipc';
import { timeAgo } from '../../lib/text';
import { TASK_LABELS, useWs } from '../../state/workspace';
import { useSkillDrafts } from './drafts';
import { SkillDraftPanel } from './SkillDraftPanel';

const TASK_FILTERS: (TaskId | 'all' | 'style')[] = ['all', 'chat', 'plot', 'outline', 'body', 'revise', 'review', 'humanize', 'style'];
const USAGE: Record<string, string> = { primary: '主技能', support: '辅助', standalone: '独立' };
const KINDS = ['user', 'craft', 'method', 'style', 'imported', 'builtin'];

export default function SkillsPage({ skillId }: { skillId?: string }) {
  const [skills, setSkills] = useState<Skill[] | null>(null);
  const [q, setQ] = useState('');
  const [filter, setFilter] = useState<(typeof TASK_FILTERS)[number]>('all');
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [drafting, setDrafting] = useState(false);
  const savedVersion = useSkillDrafts((s) => s.savedVersion);

  const load = useCallback(async () => {
    try {
      setSkills(await api.skills());
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load, savedVersion]);
  useEffect(() => {
    if (skillId) setDrafting(false);
  }, [skillId, savedVersion]);

  const shown = useMemo(() => {
    const s = q.trim();
    return (skills ?? [])
      .filter((x) => !s || x.name.includes(s) || x.description.includes(s))
      .filter((x) => (filter === 'all' ? true : filter === 'style' ? x.kind === 'style' : x.kind !== 'style' && (x.targets.length ? x.targets.includes(filter) : true)))
      .sort((a, b) => Number(b.enabled) - Number(a.enabled) || a.name.localeCompare(b.name, 'zh-Hans-CN'));
  }, [skills, q, filter]);
  const current = skills?.find((s) => s.id === skillId) ?? null;

  return (
    <div className="page page--split">
      <div className="split">
        <section className={`split__list${current || creating || drafting ? ' is-hidden-narrow' : ''}`} aria-label="技能列表">
          <header className="stack stack--tight" style={{ padding: 16 }}>
            <div className="row row--between">
              <h1 className="page__title">技能库</h1>
              <div className="row">
                <button className="btn btn--sm" onClick={() => { setDrafting(true); setCreating(false); navigate({ name: 'skills' }); }}>
                  <Icon name="spark" size={15} />
                  AI 起草
                </button>
                <button className="btn btn--primary btn--sm" onClick={() => { setCreating(true); setDrafting(false); navigate({ name: 'skills' }); }}>
                  <Icon name="plus" size={15} />
                  新建
                </button>
              </div>
            </div>
            <label className="search">
              <Icon name="search" size={16} />
              <input className="search__input" placeholder="搜索技能" value={q} onChange={(e) => setQ(e.target.value)} aria-label="搜索技能" />
            </label>
            <div className="tabs" role="tablist" aria-label="按任务筛选">
              {TASK_FILTERS.map((f) => (
                <button key={f} className="tab" role="tab" aria-selected={filter === f} onClick={() => setFilter(f)}>
                  {f === 'all' ? '全部' : f === 'style' ? '文风卡' : TASK_LABELS[f]}
                </button>
              ))}
            </div>
          </header>
          {error ? <p className="notice notice--bad" style={{ margin: 16 }}>{error}</p> : null}
          {!skills ? (
            <Spinner label="读取技能" />
          ) : (
            <ul className="skill-list">
              {shown.map((s) => (
                <li key={s.id}>
                  <a className={`skill-item${s.id === skillId ? ' skill-item--active' : ''}${s.enabled ? '' : ' is-dim'}`} href={`#/skills/${encodeURIComponent(s.id)}`} onClick={() => setCreating(false)}>
                    <span className="stack stack--tight grow" style={{ gap: 2, minWidth: 0 }}>
                      <span className="ellipsis">
                        {s.name} <span className="faint small">r{s.rev}</span>
                      </span>
                      <span className="small faint ellipsis">{s.description || (s.kind === 'style' ? '文风卡' : '')}</span>
                    </span>
                    <span className="tag">{s.kind === 'style' ? '文风' : USAGE[s.usageMode] ?? s.usageMode}</span>
                    {!s.enabled ? <span className="tag">停用</span> : null}
                  </a>
                </li>
              ))}
              {shown.length === 0 ? <li className="empty small">没有匹配的技能</li> : null}
            </ul>
          )}
        </section>
        <section className="split__detail" aria-label="技能详情">
          {drafting ? (
            <SkillDraftPanel onClose={() => setDrafting(false)} />
          ) : creating ? (
            <SkillEditor key="new" onSaved={async (s) => { setCreating(false); await load(); navigate({ name: 'skills', skillId: s.id }); }} onCancel={() => setCreating(false)} />
          ) : current ? (
            <SkillDetail key={current.id} skill={current} onChanged={() => void load()} />
          ) : (
            <div className="empty">
              <Icon name="layers" size={30} />
              <p className="empty__title">选择一个技能查看详情</p>
              <p className="small">技能是方法知识：只影响提示，不扩大 AI 的写入权限。本次使用在创作助手里选择；作品默认在这里或助手里设置。</p>
            </div>
          )}
        </section>
      </div>
    </div>
  );
}

function SkillDetail({ skill, onChanged }: { skill: Skill; onChanged: () => void }) {
  const [editing, setEditing] = useState(false);
  const [revs, setRevs] = useState<{ rev: number; contentHash: string; ts: number; sourceEvent: string; promptTemplate: string }[] | null>(null);
  const [pickRev, setPickRev] = useState<number | null>(null);
  const [deleting, setDeleting] = useState(false);
  const ws = useWs();
  useEffect(() => {
    api.skillRevisions(skill.id).then(setRevs).catch(() => setRevs([]));
  }, [skill.id, skill.rev]);
  const old = revs?.find((r) => r.rev === pickRev);

  if (editing) return <SkillEditor skill={skill} onSaved={() => { setEditing(false); onChanged(); }} onCancel={() => setEditing(false)} />;

  const useNow = (role: 'primary' | 'support') => {
    const sel = ws.composer.selection;
    ws.setComposer({
      selection:
        role === 'primary'
          ? { ...sel, primarySkillId: skill.id }
          : { ...sel, supportSkillIds: [...new Set([...(sel.supportSkillIds ?? []), skill.id])] },
    });
    toast.ok(ws.bookId ? `已加入本次选择（${role === 'primary' ? '主技能' : '辅助'}），回到工作台发送任务即可` : '已加入本次选择；打开作品后在助手中使用');
    if (ws.bookId) navigate({ name: 'book', bookId: ws.bookId });
  };

  return (
    <div className="stack" style={{ padding: 'var(--sp-4) var(--sp-5)' }}>
      <a className="btn btn--ghost btn--sm show-narrow" href="#/skills">
        <Icon name="arrowLeft" size={15} />
        返回列表
      </a>
      <div className="row row--between row--wrap">
        <div className="stack stack--tight">
          <h2 className="serif" style={{ fontSize: 22 }}>{skill.name}</h2>
          <div className="row row--wrap">
            <Pill tone={skill.enabled ? 'ok' : 'neutral'} icon={skill.enabled ? 'check' : 'x'}>{skill.enabled ? '启用' : '停用'}</Pill>
            <span className="tag">{skill.kind === 'style' ? '文风卡' : USAGE[skill.usageMode] ?? skill.usageMode}</span>
            <span className="tag">{skill.origin}</span>
            <span className="tag">版本 r{skill.rev}</span>
            {skill.targets.length ? <span className="tag">适用：{skill.targets.map((t) => TASK_LABELS[t as TaskId] ?? t).join('、')}</span> : <span className="tag">适用：按类型推断</span>}
          </div>
        </div>
        <div className="row row--wrap">
          {skill.kind !== 'style' ? (
            <>
              <button className="btn btn--sm" onClick={() => useNow('primary')} disabled={!skill.enabled}>
                本次作为主技能
              </button>
              <button className="btn btn--sm" onClick={() => useNow('support')} disabled={!skill.enabled}>
                本次加入辅助
              </button>
            </>
          ) : null}
          <button className="btn btn--sm" onClick={async () => { try { await api.setSkillEnabled(skill.id, !skill.enabled); onChanged(); } catch (e) { toast.bad(errorText(e)); } }}>
            {skill.enabled ? '停用' : '启用'}
          </button>
          <button className="btn btn--sm" onClick={() => setEditing(true)}>
            <Icon name="edit" size={14} />
            编辑
          </button>
          {skill.origin !== 'official' ? (
            <button className="btn btn--sm btn--danger" onClick={() => setDeleting(true)}>
              删除
            </button>
          ) : null}
        </div>
      </div>
      {skill.description ? <p className="muted">{skill.description}</p> : null}
      <div className="stack stack--tight">
        <span className="section-title">提示模板（{skill.promptTemplate.length} 字）</span>
        <pre className="template">{skill.promptTemplate || '（空模板：不会被注入）'}</pre>
      </div>
      <div className="stack stack--tight">
        <span className="section-title">版本历史</span>
        {!revs ? (
          <Spinner />
        ) : revs.length <= 1 ? (
          <p className="small faint">只有当前版本。修改模板、适用任务、使用方式或类型会自动生成新版本；运行中的任务使用开始时冻结的版本。</p>
        ) : (
          <div className="row row--wrap">
            {revs.map((r) => (
              <button key={r.rev} className={`chip${pickRev === r.rev ? ' chip--active' : ''}`} onClick={() => setPickRev(pickRev === r.rev ? null : r.rev)}>
                r{r.rev} · {timeAgo(r.ts)}
              </button>
            ))}
          </div>
        )}
        {old ? <DiffView before={old.promptTemplate} after={skill.promptTemplate} beforeLabel={`r${old.rev}`} afterLabel={`当前 r${skill.rev}`} /> : null}
      </div>
      {deleting ? (
        <ConfirmDialog
          title={`删除技能「${skill.name}」？`}
          body="删除后不可恢复；已绑定该技能的作品会在下次运行时显示「技能不存在」并跳过。历史运行记录中的冻结快照不受影响。"
          confirmLabel="删除"
          danger
          onCancel={() => setDeleting(false)}
          onConfirm={async () => {
            try {
              const from = location.hash;
              await api.deleteSkill(skill.id);
              toast.ok('已删除');
              navigateFrom(from, { name: 'skills' });
              onChanged();
            } catch (e) {
              toast.bad(errorText(e));
            } finally {
              setDeleting(false);
            }
          }}
        />
      ) : null}
    </div>
  );
}

function SkillEditor({ skill, onSaved, onCancel }: { skill?: Skill; onSaved: (s: Skill) => void; onCancel: () => void }) {
  const [name, setName] = useState(skill?.name ?? '');
  const [description, setDescription] = useState(skill?.description ?? '');
  const [kind, setKind] = useState(skill?.kind ?? 'user');
  const [usageMode, setUsage] = useState(skill?.usageMode ?? 'support');
  const [targets, setTargets] = useState<string[]>(skill?.targets ?? ['body']);
  const [tpl, setTpl] = useState(skill?.promptTemplate ?? '');
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const save = async () => {
    if (!name.trim()) return setErr('请填写名称');
    if (kind !== 'style' && targets.length === 0) return setErr('请至少选择一个适用任务');
    setBusy(true);
    setErr(null);
    try {
      const body = { name: name.trim(), description, kind, usageMode, targets: kind === 'style' ? [] : targets, promptTemplate: tpl };
      const from = location.hash;
      const s = skill ? await api.updateSkill({ id: skill.id, ...body }) : await api.createSkill({ ...body, enabled: true });
      toast.ok(skill ? `已保存（新版本 r${s.rev}）` : '已创建技能');
      if (location.hash === from) onSaved(s); // 保存期间作者已离开：只提示，不跳转
    } catch (e) {
      setErr(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="stack" style={{ padding: 'var(--sp-4) var(--sp-5)' }}>
      <h2 className="serif" style={{ fontSize: 20 }}>{skill ? `编辑「${skill.name}」` : '新建技能'}</h2>
      <div className="row row--wrap">
        <label className="field grow">
          <span className="label">名称</span>
          <input className="input" value={name} onChange={(e) => setName(e.target.value)} />
        </label>
        <label className="field">
          <span className="label">类型</span>
          <select className="select" value={kind} onChange={(e) => setKind(e.target.value)}>
            {KINDS.map((k) => (
              <option key={k} value={k}>
                {k === 'style' ? '文风卡' : k}
              </option>
            ))}
          </select>
        </label>
        <label className="field">
          <span className="label">使用方式</span>
          <select className="select" value={usageMode} onChange={(e) => setUsage(e.target.value)} disabled={kind === 'style'}>
            <option value="primary">主技能</option>
            <option value="support">辅助</option>
            <option value="standalone">独立</option>
          </select>
        </label>
      </div>
      <label className="field">
        <span className="label">简介</span>
        <input className="input" value={description} onChange={(e) => setDescription(e.target.value)} />
      </label>
      {kind !== 'style' ? (
        <fieldset className="field">
          <legend className="label">适用任务（不适用的任务不会注入此技能）</legend>
          <div className="row row--wrap">
            {(['chat', 'plot', 'outline', 'body', 'revise', 'review', 'humanize', 'summary', 'distill'] as TaskId[]).map((t) => (
              <label key={t} className="checkbox">
                <input type="checkbox" checked={targets.includes(t)} onChange={(e) => setTargets(e.target.checked ? [...targets, t] : targets.filter((x) => x !== t))} />
                {TASK_LABELS[t]}
              </label>
            ))}
          </div>
        </fieldset>
      ) : (
        <p className="hint">文风卡走独立的文风通道：在作品设置或助手的「本次文风」中选择，不作为技能重复注入。</p>
      )}
      <label className="field">
        <span className="label">提示模板</span>
        <textarea className="textarea template-editor" rows={16} value={tpl} onChange={(e) => setTpl(e.target.value)} />
      </label>
      {err ? <p className="notice notice--bad">{err}</p> : null}
      <div className="row">
        <button className="btn" onClick={onCancel}>
          取消
        </button>
        <button className="btn btn--primary" onClick={() => void save()} disabled={busy}>
          保存
        </button>
      </div>
    </div>
  );
}
