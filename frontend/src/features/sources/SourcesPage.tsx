import { useEffect, useState } from 'react';
import { Icon } from '../../components/Icon';
import { Spinner } from '../../components/Status';
import { toast } from '../../components/Toasts';
import { api } from '../../lib/api';
import type { Book } from '../../lib/contracts';
import { errorText, newRequestId } from '../../lib/ipc';

interface Hit {
  title: string;
  author?: string;
  url: string;
}
interface Chapter {
  title: string;
  name?: string;
  url: string;
}

/** 书源：搜索 → 目录 → 抓取所选章节 → 作为参考资料存入明确选择的作品（仅新建，不覆盖）。 */
export default function SourcesPage() {
  const [sources, setSources] = useState<{ id: string; name: string }[] | null>(null);
  const [sourceId, setSourceId] = useState('');
  const [kw, setKw] = useState('');
  const [hits, setHits] = useState<Hit[] | null>(null);
  const [book, setBook] = useState<Hit | null>(null);
  const [chapters, setChapters] = useState<Chapter[] | null>(null);
  const [picked, setPicked] = useState<Set<number>>(new Set());
  const [busy, setBusy] = useState<string | null>(null);
  const [progress, setProgress] = useState('');
  const [books, setBooks] = useState<Book[]>([]);
  const [targetBook, setTargetBook] = useState('');

  useEffect(() => {
    api.bookSources().then((s) => {
      setSources(s);
      if (s[0]) setSourceId(s[0].id);
    }, (e) => toast.bad(errorText(e)));
    api.listBooks().then(setBooks).catch(() => undefined);
  }, []);

  const search = async () => {
    if (!kw.trim() || !sourceId) return;
    setBusy('search');
    setHits(null);
    setBook(null);
    setChapters(null);
    try {
      setHits(await api.searchBooks(sourceId, kw.trim()));
    } catch (e) {
      toast.bad(errorText(e));
    } finally {
      setBusy(null);
    }
  };

  const openBook = async (h: Hit) => {
    setBook(h);
    setBusy('catalog');
    setChapters(null);
    setPicked(new Set());
    try {
      const r = await api.bookCatalog(sourceId, h.url);
      setChapters(r.chapters);
    } catch (e) {
      toast.bad(errorText(e));
    } finally {
      setBusy(null);
    }
  };

  const save = async () => {
    if (!book || !chapters || !targetBook) return;
    const list = [...picked].sort((a, b) => a - b).map((i) => chapters[i]);
    if (!list.length) return;
    if (list.length > 20) return toast.bad('每次最多抓取 20 章，请分批');
    setBusy('fetch');
    setProgress('开始抓取');
    try {
      const text = await api.chapterTexts(sourceId, list, (e) => {
        if (e.type === 'step') setProgress(String(e.title ?? ''));
      });
      if (!text || !String(text).trim()) throw new Error('没有抓到正文');
      const first = list[0].title || list[0].name || '';
      const name = `${book.title.replace(/[\\/:*?"<>|]/g, '')}_${first.replace(/[\\/:*?"<>|]/g, '').slice(0, 20)}等${list.length}章.md`;
      const r = await api.writeDoc({ bookId: targetBook, group: '参考', name, op: 'create', content: String(text), idempotencyKey: newRequestId('src') });
      if (r.commit === 'committed') toast.ok(`已存入参考/${name}`);
      else throw new Error(r.error?.message ?? '保存失败');
    } catch (e) {
      toast.bad(errorText(e));
    } finally {
      setBusy(null);
      setProgress('');
    }
  };

  return (
    <div className="page">
      <div className="page__inner">
        <header className="page__head">
          <div>
            <h1 className="page__title">书源</h1>
            <p className="page__sub">抓取公开章节作为拆解参考，存入作品的「参考」分组；不会写入正文。</p>
          </div>
        </header>
        {!sources ? (
          <Spinner label="读取书源" />
        ) : sources.length === 0 ? (
          <div className="notice">没有可用书源（data/book_sources.json 未配置）。</div>
        ) : (
          <form
            className="row row--wrap"
            onSubmit={(e) => {
              e.preventDefault();
              void search();
            }}
          >
            <select className="select" style={{ width: 'auto' }} value={sourceId} onChange={(e) => setSourceId(e.target.value)} aria-label="书源">
              {sources.map((s) => (
                <option key={s.id} value={s.id}>
                  {s.name}
                </option>
              ))}
            </select>
            <label className="search grow">
              <Icon name="search" size={16} />
              <input className="search__input" value={kw} onChange={(e) => setKw(e.target.value)} placeholder="书名或作者" aria-label="搜索关键词" />
            </label>
            <button className="btn btn--primary" disabled={busy === 'search'}>
              搜索
            </button>
          </form>
        )}
        {busy === 'search' ? <Spinner label="搜索中" /> : null}
        {hits ? (
          hits.length === 0 ? (
            <p className="empty">没有结果</p>
          ) : (
            <ul className="src-hits">
              {hits.map((h) => (
                <li key={h.url}>
                  <button className={`src-hit${book?.url === h.url ? ' is-active' : ''}`} onClick={() => void openBook(h)}>
                    <strong className="ellipsis">{h.title}</strong>
                    <span className="small faint">{h.author}</span>
                  </button>
                </li>
              ))}
            </ul>
          )
        ) : null}
        {busy === 'catalog' ? <Spinner label="读取目录" /> : null}
        {book && chapters ? (
          <section className="stack">
            <div className="row row--between row--wrap">
              <strong>《{book.title}》 · {chapters.length} 章</strong>
              <span className="small muted">已选 {picked.size} 章（每次最多 20 章）</span>
            </div>
            <div className="src-chapters">
              {chapters.slice(0, 500).map((c, i) => (
                <label key={c.url + i} className="checkbox small">
                  <input
                    type="checkbox"
                    checked={picked.has(i)}
                    onChange={(e) => {
                      const n = new Set(picked);
                      if (e.target.checked) n.add(i);
                      else n.delete(i);
                      setPicked(n);
                    }}
                  />
                  <span className="ellipsis">{c.title || c.name}</span>
                </label>
              ))}
            </div>
            <div className="row row--wrap">
              <select className="select" style={{ width: 'auto' }} value={targetBook} onChange={(e) => setTargetBook(e.target.value)} aria-label="存入作品">
                <option value="">存入哪部作品？</option>
                {books.map((b) => (
                  <option key={b.id} value={b.id}>
                    《{b.title}》
                  </option>
                ))}
              </select>
              <button className="btn btn--primary" disabled={!targetBook || picked.size === 0 || busy === 'fetch'} onClick={() => void save()}>
                {busy === 'fetch' ? <Icon name="refresh" size={15} className="spin" /> : <Icon name="download" size={15} />}
                抓取并存入参考
              </button>
              {progress ? <span className="small muted">{progress}</span> : null}
            </div>
          </section>
        ) : null}
      </div>
    </div>
  );
}
