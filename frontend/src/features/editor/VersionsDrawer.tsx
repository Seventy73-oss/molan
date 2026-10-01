import { useEffect, useState } from 'react';
import { ConfirmDialog } from '../../components/Dialog';
import { DiffView } from '../../components/DiffView';
import { Icon } from '../../components/Icon';
import { Spinner } from '../../components/Status';
import { toast } from '../../components/Toasts';
import { api } from '../../lib/api';
import type { WriteReceipt } from '../../lib/contracts';
import { errorText, invoke } from '../../lib/ipc';
import { shortHash } from '../../lib/text';
import { useWs } from '../../state/workspace';

/** 版本历史：快照列表（服务端写前自动保存）+ 与当前文本的真实差异 + 恢复（恢复本身也会留快照）。 */
export function VersionsDrawer({ onClose }: { onClose: () => void }) {
  const { doc, bookId } = useWs();
  const [list, setList] = useState<{ ts: number; size: number }[] | null>(null);
  const [receipts, setReceipts] = useState<WriteReceipt[]>([]);
  const [picked, setPicked] = useState<{ ts: number; content: string } | null>(null);
  const [restoring, setRestoring] = useState(false);
  useEffect(() => {
    if (!doc || !bookId) return;
    api.listVersions(bookId, doc.group, doc.name).then(setList).catch((e) => toast.bad(errorText(e)));
    api.docHistory(bookId, doc.group, doc.name).then(setReceipts).catch(() => setReceipts([]));
  }, [doc?.group, doc?.name, bookId, doc]);
  if (!doc || !bookId) return null;
  return (
    <div className="drawer" role="dialog" aria-modal="true" aria-label="版本历史">
      <div className="panel-head">
        <strong className="grow">版本历史 · {doc.name}</strong>
        <button className="icon-btn" onClick={onClose} aria-label="关闭">
          <Icon name="x" />
        </button>
      </div>
      <div className="panel-body versions">
        <p className="hint" style={{ padding: '0 16px' }}>每次覆盖写入前，服务端都会自动保存旧版本快照。</p>
        {!list ? (
          <Spinner label="读取版本" />
        ) : list.length === 0 ? (
          <p className="empty">还没有历史版本</p>
        ) : (
          <ul className="versions__list">
            {list.map((v) => (
              <li key={v.ts}>
                <button
                  className={`versions__item${picked?.ts === v.ts ? ' versions__item--active' : ''}`}
                  onClick={async () => {
                    try {
                      const r = await api.readVersion(bookId, doc.group, doc.name, v.ts);
                      setPicked({ ts: v.ts, content: r.content });
                    } catch (e) {
                      toast.bad(errorText(e));
                    }
                  }}
                >
                  <Icon name="clock" size={15} />
                  <span className="grow">{new Date(v.ts).toLocaleString('zh-CN', { hour12: false })}</span>
                  <span className="faint small">{v.size} 字</span>
                </button>
              </li>
            ))}
          </ul>
        )}
        {picked ? (
          <div className="stack" style={{ padding: 16 }}>
            <DiffView before={picked.content} after={doc.text} beforeLabel="该版本" afterLabel="当前编辑" />
            <button className="btn btn--danger" onClick={() => setRestoring(true)} disabled={doc.save === 'dirty'}>
              恢复为该版本{doc.save === 'dirty' ? '（请先保存或放弃当前修改）' : ''}
            </button>
          </div>
        ) : null}
        {receipts.length ? (
          <div className="stack stack--tight" style={{ padding: 16 }}>
            <span className="section-title">写入回执</span>
            {receipts.slice(0, 20).map((r) => (
              <div key={r.writeId} className="receipt-row small">
                <span className="tag">{r.op}</span>
                <span className="grow muted">{r.actor === 'ai' ? 'AI 产物交付' : '作者保存'} · {new Date(r.ts).toLocaleString('zh-CN', { hour12: false })}</span>
                <span className="mono faint">{shortHash(r.afterHash)}</span>
              </div>
            ))}
          </div>
        ) : null}
      </div>
      {restoring && picked ? (
        <ConfirmDialog
          title="恢复到该版本？"
          body="当前内容会先自动保存为一个版本快照，然后替换为所选版本。"
          confirmLabel="恢复"
          onCancel={() => setRestoring(false)}
          onConfirm={async () => {
            try {
              const r = await invoke<{ ok: boolean; err?: string }>('restore_version', { bookId, group: doc.group, name: doc.name, ts: picked.ts });
              if (r && r.ok === false) throw new Error(r.err ?? '恢复失败');
              await useWs.getState().reloadDoc();
              toast.ok('已恢复到所选版本');
              onClose();
            } catch (e) {
              toast.bad(errorText(e));
            } finally {
              setRestoring(false);
            }
          }}
        />
      ) : null}
    </div>
  );
}
