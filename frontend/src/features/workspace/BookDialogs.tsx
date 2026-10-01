import { useEffect, useState } from 'react';
import { Dialog } from '../../components/Dialog';
import { Spinner } from '../../components/Status';
import { toast } from '../../components/Toasts';
import { api } from '../../lib/api';
import { errorText, invoke } from '../../lib/ipc';
import { timeAgo } from '../../lib/text';
import { useWs } from '../../state/workspace';

/** 文件回收站：按服务端返回的真实 trashId 恢复；同名异内容冲突如实报告，不覆盖。 */
export function FileTrashDialog({ onClose }: { onClose: () => void }) {
  const { bookId, refreshTree } = useWs();
  const [rows, setRows] = useState<{ id: string; group: string; name: string; deletedAt: number }[] | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  useEffect(() => {
    if (bookId) api.listFileTrash(bookId).then(setRows).catch((e) => toast.bad(errorText(e)));
  }, [bookId]);
  if (!bookId) return null;
  return (
    <Dialog title="文件回收站" onClose={onClose}>
      {!rows ? (
        <Spinner label="读取回收站" />
      ) : rows.length === 0 ? (
        <p className="empty">回收站是空的</p>
      ) : (
        <ul className="file-list">
          {rows.map((r) => (
            <li key={r.id} className="row row--between">
              <span className="stack stack--tight" style={{ gap: 0, minWidth: 0 }}>
                <span className="ellipsis">
                  {r.group}/{r.name}
                </span>
                <span className="small faint">{timeAgo(r.deletedAt)}删除</span>
              </span>
              <button
                className="btn btn--sm"
                disabled={busy === r.id}
                onClick={async () => {
                  setBusy(r.id);
                  try {
                    const out = await api.restoreFile(bookId, r.id);
                    if (out && out.ok === false) throw new Error(out.err ?? '恢复失败');
                    toast.ok(`已恢复 ${r.group}/${r.name}`);
                    setRows(rows.filter((x) => x.id !== r.id));
                    await refreshTree();
                  } catch (e) {
                    toast.bad(errorText(e));
                  } finally {
                    setBusy(null);
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

const GENRES = ['玄幻', '仙侠', '都市', '言情', '历史', '武侠', '科幻', '悬疑灵异', '游戏', '军事', '其他'];
const STATUSES = ['构思中', '连载中', '已完结', '暂停'];

export function BookInfoDialog({ onClose }: { onClose: () => void }) {
  const { book, bookId } = useWs();
  const [title, setTitle] = useState(book?.title ?? '');
  const [genre, setGenre] = useState(book?.genre ?? '');
  const [pov, setPov] = useState(book?.pov ?? '');
  const [status, setStatus] = useState(book?.status ?? '构思中');
  const [busy, setBusy] = useState(false);
  if (!bookId) return null;
  return (
    <Dialog
      title="作品信息"
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            取消
          </button>
          <button
            className="btn btn--primary"
            disabled={busy || !title.trim()}
            onClick={async () => {
              setBusy(true);
              try {
                const b = await invoke<Record<string, unknown>>('update_book_meta', { bookId, title: title.trim(), genre, pov, status });
                useWs.setState({ book: { ...(useWs.getState().book ?? ({} as never)), ...(b as object), title: String(b.title ?? title) } as never });
                toast.ok('作品信息已更新（题材与视角会影响之后的生成）');
                onClose();
              } catch (e) {
                toast.bad(errorText(e));
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
        <span className="label">书名</span>
        <input className="input" value={title} onChange={(e) => setTitle(e.target.value)} data-autofocus />
      </label>
      <div className="row row--wrap">
        <label className="field grow">
          <span className="label">题材</span>
          <select className="select" value={genre} onChange={(e) => setGenre(e.target.value)}>
            {[genre, ...GENRES.filter((g) => g !== genre)].filter(Boolean).map((g) => (
              <option key={g}>{g}</option>
            ))}
          </select>
        </label>
        <label className="field grow">
          <span className="label">视角</span>
          <input className="input" value={pov} onChange={(e) => setPov(e.target.value)} />
        </label>
        <label className="field grow">
          <span className="label">状态</span>
          <select className="select" value={status} onChange={(e) => setStatus(e.target.value)}>
            {[status, ...STATUSES.filter((x) => x !== status)].map((g) => (
              <option key={g}>{g}</option>
            ))}
          </select>
        </label>
      </div>
    </Dialog>
  );
}
