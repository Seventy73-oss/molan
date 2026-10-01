import { useCallback, useEffect, useRef, useState } from 'react';
import { usePrefs } from '../../app/prefs';
import { ConfirmDialog } from '../../components/Dialog';
import { DiffView } from '../../components/DiffView';
import { Icon } from '../../components/Icon';
import { Markdown } from '../../components/Markdown';
import { Spinner } from '../../components/Status';
import { toast } from '../../components/Toasts';
import { api } from '../../lib/api';
import type { TaskId } from '../../lib/contracts';
import { errorText, newRequestId } from '../../lib/ipc';
import { chapterOf, shortHash, timeAgo, wordCount } from '../../lib/text';
import { useWs } from '../../state/workspace';
import { VersionsDrawer } from './VersionsDrawer';

export function SaveIndicator() {
  const doc = useWs((s) => s.doc);
  const save = useWs((s) => s.saveDoc);
  if (!doc) return null;
  const map = {
    clean: { tone: 'neutral', icon: 'check', text: doc.server?.exists ? '已同步' : '新文档' },
    dirty: { tone: 'pending', icon: 'edit', text: '未保存' },
    saving: { tone: 'info', icon: 'refresh', text: '保存中' },
    saved: { tone: 'ok', icon: 'check', text: '已保存' },
    conflict: { tone: 'bad', icon: 'diff', text: '有冲突' },
    failed: { tone: 'bad', icon: 'alert', text: '保存失败' },
  } as const;
  const m = map[doc.save];
  return (
    <span className="row" style={{ gap: 4 }}>
      <span className={`status status--${m.tone}`} role="status" aria-live="polite" title={doc.error ?? undefined}>
        <Icon name={m.icon} size={13} className={doc.save === 'saving' ? 'spin' : undefined} />
        {m.text}
      </span>
      {doc.save === 'failed' && !doc.readOnly ? (
        <button className="btn btn--sm btn--ghost" onClick={() => void save({ force: true })}>
          重试
        </button>
      ) : null}
    </span>
  );
}

export function Editor() {
  const { doc, bookId, setText, saveDoc, tree } = useWs();
  const prefs = usePrefs();
  const ta = useRef<HTMLTextAreaElement>(null);
  const composing = useRef(false);
  const [preview, setPreview] = useState(false);
  const [sel, setSel] = useState<{ start: number; end: number } | null>(null);
  const [versions, setVersions] = useState(false);
  const [confirmForce, setConfirmForce] = useState(false);

  // Ctrl/Cmd+S：输入法组字中不触发
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's' && !e.isComposing) {
        e.preventDefault();
        void saveDoc();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [saveDoc]);

  // 作者编辑的自动保存（防抖）；AI 产物从不自动写入正式文档
  useEffect(() => {
    if (!prefs.autosave || !doc || doc.save !== 'dirty' || doc.readOnly) return;
    const t = setTimeout(() => {
      if (!composing.current) void saveDoc();
    }, prefs.autosaveDelay);
    return () => clearTimeout(t);
  }, [doc?.text, doc?.save, prefs.autosave, prefs.autosaveDelay, saveDoc, doc]);

  const syncSel = useCallback(() => {
    const el = ta.current;
    if (!el) return;
    setSel(el.selectionStart !== el.selectionEnd ? { start: el.selectionStart, end: el.selectionEnd } : null);
  }, []);

  if (!bookId) return null;
  if (!doc) return <EditorEmpty />;
  if (doc.loading) {
    return (
      <div className="editor-empty">
        <Spinner label="打开文档" />
      </div>
    );
  }

  const askSelection = async (task: TaskId) => {
    if (!sel) return;
    let base = useWs.getState().doc;
    if (base?.save === 'dirty') {
      const r = await saveDoc();
      if (!r || (r.commit !== 'committed' && r.commit !== 'noop')) {
        toast.bad('选区所在文档尚未保存成功，无法发起选区任务');
        return;
      }
      base = useWs.getState().doc;
    }
    if (!base?.baseHash) return toast.bad('文档没有可用的基线版本');
    const text = base.text.slice(sel.start, sel.end);
    useWs.getState().setComposer({
      task,
      target: { group: base.group, name: base.name, baseHash: base.baseHash, start: sel.start, end: sel.end, selectionText: text, label: `选区：${base.name}（${wordCount(text)} 字）` },
    });
    useWs.getState().setPanel('assistant');
  };

  const askDocument = (task: TaskId) => {
    if (doc.save === 'dirty') return toast.info('请先保存文档，再对整篇发起任务');
    const ch = doc.group === '正文' || doc.group === '细纲' ? chapterOf(doc.name) : null;
    useWs.getState().setComposer({
      task,
      target: { group: doc.group, name: doc.name, baseHash: doc.baseHash ?? undefined, ch: ch ?? undefined, label: `文档：${doc.name}` },
    });
    useWs.getState().setPanel('assistant');
  };

  const copySave = async () => {
    const name = doc.name.replace(/(\.\w+)?$/, `（副本 ${new Date().toLocaleTimeString('zh-CN', { hour12: false }).replace(/:/g, '')}）.md`);
    try {
      const r = await api.writeDoc({ bookId, group: doc.group, name, op: 'create', content: doc.text, idempotencyKey: newRequestId('copy') });
      if (r.commit !== 'committed') throw new Error(r.error?.message ?? '另存失败');
      toast.ok(`已另存为 ${doc.group}/${name}`);
      await useWs.getState().refreshTree();
    } catch (e) {
      toast.bad(errorText(e));
    }
  };

  const words = wordCount(doc.text);
  const selWords = sel ? wordCount(doc.text.slice(sel.start, sel.end)) : 0;
  void tree;

  return (
    <div className="editor">
      <div className="editor__bar">
        <div className="row grow" style={{ minWidth: 0 }}>
          <span className="editor__doc ellipsis serif">{doc.name.replace(/\.md$/, '')}</span>
          {doc.server?.locked ? <span className="tag"><Icon name="lock" size={12} />已锁定</span> : null}
          {doc.server?.aiOff ? <span className="tag"><Icon name="eyeOff" size={12} />AI 不可见</span> : null}
        </div>
        <div className="row editor__tools">
          <button className={`btn btn--sm btn--ghost${preview ? ' is-on' : ''}`} onClick={() => setPreview((p) => !p)} aria-pressed={preview}>
            <Icon name={preview ? 'edit' : 'book'} size={15} />
            {preview ? '编辑' : '预览'}
          </button>
          <button className="btn btn--sm btn--ghost" onClick={() => setVersions(true)}>
            <Icon name="history" size={15} />
            版本
          </button>
          {!doc.readOnly ? (
            <button className="btn btn--sm" onClick={() => void saveDoc()} disabled={doc.save !== 'dirty'}>
              <Icon name="save" size={15} />
              保存
              <span className="kbd">Ctrl S</span>
            </button>
          ) : null}
        </div>
      </div>

      {doc.readOnly && doc.group === '正文待审' ? (
        <div className="notice notice--pending editor__notice">
          <Icon name="inbox" size={16} />
          <span className="grow">这是 AI 草稿（正文待审），由章节服务管理，不能直接编辑。请在「待审」面板阅读并定稿或驳回。</span>
          <button className="btn btn--sm" onClick={() => useWs.getState().setPanel('pending')}>
            打开待审
          </button>
        </div>
      ) : null}

      {doc.draft ? (
        <div className={`notice ${doc.draft.kind === 'diverged' ? 'notice--bad' : 'notice--pending'} editor__notice`} role="alert">
          <Icon name="history" size={16} />
          <span className="grow">
            {doc.draft.kind === 'restorable'
              ? `发现本机未保存的草稿（${timeAgo(doc.draft.draft.savedAt)}），基于当前服务器版本。`
              : `发现本机草稿（${timeAgo(doc.draft.draft.savedAt)}），但服务器版本已变化，恢复后需要比较合并。`}
          </span>
          <button className="btn btn--sm btn--primary" onClick={() => useWs.getState().restoreDraft()}>
            {doc.draft.kind === 'restorable' ? '恢复草稿' : '恢复并比较'}
          </button>
          <button className="btn btn--sm" onClick={() => useWs.getState().discardDraft()}>
            丢弃
          </button>
        </div>
      ) : null}

      {doc.save === 'conflict' && doc.conflict ? (
        <div className="conflict" role="alert">
          <div className="row row--wrap">
            <Icon name="diff" />
            <strong className="grow">文档在你编辑期间被修改（另一个窗口或 AI 交付），你的修改尚未写入</strong>
          </div>
          <DiffView before={doc.conflict.serverText} after={doc.text} beforeLabel="服务器当前版本" afterLabel="你的版本" />
          <div className="row row--wrap">
            <button className="btn btn--sm" onClick={() => void copySave()}>
              另存我的版本为副本
            </button>
            <button className="btn btn--sm" onClick={() => void useWs.getState().reloadDoc()}>
              放弃我的修改，采用服务器版本
            </button>
            <button className="btn btn--sm btn--danger" onClick={() => setConfirmForce(true)}>
              用我的版本覆盖
            </button>
          </div>
        </div>
      ) : null}

      {doc.error && doc.save !== 'conflict' ? (
        <div className={`notice ${doc.save === 'failed' ? 'notice--bad' : 'notice--pending'} editor__notice`}>
          <Icon name="alert" size={16} />
          <span className="grow break">{doc.error}</span>
        </div>
      ) : null}

      <div className="editor__scroll">
        {preview ? (
          <article className="paper paper--preview">
            <Markdown text={doc.text || '（空文档）'} className="manuscript-preview" />
          </article>
        ) : (
          <div className="paper">
            <textarea
              ref={ta}
              className="manuscript"
              value={doc.text}
              readOnly={doc.readOnly}
              spellCheck={false}
              aria-label={`正在编辑 ${doc.group}/${doc.name}`}
              placeholder={doc.readOnly ? '' : '从这里开始写…'}
              onChange={(e) => setText(e.target.value)}
              onCompositionStart={() => (composing.current = true)}
              onCompositionEnd={() => (composing.current = false)}
              onSelect={syncSel}
              onKeyUp={syncSel}
              onMouseUp={syncSel}
              onBlur={() => {
                if (!composing.current && prefs.autosave && useWs.getState().doc?.save === 'dirty') void saveDoc();
              }}
            />
          </div>
        )}
      </div>

      <div className="editor__foot">
        {sel && !doc.readOnly ? (
          <div className="selbar" role="toolbar" aria-label="选区操作">
            <span className="small muted nowrap">已选 {selWords} 字</span>
            <button className="btn btn--sm" onClick={() => void askSelection('revise')}>
              <Icon name="edit" size={14} />
              改写选区
            </button>
            <button className="btn btn--sm" onClick={() => void askSelection('humanize')}>
              <Icon name="feather" size={14} />
              去AI味
            </button>
            <button className="btn btn--sm" onClick={() => void askSelection('review')}>
              <Icon name="search" size={14} />
              审读选区
            </button>
          </div>
        ) : (
          <div className="row row--wrap small faint">
            <span>{words.toLocaleString()} 字</span>
            {doc.server?.revision ? <span>· 第 {doc.server.revision} 版</span> : null}
            {doc.baseHash ? <span className="mono">· {shortHash(doc.baseHash)}</span> : null}
            {doc.lastReceipt?.commit === 'committed' ? <span>· 写入回执 {shortHash(doc.lastReceipt.writeId)}</span> : null}
            {!doc.readOnly ? (
              <span className="row editor__doc-actions">
                <button className="btn btn--sm btn--ghost" onClick={() => askDocument('revise')}>
                  改写全文
                </button>
                <button className="btn btn--sm btn--ghost" onClick={() => askDocument('review')}>
                  审稿
                </button>
              </span>
            ) : null}
          </div>
        )}
      </div>

      {versions ? <VersionsDrawer onClose={() => setVersions(false)} /> : null}
      {confirmForce ? (
        <ConfirmDialog
          title="用我的版本覆盖？"
          body="服务器上的新内容会被你的版本替换（旧内容仍保留在版本历史中）。确认你已经看过上面的差异。"
          confirmLabel="覆盖保存"
          danger
          onCancel={() => setConfirmForce(false)}
          onConfirm={async () => {
            setConfirmForce(false);
            await saveDoc({ force: true });
          }}
        />
      ) : null}
    </div>
  );
}

function EditorEmpty() {
  const { tree, openDoc, setPanel } = useWs();
  const chapters = tree.find((g) => g.dir === '正文')?.files ?? [];
  const latest = [...chapters].sort((a, b) => b.name.localeCompare(a.name, 'zh-Hans-CN', { numeric: true }))[0];
  return (
    <div className="editor-empty">
      <div className="empty">
        <Icon name="feather" size={30} />
        <p className="empty__title">从目录选择文档开始写作</p>
        <div className="row row--wrap" style={{ justifyContent: 'center' }}>
          {latest ? (
            <button className="btn btn--primary" onClick={() => void openDoc('正文', latest.name)}>
              继续 {latest.name.replace(/\.md$/, '')}
            </button>
          ) : null}
          <button className="btn" onClick={() => setPanel('assistant')}>
            <Icon name="spark" size={16} />
            和创作助手聊聊
          </button>
          <button className="btn" onClick={() => setPanel('pipeline')}>
            <Icon name="route" size={16} />
            查看生产线
          </button>
        </div>
      </div>
    </div>
  );
}
