import { useCallback, useEffect, useMemo, useState } from 'react';
import { navigate } from '../../app/router';
import { ConfirmDialog, Dialog } from '../../components/Dialog';
import { Icon } from '../../components/Icon';
import { Menu } from '../../components/Menu';
import { Spinner } from '../../components/Status';
import { toast } from '../../components/Toasts';
import { api, downloadBase64 } from '../../lib/api';
import type { Book } from '../../lib/contracts';
import { errorText } from '../../lib/ipc';
import { formatCount, timeAgo } from '../../lib/text';

const GENRES = ['玄幻', '仙侠', '都市', '言情', '历史', '武侠', '科幻', '悬疑灵异', '游戏', '军事', '其他'];
const POVS = ['第三人称', '第一人称', '多视角'];
const LAST_KEY = 'molan.lastBook';

function lastBookId(): string | null {
  try {
    return localStorage.getItem(LAST_KEY);
  } catch {
    return null;
  }
}

export function rememberBook(id: string) {
  try {
    localStorage.setItem(LAST_KEY, id);
  } catch {
    /* 忽略 */
  }
}

export function LibraryPage() {
  const [books, setBooks] = useState<Book[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [q, setQ] = useState('');
  const [dialog, setDialog] = useState<null | 'create' | 'import' | 'trash'>(null);
  const [deleting, setDeleting] = useState<Book | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      setBooks(await api.listBooks());
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const shown = useMemo(() => {
    const s = q.trim();
    return (books ?? []).filter((b) => !s || b.title.includes(s) || (b.genre ?? '').includes(s));
  }, [books, q]);
  const last = books?.find((b) => b.id === lastBookId()) ?? books?.[0];

  const exportBook = async (b: Book, format: 'zip' | 'txt') => {
    try {
      const r = await api.exportBook(b.id, format);
      if (!r.ok || !r.base64) throw new Error(r.message ?? '导出失败');
      downloadBase64(r.name ?? `${b.title}.${format}`, r.mime ?? 'application/octet-stream', r.base64);
      toast.ok(`已导出《${b.title}》`);
    } catch (e) {
      toast.bad(`导出失败：${errorText(e)}`);
    }
  };

  return (
    <div className="page">
      <div className="page__inner">
        <header className="page__head">
          <div>
            <h1 className="page__title">书库</h1>
            <p className="page__sub">{books ? `${books.length} 部作品` : '正在读取作品…'}</p>
          </div>
          <div className="row row--wrap">
            <label className="search">
              <Icon name="search" size={16} />
              <input className="search__input" placeholder="搜索书名或题材" value={q} onChange={(e) => setQ(e.target.value)} aria-label="搜索作品" />
            </label>
            <button className="btn" onClick={() => setDialog('import')}>
              <Icon name="upload" size={16} />
              导入
            </button>
            <button className="btn btn--primary" onClick={() => setDialog('create')}>
              <Icon name="plus" size={16} />
              新建作品
            </button>
          </div>
        </header>

        {error ? (
          <div className="notice notice--bad" role="alert">
            <Icon name="alert" size={16} />
            <span className="grow break">读取作品失败：{error}</span>
            <button className="btn btn--sm" onClick={() => void load()}>
              重试
            </button>
          </div>
        ) : null}

        {last && !q ? (
          <button className="continue" onClick={() => openBook(last.id)}>
            <span className="continue__cover serif">{last.coverChar || last.title.slice(0, 1) || '书'}</span>
            <span className="stack stack--tight grow">
              <span className="small faint">继续写作</span>
              <span className="continue__title break">《{last.title}》</span>
              <span className="small muted">
                {last.genre || '未设题材'} · {formatCount(last.wordCount)} 字 · {last.chapterCount} 章 · {timeAgo(last.updatedAt)}更新
              </span>
            </span>
            <Icon name="chevronRight" />
          </button>
        ) : null}

        {!books ? (
          <div className="book-grid">
            {[0, 1, 2].map((i) => (
              <div key={i} className="skeleton" style={{ height: 132 }} />
            ))}
          </div>
        ) : shown.length === 0 ? (
          <div className="empty">
            <Icon name="books" size={32} />
            <p className="empty__title">{q ? '没有匹配的作品' : '书库还是空的'}</p>
            <p>{q ? '换个关键词试试' : '新建一部作品，或导入已有的 txt/md 书稿'}</p>
            {!q ? (
              <button className="btn btn--primary" onClick={() => setDialog('create')}>
                新建作品
              </button>
            ) : null}
          </div>
        ) : (
          <ul className="book-grid" aria-label="作品列表">
            {shown.map((b) => (
              <li key={b.id} className="book-card">
                <button className="book-card__main" onClick={() => openBook(b.id)}>
                  <span className="book-card__cover serif" aria-hidden>
                    {b.coverChar || b.title.slice(0, 1) || '书'}
                  </span>
                  <span className="stack stack--tight grow">
                    <span className="book-card__title break">{b.title || '未命名'}</span>
                    <span className="small muted">
                      {b.genre || '未设题材'} · {b.pov || '视角未设'}
                    </span>
                    <span className="small faint">
                      {formatCount(b.wordCount)} 字 · {b.chapterCount} 章 · {timeAgo(b.updatedAt)}
                    </span>
                  </span>
                </button>
                <div className="book-card__menu">
                  <Menu
                    label={`《${b.title}》更多操作`}
                    small
                    items={[
                      { key: 'zip', label: '导出 Markdown 压缩包', icon: 'download', onSelect: () => void exportBook(b, 'zip') },
                      { key: 'txt', label: '导出正文 TXT', icon: 'download', onSelect: () => void exportBook(b, 'txt') },
                      'sep',
                      { key: 'del', label: '移到回收站', icon: 'trash', danger: true, onSelect: () => setDeleting(b) },
                    ]}
                  />
                </div>
              </li>
            ))}
          </ul>
        )}

        <footer className="row">
          <button className="btn btn--ghost btn--sm" onClick={() => setDialog('trash')}>
            <Icon name="trash" size={15} />
            作品回收站
          </button>
        </footer>
      </div>

      {dialog === 'create' ? <CreateBookDialog onClose={() => setDialog(null)} onCreated={(b) => openBook(b.id)} /> : null}
      {dialog === 'import' ? <ImportDialog onClose={() => setDialog(null)} onImported={(id) => openBook(id)} /> : null}
      {dialog === 'trash' ? <BookTrashDialog onClose={() => setDialog(null)} onRestored={() => void load()} /> : null}
      {deleting ? (
        <ConfirmDialog
          title="移到回收站"
          body={<>《{deleting.title}》会移到作品回收站，书稿文件保留，可随时恢复。</>}
          confirmLabel="移到回收站"
          danger
          busy={busy}
          onCancel={() => setDeleting(null)}
          onConfirm={async () => {
            setBusy(true);
            try {
              await api.deleteBook(deleting.id);
              toast.ok(`《${deleting.title}》已移到回收站`);
              setDeleting(null);
              await load();
            } catch (e) {
              toast.bad(errorText(e));
            } finally {
              setBusy(false);
            }
          }}
        />
      ) : null}
    </div>
  );
}

function openBook(id: string) {
  rememberBook(id);
  navigate({ name: 'book', bookId: id });
}

function CreateBookDialog({ onClose, onCreated }: { onClose: () => void; onCreated: (b: Book) => void }) {
  const [title, setTitle] = useState('');
  const [genre, setGenre] = useState(GENRES[0]);
  const [pov, setPov] = useState(POVS[0]);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const submit = async () => {
    if (!title.trim()) return setErr('请填写书名');
    setBusy(true);
    setErr(null);
    try {
      const from = location.hash;
      const b = await api.createBook(title.trim(), genre, pov);
      if (b.ok === false || !b.id) throw new Error(b.err ?? '创建失败');
      toast.ok(`已创建《${b.title}》`);
      if (location.hash === from) onCreated(b); // 创建期间作者已去别处：不强行打开新书
    } catch (e) {
      setErr(errorText(e));
      setBusy(false);
    }
  };
  return (
    <Dialog
      title="新建作品"
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            取消
          </button>
          <button className="btn btn--primary" onClick={() => void submit()} disabled={busy}>
            {busy ? <Icon name="refresh" size={16} className="spin" /> : null}
            创建
          </button>
        </>
      }
    >
      <form
        className="stack"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <label className="field">
          <span className="label">书名</span>
          <input className="input" value={title} onChange={(e) => setTitle(e.target.value)} maxLength={80} data-autofocus placeholder="例如：青岚纪" />
        </label>
        <div className="row row--wrap">
          <label className="field grow">
            <span className="label">题材</span>
            <select className="select" value={genre} onChange={(e) => setGenre(e.target.value)}>
              {GENRES.map((g) => (
                <option key={g}>{g}</option>
              ))}
            </select>
          </label>
          <label className="field grow">
            <span className="label">叙事视角</span>
            <select className="select" value={pov} onChange={(e) => setPov(e.target.value)}>
              {POVS.map((g) => (
                <option key={g}>{g}</option>
              ))}
            </select>
          </label>
        </div>
        <p className="hint">会创建「设定 / 细纲 / 正文 / 参考 / 正文待审」五个分组。</p>
        {err ? <p className="notice notice--bad">{err}</p> : null}
      </form>
    </Dialog>
  );
}

function ImportDialog({ onClose, onImported }: { onClose: () => void; onImported: (id: string) => void }) {
  const [title, setTitle] = useState('');
  const [genre, setGenre] = useState(GENRES[0]);
  const [files, setFiles] = useState<{ name: string; content: string }[]>([]);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const pick = async (list: FileList | null) => {
    if (!list) return;
    const out: { name: string; content: string }[] = [];
    for (const f of Array.from(list)) {
      if (!/\.(txt|md)$/i.test(f.name)) continue;
      if (f.size > 8 * 1024 * 1024) {
        setErr(`${f.name} 超过 8MB，请拆分后导入`);
        continue;
      }
      out.push({ name: f.name.replace(/\.txt$/i, '.md'), content: await f.text() });
    }
    out.sort((a, b) => a.name.localeCompare(b.name, 'zh-Hans-CN', { numeric: true }));
    setFiles(out);
    if (!title && out[0]) setTitle(out.length === 1 ? out[0].name.replace(/\.(md|txt)$/i, '') : '导入作品');
  };
  const submit = async () => {
    if (!title.trim() || files.length === 0) return setErr('请选择文件并填写书名');
    const names = new Set<string>();
    for (const f of files) {
      if (names.has(f.name)) return setErr(`文件名重复：${f.name}（导入会覆盖同名文件，请先改名）`);
      names.add(f.name);
    }
    setBusy(true);
    setErr(null);
    try {
      const from = location.hash;
      const r = await api.importBook(title.trim(), genre, files);
      toast.ok(`已导入 ${files.length} 个文件到《${title.trim()}》的正文`);
      if (location.hash === from) onImported(r.bookId); // 导入期间作者已去别处：不强行打开
    } catch (e) {
      setErr(errorText(e));
      setBusy(false);
    }
  };
  return (
    <Dialog
      title="导入书稿"
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            取消
          </button>
          <button className="btn btn--primary" onClick={() => void submit()} disabled={busy || files.length === 0}>
            {busy ? <Icon name="refresh" size={16} className="spin" /> : null}
            导入 {files.length ? `${files.length} 个文件` : ''}
          </button>
        </>
      }
    >
      <label className="dropzone">
        <Icon name="upload" size={22} />
        <span>选择 .txt / .md 文件（可多选，按文件名排序导入「正文」）</span>
        <input type="file" accept=".txt,.md,text/plain,text/markdown" multiple onChange={(e) => void pick(e.target.files)} className="visually-hidden" />
      </label>
      {files.length ? (
        <ul className="file-list">
          {files.slice(0, 50).map((f) => (
            <li key={f.name} className="row row--between small">
              <span className="ellipsis">{f.name}</span>
              <span className="faint nowrap">{f.content.length.toLocaleString()} 字符</span>
            </li>
          ))}
          {files.length > 50 ? <li className="small faint">…另有 {files.length - 50} 个文件</li> : null}
        </ul>
      ) : null}
      <div className="row row--wrap">
        <label className="field grow">
          <span className="label">书名</span>
          <input className="input" value={title} onChange={(e) => setTitle(e.target.value)} />
        </label>
        <label className="field">
          <span className="label">题材</span>
          <select className="select" value={genre} onChange={(e) => setGenre(e.target.value)}>
            {GENRES.map((g) => (
              <option key={g}>{g}</option>
            ))}
          </select>
        </label>
      </div>
      {err ? <p className="notice notice--bad">{err}</p> : null}
    </Dialog>
  );
}

function BookTrashDialog({ onClose, onRestored }: { onClose: () => void; onRestored: () => void }) {
  const [rows, setRows] = useState<Book[] | null>(null);
  useEffect(() => {
    api.listBookTrash().then(setRows).catch((e) => toast.bad(errorText(e)));
  }, []);
  return (
    <Dialog title="作品回收站" onClose={onClose}>
      {!rows ? (
        <Spinner label="读取中" />
      ) : rows.length === 0 ? (
        <p className="empty">回收站是空的</p>
      ) : (
        <ul className="file-list">
          {rows.map((b) => (
            <li key={b.id} className="row row--between">
              <span className="ellipsis">《{b.title}》</span>
              <button
                className="btn btn--sm"
                onClick={async () => {
                  try {
                    await api.restoreBook(b.id);
                    toast.ok(`已恢复《${b.title}》`);
                    setRows(rows.filter((x) => x.id !== b.id));
                    onRestored();
                  } catch (e) {
                    toast.bad(errorText(e));
                  }
                }}
              >
                恢复
              </button>
            </li>
          ))}
        </ul>
      )}
    </Dialog>
  );
}
