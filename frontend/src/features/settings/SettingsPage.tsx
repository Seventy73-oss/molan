import { useCallback, useEffect, useMemo, useState } from 'react';
import { usePrefs } from '../../app/prefs';
import { ConfirmDialog, Dialog } from '../../components/Dialog';
import { Icon } from '../../components/Icon';
import { Pill, Spinner } from '../../components/Status';
import { toast } from '../../components/Toasts';
import { api, downloadBase64, type Channel, type Skill } from '../../lib/api';
import type { AppInfo, Book, TaskId } from '../../lib/contracts';
import { errorText } from '../../lib/ipc';
import { TASK_LABELS } from '../../state/workspace';

const TABS = [
  { id: 'channels', label: '模型渠道', icon: 'globe' },
  { id: 'roles', label: '角色分工', icon: 'route' },
  { id: 'book', label: '作品设置', icon: 'book' },
  { id: 'agent', label: '运行预算', icon: 'spark' },
  { id: 'appearance', label: '外观与编辑', icon: 'sun' },
  { id: 'data', label: '数据与用量', icon: 'download' },
  { id: 'about', label: '关于', icon: 'info' },
];

export default function SettingsPage({ tab, onNavigate }: { tab?: string; onNavigate: (tab: string) => void }) {
  const active = TABS.find((t) => t.id === tab)?.id ?? 'channels';
  const [settings, setSettings] = useState<Record<string, string> | null>(null);
  const reload = useCallback(async () => {
    try {
      setSettings(await api.settings());
    } catch (e) {
      toast.bad(errorText(e));
    }
  }, []);
  useEffect(() => {
    void reload();
  }, [reload]);
  return (
    <div className="page">
      <div className="page__inner">
        <header className="page__head">
          <h1 className="page__title">设置</h1>
        </header>
        <div className="settings">
          <nav className="settings__nav" aria-label="设置分类">
            {TABS.map((t) => (
              <button key={t.id} className={`settings__tab${active === t.id ? ' is-active' : ''}`} onClick={() => onNavigate(t.id)} aria-current={active === t.id ? 'page' : undefined}>
                <Icon name={t.icon} size={16} />
                {t.label}
              </button>
            ))}
          </nav>
          <section className="settings__body">
            {!settings ? (
              <Spinner label="读取设置" />
            ) : active === 'channels' ? (
              <Channels settings={settings} reload={reload} />
            ) : active === 'roles' ? (
              <Roles settings={settings} reload={reload} />
            ) : active === 'book' ? (
              <BookSettings />
            ) : active === 'agent' ? (
              <AgentBudget settings={settings} reload={reload} />
            ) : active === 'appearance' ? (
              <Appearance />
            ) : active === 'data' ? (
              <DataUsage />
            ) : (
              <About />
            )}
          </section>
        </div>
      </div>
    </div>
  );
}

function parseChannels(raw: string | undefined): Channel[] {
  try {
    const v = JSON.parse(raw ?? '[]') as Channel[];
    return Array.isArray(v) ? v.map((c) => ({ ...c, models: Array.isArray(c.models) ? c.models : [] })) : [];
  } catch {
    return [];
  }
}

function Channels({ settings, reload }: { settings: Record<string, string>; reload: () => Promise<void> }) {
  const channels = parseChannels(settings.channels).filter((c) => !c.builtin);
  const active = settings.active_channel ?? '';
  const [editing, setEditing] = useState<Channel | null>(null);
  const [keys, setKeys] = useState<Record<string, boolean>>({});
  const [testing, setTesting] = useState<string | null>(null);
  const [removing, setRemoving] = useState<Channel | null>(null);
  useEffect(() => {
    void Promise.all(channels.map(async (c) => [c.id, await api.hasChannelKey(c.id).catch(() => false)] as const)).then((r) => setKeys(Object.fromEntries(r)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [settings.channels]);
  const saveList = async (list: Channel[], activeId = active) => {
    const all = [...parseChannels(settings.channels).filter((c) => c.builtin), ...list];
    await api.setSettings({ channels: JSON.stringify(all), active_channel: activeId });
    await reload();
  };
  return (
    <div className="stack">
      <p className="muted">配置你自己的模型服务（OpenAI 兼容接口）。密钥只保存在服务端，界面不会读回明文。</p>
      {channels.length === 0 ? <div className="notice notice--pending">还没有可用渠道：添加一个后才能使用 AI 功能。</div> : null}
      <ul className="channel-list">
        {channels.map((c) => (
          <li key={c.id} className="channel">
            <div className="stack stack--tight grow" style={{ minWidth: 0 }}>
              <div className="row row--wrap">
                <strong>{c.label || c.id}</strong>
                {c.id === active ? <Pill tone="accent" icon="check">默认渠道</Pill> : null}
                {keys[c.id] ? <Pill tone="ok">已设置密钥</Pill> : <Pill tone="pending">未设置密钥</Pill>}
              </div>
              <span className="small muted break">{c.baseUrl}</span>
              <span className="small faint">默认模型：{c.model || '未设置'}{c.models.length ? ` · 共 ${c.models.length} 个模型` : ''}</span>
            </div>
            <div className="row row--wrap">
              {c.id !== active ? (
                <button className="btn btn--sm" onClick={() => void saveList(channels, c.id)}>
                  设为默认
                </button>
              ) : null}
              <button
                className="btn btn--sm"
                disabled={testing === c.id}
                onClick={async () => {
                  setTesting(c.id);
                  try {
                    const r = await api.testChannel(c.id);
                    if (r.ok) toast.ok(`连接正常（${r.totalMs ?? '?'}ms）：${r.output}`);
                    else toast.bad(`连接失败：${r.output}`);
                  } catch (e) {
                    toast.bad(errorText(e));
                  } finally {
                    setTesting(null);
                  }
                }}
              >
                {testing === c.id ? <Icon name="refresh" size={14} className="spin" /> : null}
                测试
              </button>
              <button className="btn btn--sm" onClick={() => setEditing(c)}>
                编辑
              </button>
              <button className="btn btn--sm btn--danger" onClick={() => setRemoving(c)}>
                删除
              </button>
            </div>
          </li>
        ))}
      </ul>
      <button className="btn btn--primary" style={{ alignSelf: 'flex-start' }} onClick={() => setEditing({ id: `ch-${Date.now().toString(36)}`, label: '', baseUrl: '', modelsUrl: '', model: '', models: [] })}>
        <Icon name="plus" size={15} />
        添加渠道
      </button>
      {editing ? (
        <ChannelDialog
          channel={editing}
          isNew={!channels.some((c) => c.id === editing.id)}
          onClose={() => setEditing(null)}
          onSave={async (c, key) => {
            const list = channels.some((x) => x.id === c.id) ? channels.map((x) => (x.id === c.id ? c : x)) : [...channels, c];
            if (key) await api.setChannelKey(c.id, key);
            await saveList(list, active || c.id);
            setEditing(null);
            toast.ok('渠道已保存');
          }}
        />
      ) : null}
      {removing ? (
        <ConfirmDialog
          title={`删除渠道「${removing.label || removing.id}」？`}
          body="使用该渠道的角色分工会回落到默认渠道。"
          confirmLabel="删除"
          danger
          onCancel={() => setRemoving(null)}
          onConfirm={async () => {
            await api.deleteChannelKey(removing.id).catch(() => undefined);
            const rest = channels.filter((c) => c.id !== removing.id);
            await saveList(rest, active === removing.id ? rest[0]?.id ?? '' : active);
            setRemoving(null);
          }}
        />
      ) : null}
    </div>
  );
}

function ChannelDialog({ channel, isNew, onClose, onSave }: { channel: Channel; isNew: boolean; onClose: () => void; onSave: (c: Channel, key: string) => Promise<void> }) {
  const [c, setC] = useState(channel);
  const [key, setKey] = useState('');
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [fetching, setFetching] = useState(false);
  const valid = /^(https?:\/\/|mock:\/\/)/.test(c.baseUrl.trim());
  return (
    <Dialog
      title={isNew ? '添加渠道' : `编辑渠道 · ${channel.label || channel.id}`}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            取消
          </button>
          <button
            className="btn btn--primary"
            disabled={busy || !valid}
            onClick={async () => {
              setBusy(true);
              setErr(null);
              try {
                await onSave({ ...c, baseUrl: c.baseUrl.trim(), modelsUrl: c.modelsUrl?.trim() }, key.trim());
              } catch (e) {
                setErr(errorText(e));
              } finally {
                setBusy(false);
              }
            }}
          >
            保存
          </button>
        </>
      }
    >
      <label className="field">
        <span className="label">名称</span>
        <input className="input" value={c.label} onChange={(e) => setC({ ...c, label: e.target.value })} placeholder="例如：DeepSeek" data-autofocus />
      </label>
      <label className="field">
        <span className="label">接口地址（baseUrl）</span>
        <input className="input" value={c.baseUrl} onChange={(e) => setC({ ...c, baseUrl: e.target.value })} placeholder="https://api.example.com/v1" />
        {!valid && c.baseUrl ? <span className="hint">必须以 http:// 或 https:// 开头</span> : null}
      </label>
      <label className="field">
        <span className="label">模型列表地址（可选）</span>
        <input className="input" value={c.modelsUrl ?? ''} onChange={(e) => setC({ ...c, modelsUrl: e.target.value })} placeholder="https://api.example.com/v1/models" />
      </label>
      <label className="field">
        <span className="label">密钥 {isNew ? '' : '（留空表示不修改）'}</span>
        <input className="input" type="password" autoComplete="off" value={key} onChange={(e) => setKey(e.target.value)} placeholder="sk-…" />
      </label>
      <div className="row row--wrap" style={{ alignItems: 'flex-end' }}>
        <label className="field grow">
          <span className="label">默认模型</span>
          <input className="input" list={`models-${c.id}`} value={c.model} onChange={(e) => setC({ ...c, model: e.target.value })} />
          <datalist id={`models-${c.id}`}>
            {c.models.map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
        </label>
        {!isNew ? (
          <button
            className="btn"
            disabled={fetching}
            onClick={async () => {
              setFetching(true);
              try {
                const models = await api.listChannelModels(c.id);
                setC({ ...c, models });
                toast.ok(`获取到 ${models.length} 个模型`);
              } catch (e) {
                toast.bad(errorText(e));
              } finally {
                setFetching(false);
              }
            }}
          >
            获取模型列表
          </button>
        ) : null}
      </div>
      {err ? <p className="notice notice--bad">{err}</p> : null}
    </Dialog>
  );
}

const ROLES: { id: string; label: string; hint: string }[] = [
  { id: 'chat', label: '创作助手（Agent）', hint: '对话与工具调用' },
  { id: 'outline', label: '剧情 / 细纲', hint: 'plot、outline 任务' },
  { id: 'chapter', label: '正文', hint: '正文起草、自动写作' },
  { id: 'review', label: '审稿 / 修改 / 去味', hint: 'review、revise、humanize' },
  { id: 'summary', label: '总结 / 记忆', hint: '章节记忆抽取' },
  { id: 'distill', label: '蒸馏 / 拆书', hint: '文风蒸馏' },
];

function Roles({ settings, reload }: { settings: Record<string, string>; reload: () => Promise<void> }) {
  const channels = parseChannels(settings.channels).filter((c) => !c.builtin);
  const profile = (role: string): { channelId: string; model: string } => {
    try {
      return { channelId: '', model: '', ...(JSON.parse(settings[`agent_profile__${role}`] ?? '{}') as object) };
    } catch {
      return { channelId: '', model: '' };
    }
  };
  return (
    <div className="stack">
      <p className="muted">每个角色可以使用不同的渠道和模型；未设置时使用默认渠道。系统不会在失败时静默换模型。</p>
      <div className="role-table">
        {ROLES.map((r) => {
          const p = profile(r.id);
          const ch = channels.find((c) => c.id === p.channelId);
          return (
            <div key={r.id} className="role-row">
              <div className="stack stack--tight" style={{ gap: 0 }}>
                <strong>{r.label}</strong>
                <span className="small faint">{r.hint}</span>
              </div>
              <select
                className="select"
                value={p.channelId}
                onChange={async (e) => {
                  try {
                    await api.setAgentProfile(r.id, e.target.value, '');
                    await reload();
                  } catch (err) {
                    toast.bad(errorText(err));
                  }
                }}
                aria-label={`${r.label} 渠道`}
              >
                <option value="">默认渠道</option>
                {channels.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.label || c.id}
                  </option>
                ))}
              </select>
              <input
                className="input"
                list={`role-models-${r.id}`}
                defaultValue={p.model}
                placeholder={ch?.model || '使用渠道默认模型'}
                aria-label={`${r.label} 模型`}
                onBlur={async (e) => {
                  if (e.target.value === p.model) return;
                  try {
                    await api.setAgentProfile(r.id, p.channelId, e.target.value.trim());
                    toast.ok(`已更新「${r.label}」模型`);
                    await reload();
                  } catch (err) {
                    toast.bad(errorText(err));
                  }
                }}
              />
              <datalist id={`role-models-${r.id}`}>
                {(ch?.models ?? []).map((m) => (
                  <option key={m} value={m} />
                ))}
              </datalist>
            </div>
          );
        })}
      </div>
    </div>
  );
}

const HUMANIZE_OPTS = [
  { v: 'official:standard', label: '官方标准去味' },
  { v: 'official:deep', label: '官方深度去味' },
  { v: 'none', label: '不去味' },
];
const BIND_TASKS: TaskId[] = ['plot', 'outline', 'body', 'revise', 'review', 'humanize', 'chat'];

function BookSettings() {
  const [books, setBooks] = useState<Book[]>([]);
  const [bookId, setBookId] = useState<string>(() => {
    try {
      return localStorage.getItem('molan.lastBook') ?? '';
    } catch {
      return '';
    }
  });
  const [style, setStyle] = useState<string>('');
  const [humanize, setHumanize] = useState<string>('official:standard');
  const [genres, setGenres] = useState<{ key: string; label: string }[]>([]);
  const [skills, setSkills] = useState<Skill[]>([]);
  const [bindings, setBindings] = useState<Record<string, { primary: string; supports: string[] }>>({});
  useEffect(() => {
    api.listBooks().then(setBooks).catch(() => undefined);
    api.genreStyles().then(setGenres).catch(() => undefined);
    api.skills().then(setSkills).catch(() => undefined);
  }, []);
  useEffect(() => {
    if (!bookId) return;
    api.settings().then((s) => {
      setStyle(s[`book_style__${bookId}`] ?? '');
      setHumanize(s[`book_humanize__${bookId}`] || 'official:standard');
    });
    api.bookBindings(bookId).then(setBindings).catch(() => setBindings({}));
  }, [bookId]);
  const name = (id: string) => skills.find((s) => s.id === id)?.name ?? (id ? `（不存在：${id.slice(0, 8)}）` : '—');
  const styleCards = skills.filter((s) => s.kind === 'style');
  if (!books.length) return <p className="muted">还没有作品。</p>;
  return (
    <div className="stack">
      <label className="field">
        <span className="label">作品</span>
        <select className="select" value={bookId} onChange={(e) => setBookId(e.target.value)}>
          <option value="">选择作品…</option>
          {books.map((b) => (
            <option key={b.id} value={b.id}>
              《{b.title}》
            </option>
          ))}
        </select>
      </label>
      {bookId ? (
        <>
          <div className="row row--wrap">
            <label className="field grow">
              <span className="label">默认文风</span>
              <select
                className="select"
                value={style}
                onChange={async (e) => {
                  setStyle(e.target.value);
                  await api.setBookStyle(bookId, e.target.value).then(() => toast.ok('默认文风已更新'), (err) => toast.bad(errorText(err)));
                }}
              >
                <option value="">作品蒸馏文风（未设置）</option>
                <option value="auto">按题材默认</option>
                <option value="off">不使用文风</option>
                <option value="distill">作品蒸馏文风</option>
                {genres.map((g) => (
                  <option key={g.key} value={g.key}>
                    题材：{g.label}
                  </option>
                ))}
                {styleCards.map((s) => (
                  <option key={s.id} value={`style:${s.id}`}>
                    文风卡：{s.name}
                  </option>
                ))}
              </select>
            </label>
            <label className="field grow">
              <span className="label">默认去AI味</span>
              <select
                className="select"
                value={humanize}
                onChange={async (e) => {
                  setHumanize(e.target.value);
                  await api.setBookHumanize(bookId, e.target.value).then(() => toast.ok('默认去味方式已更新'), (err) => toast.bad(errorText(err)));
                }}
              >
                {HUMANIZE_OPTS.map((h) => (
                  <option key={h.v} value={h.v}>
                    {h.label}
                  </option>
                ))}
              </select>
            </label>
          </div>
          <div className="stack stack--tight">
            <span className="section-title">各任务默认技能</span>
            <div className="bind-table">
              {BIND_TASKS.map((t) => (
                <div key={t} className="bind-row">
                  <strong>{TASK_LABELS[t]}</strong>
                  <span className="small">主：{name(bindings[t]?.primary ?? '')}</span>
                  <span className="small faint ellipsis">辅：{(bindings[t]?.supports ?? []).map(name).join('、') || '—'}</span>
                </div>
              ))}
            </div>
            <p className="hint">在工作台助手的「技能」里选择后点「设为本书默认」即可修改；本次使用不会改动这里。</p>
          </div>
        </>
      ) : null}
    </div>
  );
}

const BUDGET_KEYS = [
  { k: 'agent_max_tool_rounds', label: '单次运行最多工具轮数', hint: '1–32，默认 8；用尽后模型会基于已有结果收尾', min: 1, max: 32 },
  { k: 'agent_budget_tokens', label: '单次运行 token 预算', hint: '0 为不限；上游未返回用量时按字数估算计入', min: 0, max: 10_000_000 },
  { k: 'agent_retry_extra', label: '瞬态错误额外重试次数', hint: '0–3，默认 2，同模型退避，不换模型', min: 0, max: 3 },
  { k: 'max_tokens', label: '单次最大输出 tokens', hint: '默认 8192', min: 256, max: 200_000 },
];

function AgentBudget({ settings, reload }: { settings: Record<string, string>; reload: () => Promise<void> }) {
  const [vals, setVals] = useState<Record<string, string>>(() => Object.fromEntries(BUDGET_KEYS.map((b) => [b.k, settings[b.k] ?? ''])));
  const dirty = BUDGET_KEYS.some((b) => (settings[b.k] ?? '') !== vals[b.k]);
  return (
    <div className="stack">
      {BUDGET_KEYS.map((b) => (
        <label key={b.k} className="field">
          <span className="label">{b.label}</span>
          <input className="input input--num" type="number" min={b.min} max={b.max} value={vals[b.k]} placeholder="默认" onChange={(e) => setVals({ ...vals, [b.k]: e.target.value })} />
          <span className="hint">{b.hint}</span>
        </label>
      ))}
      <button
        className="btn btn--primary"
        style={{ alignSelf: 'flex-start' }}
        disabled={!dirty}
        onClick={async () => {
          try {
            await api.setSettings(Object.fromEntries(BUDGET_KEYS.map((b) => [b.k, vals[b.k]])));
            toast.ok('运行预算已保存，下次运行生效');
            await reload();
          } catch (e) {
            toast.bad(errorText(e));
          }
        }}
      >
        保存
      </button>
    </div>
  );
}

function Appearance() {
  const p = usePrefs();
  return (
    <div className="stack">
      <fieldset className="field">
        <legend className="label">主题</legend>
        <div className="row row--wrap">
          {(['system', 'light', 'dark'] as const).map((t) => (
            <label key={t} className="checkbox">
              <input type="radio" checked={p.theme === t} onChange={() => p.set({ theme: t })} />
              {t === 'system' ? '跟随系统' : t === 'light' ? '浅色' : '暗色'}
            </label>
          ))}
        </div>
      </fieldset>
      <label className="field">
        <span className="label">书稿字号：{p.fontSize}px</span>
        <input type="range" min={15} max={22} value={p.fontSize} onChange={(e) => p.set({ fontSize: Number(e.target.value) })} />
      </label>
      <label className="field">
        <span className="label">行高：{p.lineHeight.toFixed(1)}</span>
        <input type="range" min={1.6} max={2.2} step={0.1} value={p.lineHeight} onChange={(e) => p.set({ lineHeight: Number(e.target.value) })} />
      </label>
      <label className="field">
        <span className="label">阅读列宽：约 {p.measure} 字</span>
        <input type="range" min={32} max={56} value={p.measure} onChange={(e) => p.set({ measure: Number(e.target.value) })} />
      </label>
      <label className="checkbox">
        <input type="checkbox" checked={p.autosave} onChange={(e) => p.set({ autosave: e.target.checked })} />
        自动保存我的编辑（停止输入 {Math.round(p.autosaveDelay / 1000)} 秒后；AI 产物永远不会自动写入）
      </label>
      <div className="paper paper--sample">
        <p className="manuscript manuscript--sample">山风掠过石阶，少年背着一个旧行囊，站在了青岚宗的山门前。他抬头望着云雾深处的连绵殿宇，攥紧了手里的荐书。</p>
      </div>
    </div>
  );
}

function DataUsage() {
  const [usage, setUsage] = useState<Awaited<ReturnType<typeof api.usage>> | null>(null);
  useEffect(() => {
    api.usage().then(setUsage).catch(() => undefined);
  }, []);
  const total = useMemo(() => (usage?.byModel ?? []).reduce((s, x) => s + (x.totalTokens ?? 0), 0), [usage]);
  return (
    <div className="stack">
      <div className="row row--wrap">
        <button
          className="btn"
          onClick={async () => {
            try {
              const r = await api.exportBook('', 'all');
              if (!r.ok || !r.base64) throw new Error(r.message ?? '导出失败');
              downloadBase64(r.name ?? 'molan-all.zip', r.mime ?? 'application/zip', r.base64);
            } catch (e) {
              toast.bad(errorText(e));
            }
          }}
        >
          <Icon name="download" size={15} />
          导出全部数据
        </button>
      </div>
      <span className="section-title">近 7 天模型用量（写作链记账；Agent 运行另计于运行记录）</span>
      {!usage ? (
        <Spinner />
      ) : usage.byModel.length === 0 ? (
        <p className="small faint">暂无记录</p>
      ) : (
        <div className="usage-table" role="table">
          {usage.byModel.map((m, i) => (
            <div key={i} className="usage-row" role="row">
              <span className="ellipsis">{m.model}</span>
              <span className="faint small">{m.tag || '—'}</span>
              <span className="small">{m.calls} 次</span>
              <span className="small">{(m.totalTokens ?? 0).toLocaleString()} tokens</span>
            </div>
          ))}
          <div className="usage-row usage-row--total" role="row">
            <span>合计</span>
            <span />
            <span />
            <span className="small">{total.toLocaleString()} tokens</span>
          </div>
        </div>
      )}
    </div>
  );
}

function About() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  useEffect(() => {
    api.appInfo().then(setInfo).catch(() => undefined);
  }, []);
  return (
    <dl className="kv">
      <dt>应用</dt>
      <dd>墨澜工坊 · Paper Studio</dd>
      <dt>服务版本</dt>
      <dd>{info?.version ?? '—'}</dd>
      <dt>契约版本</dt>
      <dd>v{info?.contract ?? '—'}</dd>
      <dt>访问控制</dt>
      <dd>{info ? (info.authRequired ? '已启用登录' : '本机模式（未启用登录）') : '—'}</dd>
      <dt>旧界面</dt>
      <dd>部署旧静态树到 MOLAN_WEB_DIR 即可回退（旧入口自动注入原有脚本）</dd>
    </dl>
  );
}
