import { useMemo, useState } from 'react';
import { ConfirmDialog, Dialog } from '../../components/Dialog';
import { Icon } from '../../components/Icon';
import { Menu } from '../../components/Menu';
import { Spinner } from '../../components/Status';
import { toast } from '../../components/Toasts';
import { api } from '../../lib/api';
import type { TreeFile, TreeGroup } from '../../lib/contracts';
import { errorText } from '../../lib/ipc';
import { useWs } from '../../state/workspace';
import { FileTrashDialog } from './BookDialogs';

const ORDER: Record<string, number> = { 设定: 0, 细纲: 1, 正文: 2, 参考: 3 };

function naturalSort(a: TreeFile, b: TreeFile) {
  return a.name.localeCompare(b.name, 'zh-Hans-CN', { numeric: true });
}

export function FileTree() {
  const { tree, bookId, doc, openDoc, refreshTree } = useWs();
  const [closed, setClosed] = useState<Record<string, boolean>>({});
  const [q, setQ] = useState('');
  const [creating, setCreating] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<{ group: string; file: TreeFile } | null>(null);
  const [deleting, setDeleting] = useState<{ group: string; file: TreeFile } | null>(null);
  const [folderDialog, setFolderDialog] = useState(false);
  const [trashOpen, setTrashOpen] = useState(false);
  const [busy, setBusy] = useState(false);

  const groups = useMemo(
    () =>
      [...tree].sort((a, b) => (ORDER[a.dir] ?? 9) - (ORDER[b.dir] ?? 9)).map((g) => ({
        ...g,
        files: g.files.filter((f) => !q.trim() || f.name.includes(q.trim())).sort(naturalSort),
      })),
    [tree, q],
  );

  if (!bookId) return null;
  if (!tree.length) {
    return (
      <div className="tree-empty">
        <Spinner label="读取目录" />
      </div>
    );
  }

  const flag = async (g: TreeGroup, f: TreeFile, k: 'locked' | 'aiOff') => {
    try {
      await api.setFileFlag(bookId, g.dir, f.name, k, !f[k]);
      await refreshTree();
    } catch (e) {
      toast.bad(errorText(e));
    }
  };

  return (
    <div className="tree">
      <div className="tree__tools">
        <label className="search search--sm grow">
          <Icon name="search" size={15} />
          <input className="search__input" placeholder="筛选文件" value={q} onChange={(e) => setQ(e.target.value)} aria-label="筛选文件" />
        </label>
        <Menu
          label="目录操作"
          small
          items={[
            { key: 'folder', label: '新建分组', icon: 'folder', onSelect: () => setFolderDialog(true) },
            { key: 'refresh', label: '刷新目录', icon: 'refresh', onSelect: () => void refreshTree() },
            { key: 'trash', label: '文件回收站', icon: 'trash', onSelect: () => setTrashOpen(true) },
          ]}
        />
      </div>
      <div className="panel-body">
        {groups.map((g) => {
          const isClosed = closed[g.dir];
          const pendingGroup = g.dir === '正文待审';
          return (
            <section key={g.key} className="tree__group">
              <div className="tree__head">
                <button className="tree__toggle" onClick={() => setClosed({ ...closed, [g.dir]: !isClosed })} aria-expanded={!isClosed}>
                  <Icon name={isClosed ? 'chevronRight' : 'chevronDown'} size={14} />
                  <span className="grow ellipsis">{g.label === g.dir ? g.dir : `${g.dir}`}</span>
                  <span className="faint small">{g.files.length}</span>
                </button>
                {!pendingGroup ? (
                  <button className="icon-btn icon-btn--sm" onClick={() => setCreating(g.dir)} aria-label={`在${g.dir}中新建文件`} title="新建文件">
                    <Icon name="plus" size={15} />
                  </button>
                ) : null}
              </div>
              {!isClosed ? (
                <ul className="tree__files">
                  {g.files.length === 0 ? <li className="tree__none faint small">（空）</li> : null}
                  {g.files.map((f) => {
                    const active = doc?.group === g.dir && doc?.name === f.name;
                    return (
                      <li key={f.name} className={`tree__file${active ? ' tree__file--active' : ''}`}>
                        <button className="tree__name" onClick={() => void openDoc(g.dir, f.name)} aria-current={active ? 'true' : undefined} title={f.name}>
                          <Icon name="file" size={15} />
                          <span className="ellipsis grow">{f.name.replace(/\.md$/, '')}</span>
                          {f.locked ? <Icon name="lock" size={13} title="已锁定：AI 不可改写" /> : null}
                          {f.aiOff ? <Icon name="eyeOff" size={13} title="AI 不可见" /> : null}
                        </button>
                        {!pendingGroup ? (
                          <Menu
                            label={`${f.name} 操作`}
                            small
                            items={[
                              { key: 'rename', label: '重命名', icon: 'edit', onSelect: () => setRenaming({ group: g.dir, file: f }) },
                              { key: 'lock', label: f.locked ? '解除锁定' : '锁定（禁止 AI 改写）', icon: 'lock', onSelect: () => void flag(g, f, 'locked') },
                              { key: 'aioff', label: f.aiOff ? '允许 AI 读取' : '对 AI 隐藏', icon: 'eyeOff', onSelect: () => void flag(g, f, 'aiOff') },
                              'sep',
                              { key: 'del', label: '移到回收站', icon: 'trash', danger: true, onSelect: () => setDeleting({ group: g.dir, file: f }) },
                            ]}
                          />
                        ) : null}
                      </li>
                    );
                  })}
                </ul>
              ) : null}
            </section>
          );
        })}
      </div>

      {creating ? (
        <NameDialog
          title={`在「${creating}」新建文件`}
          initial={creating === '正文' ? `第${nextChapter(tree)}章.md` : creating === '细纲' ? `细纲_第${nextChapter(tree)}章.md` : '新文档.md'}
          onClose={() => setCreating(null)}
          onSubmit={async (name) => {
            const n = /\.(md|txt)$/i.test(name) ? name : `${name}.md`;
            await api.createFile(bookId, creating, n);
            await refreshTree();
            setCreating(null);
            await openDoc(creating, n);
          }}
        />
      ) : null}
      {renaming ? (
        <NameDialog
          title="重命名"
          initial={renaming.file.name}
          onClose={() => setRenaming(null)}
          onSubmit={async (name) => {
            await api.renameFile(bookId, renaming.group, renaming.file.name, name);
            await refreshTree();
            setRenaming(null);
            if (doc?.group === renaming.group && doc.name === renaming.file.name) await openDoc(renaming.group, name);
          }}
        />
      ) : null}
      {trashOpen ? <FileTrashDialog onClose={() => setTrashOpen(false)} /> : null}
      {folderDialog ? (
        <NameDialog
          title="新建分组"
          initial="新分组"
          onClose={() => setFolderDialog(false)}
          onSubmit={async (name) => {
            const r = (await api.createFolder(bookId, name)) as { ok?: boolean; err?: string };
            if (r && r.ok === false) throw new Error(r.err ?? '创建失败');
            await refreshTree();
            setFolderDialog(false);
          }}
        />
      ) : null}
      {deleting ? (
        <ConfirmDialog
          title="移到回收站"
          body={<>「{deleting.group}/{deleting.file.name}」会移到回收站，可恢复。</>}
          confirmLabel="移到回收站"
          danger
          busy={busy}
          onCancel={() => setDeleting(null)}
          onConfirm={async () => {
            setBusy(true);
            try {
              const r = await api.deleteFile(bookId, deleting.group, deleting.file.name);
              toast.ok(`已移到回收站${r.trashId ? '' : '（文件已不存在）'}`);
              if (doc?.group === deleting.group && doc.name === deleting.file.name) useWs.getState().closeDoc();
              await refreshTree();
              setDeleting(null);
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

function nextChapter(tree: TreeGroup[]): number {
  const g = tree.find((x) => x.dir === '正文');
  let max = 0;
  for (const f of g?.files ?? []) {
    const m = /第(\d+)章/.exec(f.name);
    if (m) max = Math.max(max, Number(m[1]));
  }
  return max + 1;
}

function NameDialog({ title, initial, onClose, onSubmit }: { title: string; initial: string; onClose: () => void; onSubmit: (name: string) => Promise<void> }) {
  const [name, setName] = useState(initial);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const submit = async () => {
    const n = name.trim();
    if (!n) return setErr('名称不能为空');
    if (/[\\/:*?"<>|]/.test(n)) return setErr('名称不能包含 \\ / : * ? " < > |');
    setBusy(true);
    try {
      await onSubmit(n);
    } catch (e) {
      setErr(errorText(e));
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={title}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            取消
          </button>
          <button className="btn btn--primary" disabled={busy} onClick={() => void submit()}>
            确定
          </button>
        </>
      }
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <input className="input" value={name} onChange={(e) => setName(e.target.value)} data-autofocus aria-label="名称" />
      </form>
      {err ? <p className="notice notice--bad">{err}</p> : null}
    </Dialog>
  );
}
