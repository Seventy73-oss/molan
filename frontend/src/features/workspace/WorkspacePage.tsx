import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { usePrefs } from '../../app/prefs';
import { Icon } from '../../components/Icon';
import { Menu } from '../../components/Menu';
import { Pill, Spinner } from '../../components/Status';
import { toast } from '../../components/Toasts';
import { api, downloadBase64 } from '../../lib/api';
import { errorText } from '../../lib/ipc';
import type { AppInfo } from '../../lib/contracts';
import { useWs, type Panel } from '../../state/workspace';
import { AssistantPanel } from '../assistant/AssistantPanel';
import { ContextPanel } from '../assistant/ContextPanel';
import { Editor, SaveIndicator } from '../editor/Editor';
import { rememberBook } from '../library/LibraryPage';
import { PendingPanel } from '../pending/PendingPanel';
import { PipelinePanel } from '../pipeline/PipelinePanel';
import { BookInfoDialog } from './BookDialogs';
import { FileTree } from './FileTree';

const PANELS: { id: Panel; label: string; icon: string }[] = [
  { id: 'assistant', label: '助手', icon: 'spark' },
  { id: 'pending', label: '待审', icon: 'inbox' },
  { id: 'pipeline', label: '生产线', icon: 'route' },
  { id: 'context', label: '上下文', icon: 'layers' },
];

const W_KEY = 'molan.ws.widths';

function loadWidths(): { left: number; right: number } {
  try {
    const v = JSON.parse(localStorage.getItem(W_KEY) ?? '{}') as { left?: number; right?: number };
    return { left: v.left ?? 260, right: v.right ?? 420 };
  } catch {
    return { left: 260, right: 420 };
  }
}

export function WorkspacePage({ bookId, info }: { bookId: string; info: AppInfo | null }) {
  const ws = useWs();
  const [widths, setWidths] = useState(loadWidths);
  const bodyRef = useRef<HTMLDivElement>(null);
  const [dragging, setDragging] = useState<null | 'left' | 'right'>(null);
  const [infoOpen, setInfoOpen] = useState(false);
  void info;

  // 布局阶段切换作品：保证路由已指向新作品时，界面绝不会在旧作品的状态上接受输入（否则快速点击会把消息发到上一部作品）
  useLayoutEffect(() => {
    rememberBook(bookId);
    if (useWs.getState().bookId !== bookId) void useWs.getState().openBook(bookId);
  }, [bookId]);

  // 窄屏默认收起侧栏
  useEffect(() => {
    if (window.innerWidth < 760) useWs.getState().toggle('rightOpen', false);
    if (window.innerWidth < 1080) useWs.getState().toggle('leftOpen', false);
  }, []);

  // 离开页面/刷新前：有未保存编辑时提示（本地草稿也已保存）
  useEffect(() => {
    const onBefore = (e: BeforeUnloadEvent) => {
      const s = useWs.getState().doc?.save;
      if (s === 'dirty' || s === 'saving' || s === 'conflict') {
        e.preventDefault();
        e.returnValue = '';
      }
    };
    window.addEventListener('beforeunload', onBefore);
    return () => window.removeEventListener('beforeunload', onBefore);
  }, []);

  useEffect(() => {
    if (!dragging) return;
    const move = (e: PointerEvent) => {
      const r = bodyRef.current?.getBoundingClientRect();
      if (!r) return;
      setWidths((w) => {
        const next =
          dragging === 'left'
            ? { ...w, left: Math.round(Math.min(420, Math.max(200, e.clientX - r.left))) }
            : { ...w, right: Math.round(Math.min(Math.max(320, r.width * 0.55), Math.max(320, r.right - e.clientX))) };
        try {
          localStorage.setItem(W_KEY, JSON.stringify(next));
        } catch {
          /* 忽略 */
        }
        return next;
      });
    };
    const up = () => setDragging(null);
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
    return () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
    };
  }, [dragging]);

  const { book, doc, leftOpen, rightOpen, focus, panel, pendingCount, run } = ws;
  const running = run && (run.status === 'running' || run.status === 'starting');

  if (ws.bookId !== bookId) {
    return (
      <div className="ws" aria-busy="true">
        <Spinner label="打开作品…" />
      </div>
    );
  }

  return (
    <div className={`ws${focus ? ' ws--focus' : ''}`}>
      <header className="ws__bar">
        <a className="icon-btn" href="#/" aria-label="返回书库">
          <Icon name="arrowLeft" />
        </a>
        <button className={`icon-btn${leftOpen ? ' icon-btn--active' : ''}`} onClick={() => ws.toggle('leftOpen')} aria-label={leftOpen ? '收起资料目录' : '展开资料目录'} aria-pressed={leftOpen}>
          <Icon name="panelLeft" />
        </button>
        <div className="stack stack--tight grow" style={{ gap: 0 }}>
          <span className="ws__title ellipsis" title={book?.title}>
            {book ? `《${book.title}》` : '读取中…'}
          </span>
          <span className="ws__crumb ellipsis">{doc ? `${doc.group} / ${doc.name}` : '未打开文档'}</span>
        </div>
        {doc ? <SaveIndicator /> : null}
        {running ? <Pill tone="info" icon="spark">AI 运行中</Pill> : null}
        <Menu
          label="作品操作"
          items={[
            { key: 'info', label: '作品信息', icon: 'edit', onSelect: () => setInfoOpen(true) },
            {
              key: 'zip',
              label: '导出 Markdown 压缩包',
              icon: 'download',
              onSelect: async () => {
                try {
                  const r = await api.exportBook(bookId, 'zip');
                  if (!r.ok || !r.base64) throw new Error(r.message ?? '导出失败');
                  downloadBase64(r.name ?? 'book.zip', r.mime ?? 'application/zip', r.base64);
                } catch (e) {
                  toast.bad(errorText(e));
                }
              },
            },
            { key: 'settings', label: '作品默认技能与文风', icon: 'settings', onSelect: () => { location.hash = '#/settings/book'; } },
          ]}
        />
        <button className={`icon-btn${focus ? ' icon-btn--active' : ''}`} onClick={() => ws.toggle('focus')} aria-label="专注模式" aria-pressed={focus} title="专注模式：收起两侧栏">
          <Icon name="focus" />
        </button>
        <button className={`icon-btn${rightOpen ? ' icon-btn--active' : ''}`} onClick={() => ws.toggle('rightOpen')} aria-label={rightOpen ? '收起辅助区' : '展开辅助区'} aria-pressed={rightOpen}>
          <Icon name="panel" />
        </button>
      </header>
      <div
        ref={bodyRef}
        className={`ws__body${!leftOpen || focus ? ' ws__body--no-left' : ''}${!rightOpen || focus ? ' ws__body--no-right' : ''}`}
        style={{ ['--left-w' as string]: `${widths.left}px`, ['--right-w' as string]: `${widths.right}px` }}
      >
        <aside className="ws__left" aria-label="资料目录">
          {ws.treeError ? (
            <div className="notice notice--bad" style={{ margin: 12 }}>
              {ws.treeError}
            </div>
          ) : null}
          <FileTree />
        </aside>
        {leftOpen && !focus ? (
          <div className={`ws__resizer ws__resizer--left${dragging === 'left' ? ' ws__resizer--drag' : ''}`} style={{ left: widths.left - 4 }} onPointerDown={() => setDragging('left')} role="separator" aria-orientation="vertical" aria-label="调整目录宽度" />
        ) : null}
        <section className="ws__center" aria-label="书稿">
          <Editor />
        </section>
        {rightOpen && !focus ? (
          <div className={`ws__resizer${dragging === 'right' ? ' ws__resizer--drag' : ''}`} style={{ right: widths.right - 4 }} onPointerDown={() => setDragging('right')} role="separator" aria-orientation="vertical" aria-label="调整辅助区宽度" />
        ) : null}
        <aside className="ws__right" aria-label="辅助区">
          <div className="panel-head">
            <div className="tabs grow" role="tablist" aria-label="辅助面板">
              {PANELS.map((p) => (
                <button key={p.id} role="tab" aria-selected={panel === p.id} className="tab" onClick={() => ws.setPanel(p.id)}>
                  <Icon name={p.icon} size={15} />
                  {p.label}
                  {p.id === 'pending' && pendingCount > 0 ? <span className="count">{pendingCount}</span> : null}
                </button>
              ))}
            </div>
            <button className="icon-btn panel-close" onClick={() => ws.toggle('rightOpen', false)} aria-label="关闭辅助区">
              <Icon name="x" />
            </button>
          </div>
          <div className="panel-slot">
            {panel === 'assistant' ? <AssistantPanel /> : null}
            {panel === 'pending' ? <PendingPanel /> : null}
            {panel === 'pipeline' ? <PipelinePanel /> : null}
            {panel === 'context' ? <ContextPanel /> : null}
          </div>
        </aside>
      </div>
      <EditorPrefsBridge />
      {infoOpen ? <BookInfoDialog onClose={() => setInfoOpen(false)} /> : null}
    </div>
  );
}

/** 编辑器偏好变化时无需重载：CSS 变量已由 prefs.set 写入。这里只确保首帧应用。 */
function EditorPrefsBridge() {
  usePrefs((p) => p.fontSize);
  return null;
}
