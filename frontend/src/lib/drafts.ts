/**
 * 本地草稿：作者未保存的编辑按「作品/文档/基线 hash」隔离保存在本机浏览器，
 * 恢复前必须与服务器版本核对。有容量与条数上限，绝不把整本书复制进浏览器存储。
 * 本地草稿只是安全网，不是保存：正式写入永远走 doc_write。
 */
export interface LocalDraft {
  bookId: string;
  group: string;
  name: string;
  baseHash: string | null;
  text: string;
  savedAt: number;
}

const PREFIX = 'molan.draft.';
export const MAX_DRAFTS = 20;
export const MAX_DRAFT_CHARS = 400_000;

type StorageLike = Pick<Storage, 'getItem' | 'setItem' | 'removeItem' | 'key' | 'length'>;

function store(): StorageLike | null {
  try {
    return typeof localStorage !== 'undefined' ? localStorage : null;
  } catch {
    return null;
  }
}

export function draftKey(bookId: string, group: string, name: string): string {
  return `${PREFIX}${bookId}/${group}/${name}`;
}

function allKeys(s: StorageLike): string[] {
  const keys: string[] = [];
  for (let i = 0; i < s.length; i++) {
    const k = s.key(i);
    if (k && k.startsWith(PREFIX)) keys.push(k);
  }
  return keys;
}

export function loadDraft(bookId: string, group: string, name: string, s: StorageLike | null = store()): LocalDraft | null {
  if (!s) return null;
  try {
    const raw = s.getItem(draftKey(bookId, group, name));
    if (!raw) return null;
    const d = JSON.parse(raw) as LocalDraft;
    return typeof d.text === 'string' ? d : null;
  } catch {
    return null;
  }
}

/** 保存草稿；超长不保存（返回 false 由界面提示），超出条数按最旧淘汰。 */
export function saveDraft(d: LocalDraft, s: StorageLike | null = store()): boolean {
  if (!s || d.text.length > MAX_DRAFT_CHARS) return false;
  try {
    s.setItem(draftKey(d.bookId, d.group, d.name), JSON.stringify(d));
    const keys = allKeys(s);
    if (keys.length > MAX_DRAFTS) {
      const aged = keys
        .map((k) => {
          try {
            return { k, t: (JSON.parse(s.getItem(k) ?? '{}') as LocalDraft).savedAt ?? 0 };
          } catch {
            return { k, t: 0 };
          }
        })
        .sort((a, b) => a.t - b.t);
      for (const x of aged.slice(0, keys.length - MAX_DRAFTS)) s.removeItem(x.k);
    }
    return true;
  } catch {
    return false;
  }
}

export function clearDraft(bookId: string, group: string, name: string, s: StorageLike | null = store()): void {
  try {
    s?.removeItem(draftKey(bookId, group, name));
  } catch {
    /* 存储不可用时无需处理 */
  }
}

export type DraftVerdict =
  | { kind: 'none' }
  | { kind: 'same' }
  | { kind: 'restorable'; draft: LocalDraft }
  | { kind: 'diverged'; draft: LocalDraft };

/** 与服务器版本核对：基线一致且内容不同 → 可恢复；基线已变 → 只能比较/另存，不能直接覆盖。 */
export function judgeDraft(draft: LocalDraft | null, serverHash: string | null, serverText: string): DraftVerdict {
  if (!draft) return { kind: 'none' };
  if (draft.text === serverText) return { kind: 'same' };
  if (draft.baseHash === serverHash) return { kind: 'restorable', draft };
  return { kind: 'diverged', draft };
}
