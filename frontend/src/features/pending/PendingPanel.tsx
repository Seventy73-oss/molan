import { useCallback, useEffect, useState } from 'react';
import { ConfirmDialog, Dialog } from '../../components/Dialog';
import { DiffView } from '../../components/DiffView';
import { Icon } from '../../components/Icon';
import { Markdown } from '../../components/Markdown';
import { Pill, Spinner } from '../../components/Status';
import { toast } from '../../components/Toasts';
import { api, type PendingChapter, type Proposal, type ReviewState } from '../../lib/api';
import { errorText } from '../../lib/ipc';
import { shortHash, timeAgo } from '../../lib/text';
import { useWs } from '../../state/workspace';

/**
 * 待审与提案：阅读 → 比较 → 定稿/驳回、接受/拒绝。
 * 定稿绑定作者阅读时的 contentHash：期间稿件被改则拒绝定稿（需重新阅读）。
 */
export function PendingPanel() {
  const { bookId, refreshPendingCount, refreshTree } = useWs();
  const [pending, setPending] = useState<PendingChapter[] | null>(null);
  const [proposals, setProposals] = useState<Proposal[] | null>(null);
  const [reading, setReading] = useState<PendingChapter | null>(null);
  const [proposal, setProposal] = useState<Proposal | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    if (!bookId) return;
    try {
      const [p, q] = await Promise.all([api.pending(bookId), api.proposals(bookId).catch(() => [])]);
      setPending(p);
      setProposals(q);
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, [bookId]);

  useEffect(() => {
    void load();
  }, [load]);

  const done = async () => {
    await load();
    void refreshPendingCount();
    void refreshTree();
  };

  return (
    <div className="panel-body pending">
      <div className="row row--between" style={{ padding: '10px 14px 4px' }}>
        <span className="section-title">正文待审</span>
        <button className="icon-btn icon-btn--sm" onClick={() => void load()} aria-label="刷新待审">
          <Icon name="refresh" size={15} />
        </button>
      </div>
      {error ? <p className="notice notice--bad" style={{ margin: 12 }}>{error}</p> : null}
      {!pending ? (
        <Spinner label="读取待审" />
      ) : pending.length === 0 ? (
        <p className="empty small">没有待审章节。AI 起草的正文会先进入这里，定稿后才进入「正文」。</p>
      ) : (
        <ul className="pend-list">
          {pending.map((p) => (
            <li key={p.ch} className="pend-item">
              <div className="row">
                <Icon name="feather" size={16} />
                <strong className="grow">第 {p.ch} 章</strong>
                <span className="small faint">{(p.chars ?? p.words ?? 0).toLocaleString()} 字</span>
              </div>
              {p.dependencyStatus === 'stale' ? (
                <Pill tone="bad" icon="alert">前一章已变更，定稿前需重审</Pill>
              ) : null}
              <ReviewBadge review={p.review} />
              {p.outline ? <p className="small muted pend-outline">细纲：{p.outline}</p> : null}
              <div className="row row--wrap">
                <button className="btn btn--sm btn--primary" onClick={() => setReading(p)}>
                  阅读并处理
                </button>
                <button className="btn btn--sm" onClick={() => void useWs.getState().openDoc('正文待审', p.name)}>
                  在编辑区打开
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}

      <div className="row" style={{ padding: '16px 14px 4px' }}>
        <span className="section-title">修改提案</span>
      </div>
      {!proposals ? null : proposals.length === 0 ? (
        <p className="empty small">没有待处理的提案。</p>
      ) : (
        <ul className="pend-list">
          {proposals.map((p) => (
            <li key={p.id} className="pend-item">
              <div className="row">
                <Icon name="diff" size={16} />
                <strong className="grow ellipsis">
                  {p.groupName}/{p.fileName}
                </strong>
                <span className="small faint">{timeAgo(p.createdAt)}</span>
              </div>
              <p className="small muted">{p.summary}</p>
              {p.error ? <p className="small notice notice--bad">{p.error}</p> : null}
              <button className="btn btn--sm" onClick={() => setProposal(p)}>
                查看差异
              </button>
            </li>
          ))}
        </ul>
      )}

      {reading && bookId ? <PendingReader bookId={bookId} p={reading} onClose={() => setReading(null)} onDone={() => void done()} /> : null}
      {proposal && bookId ? <ProposalDialog bookId={bookId} p={proposal} onClose={() => setProposal(null)} onDone={() => void done()} /> : null}
    </div>
  );
}

function PendingReader({ bookId, p, onClose, onDone }: { bookId: string; p: PendingChapter; onClose: () => void; onDone: () => void }) {
  const [doc, setDoc] = useState<{ content: string; hash: string | null } | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmReject, setConfirmReject] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [review, setReview] = useState<ReviewState | undefined>(p.review);
  useEffect(() => {
    api.readDoc(bookId, '正文待审', p.name).then((d) => setDoc({ content: d.content, hash: d.hash })).catch((e) => setError(errorText(e)));
  }, [bookId, p.name]);
  const stale = p.contentHash && doc?.hash && p.contentHash !== doc.hash;
  const approve = async () => {
    if (!doc?.hash) return;
    setBusy(true);
    setError(null);
    try {
      const r = await api.approve(bookId, p.name, doc.hash);
      toast.ok(r.alreadyApproved ? `第${p.ch}章此前已定稿` : `定稿完成：正文/${r.finalName}，记忆更新排队中`);
      onDone();
      onClose();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={`第 ${p.ch} 章 · 待审稿`}
      wide
      onClose={onClose}
      footer={
        <>
          <span className="small faint grow">审阅版本 {shortHash(doc?.hash)}</span>
          <button className="btn btn--danger" onClick={() => setConfirmReject(true)} disabled={busy}>
            驳回
          </button>
          <button className="btn btn--primary" onClick={() => void approve()} disabled={busy || !doc || !!stale}>
            {busy ? <Icon name="refresh" size={15} className="spin" /> : <Icon name="check" size={15} />}
            定稿（按此版本）
          </button>
        </>
      }
    >
      {stale ? <p className="notice notice--pending">打开后稿件已变化，请关闭重新阅读后再定稿。</p> : null}
      {error ? <p className="notice notice--bad">{error}</p> : null}
      <ReviewNote
        review={review}
        hash={doc?.hash ?? null}
        onReview={async () => {
          try {
            setReview((await api.reviewPending(bookId, p.ch)).review);
            onDone();
          } catch (e) {
            setError(errorText(e));
          }
        }}
      />
      {!doc ? <Spinner label="读取稿件" /> : <Markdown text={doc.content} className="reading" />}
      {confirmReject ? (
        <ConfirmDialog
          title="驳回这一稿？"
          body="待审稿会移到回收站（可恢复），该章可以重新起草。"
          confirmLabel="驳回"
          danger
          onCancel={() => setConfirmReject(false)}
          onConfirm={async () => {
            try {
              await api.reject(bookId, p.ch, p.name);
              toast.ok('已驳回，稿件在回收站可恢复');
              onDone();
              onClose();
            } catch (e) {
              setError(errorText(e));
            } finally {
              setConfirmReject(false);
            }
          }}
        />
      ) : null}
    </Dialog>
  );
}

/** 审稿结论只对审过的那一版有效：当前稿 hash 与被审 hash 不同即「已失效」。 */
function reviewState(r: ReviewState | undefined, hash: string | null): ReviewState['state'] {
  if (!r || r.state === 'none') return 'none';
  if (hash && r.bodyHash && hash !== r.bodyHash) return 'stale';
  return r.state;
}

function ReviewBadge({ review }: { review?: ReviewState }) {
  const s = reviewState(review, null);
  if (s === 'none') return <Pill tone="neutral" icon="info">尚未审稿</Pill>;
  if (s === 'stale') return <Pill tone="pending" icon="alert">审稿结论已失效</Pill>;
  return review?.ok ? (
    <Pill tone="ok" icon="check">审稿通过</Pill>
  ) : (
    <Pill tone="bad" icon="alert">审稿提出 {review?.issues?.length ?? 0} 个问题</Pill>
  );
}

function ReviewNote({ review, hash, onReview }: { review?: ReviewState; hash: string | null; onReview: () => Promise<void> }) {
  const [busy, setBusy] = useState(false);
  const s = reviewState(review, hash);
  const action =
    s === 'current' ? null : (
      <button
        className="btn btn--sm"
        disabled={busy}
        onClick={async () => {
          setBusy(true);
          await onReview();
          setBusy(false);
        }}
      >
        {busy ? <Icon name="refresh" size={14} className="spin" /> : <Icon name="search" size={14} />}
        审稿当前版本
      </button>
    );
  if (s === 'none')
    return (
      <div className="notice" role="status">
        <Icon name="info" size={16} />
        <span className="grow">这一稿尚未经过审稿（或审稿未完成）。可以自行通读，或让审稿模型检查当前版本。</span>
        {action}
      </div>
    );
  const issues = review?.issues ?? [];
  return (
    <div className={`notice ${s === 'stale' ? 'notice--pending' : review?.ok ? 'notice--ok' : 'notice--bad'}`} role="status">
      <Icon name={s === 'stale' || !review?.ok ? 'alert' : 'check'} size={16} />
      <div className="stack stack--tight grow">
        <span>
          {s === 'stale'
            ? '审稿结论已失效：审稿后正文被修改（去AI味、重写或手动编辑），以下结论仅供参考。'
            : review?.ok
              ? '审稿通过（针对当前版本）。'
              : `审稿提出 ${issues.length} 个问题（针对当前版本）：`}
        </span>
        {issues.length ? (
          <ul className="small">
            {issues.map((i) => (
              <li key={i}>{i}</li>
            ))}
          </ul>
        ) : null}
        {review?.note && !review.ok ? <span className="small muted">{review.note}</span> : null}
      </div>
      {action}
    </div>
  );
}

function ProposalDialog({ bookId, p, onClose, onDone }: { bookId: string; p: Proposal; onClose: () => void; onDone: () => void }) {
  const [full, setFull] = useState<Proposal | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api.proposal(bookId, p.id).then(setFull).catch((e) => setError(errorText(e)));
  }, [bookId, p.id]);
  const act = async (accept: boolean) => {
    setBusy(true);
    setError(null);
    try {
      if (accept) {
        const r = await api.acceptProposal(bookId, p.id);
        toast.ok(r.indexError ? `已写入 ${p.groupName}/${p.fileName}，但索引登记失败（文件完好）` : `已写入 ${p.groupName}/${p.fileName}`);
      } else {
        await api.rejectProposal(bookId, p.id, '作者拒绝');
        toast.ok('已拒绝提案');
      }
      onDone();
      onClose();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={`提案：${p.groupName}/${p.fileName}`}
      wide
      onClose={onClose}
      footer={
        <>
          <button className="btn btn--danger" onClick={() => void act(false)} disabled={busy}>
            拒绝
          </button>
          <button className="btn btn--primary" onClick={() => void act(true)} disabled={busy || !full}>
            接受并写入
          </button>
        </>
      }
    >
      <p className="muted">{p.summary}</p>
      {error ? <p className="notice notice--bad">{error}</p> : null}
      {!full ? <Spinner /> : <DiffView before={full.baseContent ?? ''} after={full.proposedContent ?? ''} beforeLabel="提案基线" afterLabel="提议内容" />}
      <p className="hint">接受时以提案生成时的原文为基线做比较交换：文件期间被改过会拒绝写入，不会覆盖新内容。</p>
    </Dialog>
  );
}
