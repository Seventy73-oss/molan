import { useEffect, useMemo, useState } from 'react';
import { Dialog } from '../../components/Dialog';
import { DiffView } from '../../components/DiffView';
import { Icon } from '../../components/Icon';
import { Markdown } from '../../components/Markdown';
import { Spinner } from '../../components/Status';
import { toast } from '../../components/Toasts';
import { api } from '../../lib/api';
import type { ArtifactView, Book, DocRead, WriteOp } from '../../lib/contracts';
import { errorText } from '../../lib/ipc';
import { useWs } from '../../state/workspace';
import { useSkillDrafts } from '../skills/drafts';
import { deliver } from './actions';

export type DialogKind = 'destination' | 'apply' | 'compare' | 'edit' | 'read' | 'bookSetup';

export function ArtifactDialogs({ a, kind, onClose }: { a: ArtifactView; kind: DialogKind; onClose: () => void }) {
  switch (kind) {
    case 'destination':
      return <DestinationDialog a={a} onClose={onClose} />;
    case 'apply':
      return <ApplyDialog a={a} onClose={onClose} />;
    case 'compare':
      return <CompareDialog a={a} onClose={onClose} />;
    case 'edit':
      return <EditDialog a={a} onClose={onClose} />;
    case 'read':
      return <ReadDialog a={a} onClose={onClose} />;
    case 'bookSetup':
      return <BookSetupDialog a={a} onClose={onClose} />;
  }
}

function useFull(a: ArtifactView): string | null {
  const bookId = useWs((s) => s.bookId);
  const [text, setText] = useState<string | null>(a.truncated ? null : a.content);
  useEffect(() => {
    if (!a.truncated || a.legacy || !bookId) return;
    api.artifact(bookId, a.id).then((v) => setText(v.content)).catch((e) => toast.bad(errorText(e)));
  }, [a.id, a.truncated, a.legacy, bookId]);
  return text;
}

function safeFileName(s: string): string {
  const base = s.replace(/[\\/:*?"<>|\n\r\t]/g, '').replace(/^\.+/, '').trim().slice(0, 60) || '未命名';
  return /\.(md|txt)$/i.test(base) ? base : `${base}.md`;
}

/** 交付去向：明确区分「新建」与「追加/替换已有文档」；替换前必须先看真实差异。 */
function DestinationDialog({ a, onClose }: { a: ArtifactView; onClose: () => void }) {
  const { tree, bookId, book } = useWs();
  const groups = tree.filter((g) => g.dir !== '正文待审').map((g) => g.dir);
  const suggestedGroup = a.kind === 'outline_draft' ? '细纲' : a.kind === 'body_draft' ? '参考' : a.task === 'review' || a.task === 'plot' || a.task === 'summary' ? '参考' : groups.includes('设定') ? '设定' : groups[0] ?? '参考';
  const [group, setGroup] = useState(suggestedGroup);
  const [name, setName] = useState(safeFileName(a.title || a.kindLabel));
  const [op, setOp] = useState<WriteOp>('create');
  const [existing, setExisting] = useState<DocRead | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const content = useFull(a);
  const exists = useMemo(() => tree.find((g) => g.dir === group)?.files.some((f) => f.name === name) ?? false, [tree, group, name]);
  const canRewrite = a.scope !== 'fragment' && !['review_report', 'plot_note', 'summary', 'distill_note'].includes(a.kind);

  useEffect(() => {
    setOp(exists ? 'append' : 'create');
    setExisting(null);
    if (exists && bookId) api.readDoc(bookId, group, name).then(setExisting).catch(() => setExisting(null));
  }, [exists, group, name, bookId]);

  const submit = async () => {
    setError(null);
    const r = await deliver(
      a,
      { action: 'save', group, name, op, baseHash: op === 'create' ? null : existing?.hash ?? null },
      { setBusy: (b) => setBusy(b), setError },
    );
    if (r?.delivery.ok) onClose();
  };

  return (
    <Dialog
      title="选择保存位置"
      wide={op === 'replace'}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            取消
          </button>
          <button className="btn btn--primary" onClick={() => void submit()} disabled={busy || !content || (op !== 'create' && !existing)}>
            {busy ? <Icon name="refresh" size={15} className="spin" /> : <Icon name="save" size={15} />}
            {op === 'create' ? '新建保存' : op === 'append' ? '追加到末尾' : '替换全文'}
          </button>
        </>
      }
    >
      <p className="small muted">保存到《{book?.title}》。AI 产物写入会留下写入回执与旧版本快照；锁定的文件不会被写入。</p>
      <div className="row row--wrap">
        <label className="field">
          <span className="label">分组</span>
          <select className="select" value={group} onChange={(e) => setGroup(e.target.value)}>
            {groups.map((g) => (
              <option key={g}>{g}</option>
            ))}
          </select>
        </label>
        <label className="field grow">
          <span className="label">文件名</span>
          <input className="input" value={name} onChange={(e) => setName(e.target.value)} data-autofocus />
        </label>
      </div>
      <div className="dest-path break">
        目标：<strong>{group}/{name}</strong> · {exists ? <span className="status status--pending">已存在</span> : <span className="status status--ok">新文件</span>}
      </div>
      {exists ? (
        <fieldset className="stack stack--tight dest-ops">
          <legend className="label">已有同名文档，选择操作</legend>
          <label className="checkbox">
            <input type="radio" name="op" checked={op === 'append'} onChange={() => setOp('append')} />
            追加到文档末尾（不改动原有内容）
          </label>
          {canRewrite ? (
            <label className="checkbox">
              <input type="radio" name="op" checked={op === 'replace'} onChange={() => setOp('replace')} />
              替换全文（旧内容保留在版本历史）
            </label>
          ) : (
            <span className="hint">此类产物不能替换已有文档，可追加或换个文件名新建。</span>
          )}
          <span className="hint">或修改上方文件名以新建。</span>
        </fieldset>
      ) : null}
      {op === 'replace' && existing && content != null ? <DiffView before={existing.content} after={content} beforeLabel="现有文档" afterLabel="产物" /> : null}
      {error ? <p className="notice notice--bad">{error}</p> : null}
    </Dialog>
  );
}

/** 整篇替换回原文档：先读取当前文档并与产物比较；原文已变时只能比较或另存。 */
function ApplyDialog({ a, onClose }: { a: ArtifactView; onClose: () => void }) {
  const bookId = useWs((s) => s.bookId);
  const t = a.target;
  const [doc, setDoc] = useState<DocRead | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const content = useFull(a);
  useEffect(() => {
    if (bookId && t.group && t.name) api.readDoc(bookId, t.group, t.name).then(setDoc).catch((e) => setError(errorText(e)));
  }, [bookId, t.group, t.name]);
  const changed = doc && t.baseHash && doc.hash !== t.baseHash;
  return (
    <Dialog
      title={`替换 ${t.group}/${t.name}`}
      wide
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            取消
          </button>
          <button
            className="btn btn--primary"
            disabled={!doc || !!changed || busy}
            onClick={async () => {
              const r = await deliver(a, { action: 'save', op: 'replace', group: t.group, name: t.name, baseHash: t.baseHash }, { setBusy, setError });
              if (r?.delivery.ok) onClose();
            }}
          >
            确认替换全文
          </button>
        </>
      }
    >
      {!doc || content == null ? <Spinner label="读取当前文档" /> : <DiffView before={doc.content} after={content} beforeLabel="当前文档" afterLabel="产物" />}
      {changed ? <p className="notice notice--pending">生成后原文已被修改：为避免覆盖新内容，不能直接替换。请手动合并，或把产物另存为新文档。</p> : null}
      {error ? <p className="notice notice--bad">{error}</p> : null}
    </Dialog>
  );
}

function CompareDialog({ a, onClose }: { a: ArtifactView; onClose: () => void }) {
  const bookId = useWs((s) => s.bookId);
  const last = [...a.deliveries].reverse().find((d) => (d.status === 'committed' || d.status === 'noop') && d.name);
  const loc = last ? { group: last.group, name: last.name } : a.target.group && a.target.name ? { group: a.target.group, name: a.target.name } : null;
  const [doc, setDoc] = useState<DocRead | null>(null);
  const [error, setError] = useState<string | null>(null);
  const content = useFull(a);
  useEffect(() => {
    if (bookId && loc) api.readDoc(bookId, loc.group, loc.name).then(setDoc).catch((e) => setError(errorText(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bookId, loc?.group, loc?.name]);
  const fragment = a.scope === 'fragment' && a.target.selectionText;
  return (
    <Dialog title="与当前文件比较" wide onClose={onClose}>
      {!loc ? <p className="muted">此产物没有关联的文件。</p> : null}
      {error ? <p className="notice notice--bad">{error}</p> : null}
      {fragment && content != null ? <DiffView before={a.target.selectionText ?? ''} after={content} beforeLabel="原选区" afterLabel="改写" /> : null}
      {loc && doc && content != null && !fragment ? (
        doc.exists ? <DiffView before={doc.content} after={content} beforeLabel={`当前 ${loc.group}/${loc.name}`} afterLabel="产物" /> : <p className="muted">文件 {loc.group}/{loc.name} 已不存在。</p>
      ) : null}
    </Dialog>
  );
}

/** 作者编辑 AI 产物：形成新修订（保留原生成内容与来源），不直接写入书稿。 */
function EditDialog({ a, onClose }: { a: ArtifactView; onClose: () => void }) {
  const bookId = useWs((s) => s.bookId);
  const full = useFull(a);
  const [text, setText] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (full != null && text == null) setText(full);
  }, [full, text]);
  if (a.legacy) {
    return (
      <Dialog title="编辑产物" onClose={onClose}>
        <p className="muted">旧版记录不支持在线修订，请先保存到书稿后在编辑器中修改。</p>
      </Dialog>
    );
  }
  return (
    <Dialog
      title={`编辑「${a.title}」（形成修订 ${a.rev + 1}）`}
      wide
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            取消
          </button>
          <button
            className="btn btn--primary"
            disabled={busy || text == null || text === full}
            onClick={async () => {
              if (text == null) return;
              const skill = a.kind === 'skill_draft';
              if (!skill && !bookId) return;
              setBusy(true);
              try {
                if (skill) {
                  const v = await api.skillDraftRevise(a.id, a.rev, text);
                  useSkillDrafts.getState().upsert(v);
                  toast.ok(`已保存为草稿修订 ${v.rev}（尚未保存为技能）`);
                  onClose();
                  return;
                }
                const v = await api.reviseArtifact(bookId!, a.id, a.rev, text);
                useWs.getState().upsertArtifact(v);
                toast.ok(`已保存为产物修订 ${v.rev}（尚未写入书稿）`);
                onClose();
              } catch (e) {
                setError(errorText(e));
              } finally {
                setBusy(false);
              }
            }}
          >
            保存为新修订
          </button>
        </>
      }
    >
      {text == null ? <Spinner /> : <textarea className="textarea artifact-editor" value={text} onChange={(e) => setText(e.target.value)} rows={18} aria-label="产物内容" />}
      <p className="hint">原生成内容保留为修订 1；保存后卡片显示「尚未保存」，需要再次选择去向写入书稿。</p>
      {error ? <p className="notice notice--bad">{error}</p> : null}
    </Dialog>
  );
}

function ReadDialog({ a, onClose }: { a: ArtifactView; onClose: () => void }) {
  const full = useFull(a);
  return (
    <Dialog title={a.title || a.kindLabel} wide onClose={onClose}>
      {full == null ? <Spinner /> : <Markdown text={full} className="reading" />}
    </Dialog>
  );
}

/** 旧「建书资料包」卡片：明确选择新建作品或已有作品，并逐项确认目录与文件名。 */
function BookSetupDialog({ a, onClose }: { a: ArtifactView; onClose: () => void }) {
  const { bookId } = useWs();
  const [books, setBooks] = useState<Book[]>([]);
  const [mode, setMode] = useState<'new' | 'existing'>('new');
  const [title, setTitle] = useState('');
  const [target, setTarget] = useState('');
  const [rows, setRows] = useState(() =>
    (a.items ?? []).map((it) => ({ index: it.index, on: it.state !== 'saved', group: ['设定', '细纲', '参考'].includes(it.group ?? '') ? (it.group as string) : '设定', name: safeFileName(it.name || it.title || `资料${it.index + 1}`) })),
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api.listBooks().then(setBooks).catch(() => setBooks([]));
  }, []);
  const submit = async () => {
    if (!bookId) return;
    const files = rows.filter((r) => r.on).map((r) => ({ index: r.index, group: r.group, name: r.name }));
    if (!files.length) return setError('请至少选择一项');
    if (mode === 'new' && !title.trim()) return setError('请填写新作品书名');
    if (mode === 'existing' && !target) return setError('请选择目标作品');
    setBusy(true);
    setError(null);
    try {
      const r = await api.saveBookSetup({
        sourceBookId: bookId,
        messageId: a.messageId,
        destination: mode === 'new' ? { mode, title: title.trim() } : { mode, bookId: target },
        files,
      });
      const saved = ((r.savedFiles as unknown[]) ?? []).length;
      toast.ok(`建书资料已保存 ${saved} 项${r.complete ? '' : '（还有未保存项）'}`);
      await useWs.getState().reloadSession();
      onClose();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title="保存建书资料包"
      wide
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            取消
          </button>
          <button className="btn btn--primary" disabled={busy} onClick={() => void submit()}>
            保存所选项
          </button>
        </>
      }
    >
      <fieldset className="row row--wrap">
        <label className="checkbox">
          <input type="radio" checked={mode === 'new'} onChange={() => setMode('new')} />
          新建独立作品
        </label>
        <label className="checkbox">
          <input type="radio" checked={mode === 'existing'} onChange={() => setMode('existing')} />
          保存到已有作品（不改书名）
        </label>
      </fieldset>
      {mode === 'new' ? (
        <input className="input" placeholder="新作品书名" value={title} onChange={(e) => setTitle(e.target.value)} />
      ) : (
        <select className="select" value={target} onChange={(e) => setTarget(e.target.value)}>
          <option value="">选择作品…</option>
          {books.map((b) => (
            <option key={b.id} value={b.id}>
              《{b.title}》
            </option>
          ))}
        </select>
      )}
      <div className="setup-rows">
        {rows.map((r, i) => (
          <div key={r.index} className="row row--wrap setup-row">
            <input type="checkbox" checked={r.on} onChange={(e) => setRows(rows.map((x, j) => (j === i ? { ...x, on: e.target.checked } : x)))} aria-label="保存此项" />
            <select className="select select--sm" value={r.group} onChange={(e) => setRows(rows.map((x, j) => (j === i ? { ...x, group: e.target.value } : x)))}>
              {['设定', '细纲', '参考'].map((g) => (
                <option key={g}>{g}</option>
              ))}
            </select>
            <input className="input grow" value={r.name} onChange={(e) => setRows(rows.map((x, j) => (j === i ? { ...x, name: e.target.value } : x)))} aria-label="文件名" />
          </div>
        ))}
      </div>
      <p className="hint">内容由服务端从这条消息读取；同名不同内容不会覆盖，已保存项不能换路径。</p>
      {error ? <p className="notice notice--bad">{error}</p> : null}
    </Dialog>
  );
}
