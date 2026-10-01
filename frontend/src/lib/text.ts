/** 文本工具：UTF-16 偏移、字数、哈希、差异。浏览器字符串本身就是 UTF-16，textarea 选区即 UTF-16 偏移。 */
import { diffLines, diffWordsWithSpace, type Change } from 'diff';

/** 偏移是否落在代理对中间（服务端会拒绝这样的偏移）。 */
export function splitsSurrogate(s: string, off: number): boolean {
  if (off <= 0 || off >= s.length) return false;
  const prev = s.charCodeAt(off - 1);
  const next = s.charCodeAt(off);
  return prev >= 0xd800 && prev <= 0xdbff && next >= 0xdc00 && next <= 0xdfff;
}

/** 把偏移调整到最近的码点边界（向后）。 */
export function snapOffset(s: string, off: number): number {
  const o = Math.max(0, Math.min(off, s.length));
  return splitsSurrogate(s, o) ? o + 1 : o;
}

/** 中文写作口径字数：去掉空白后的码点数。 */
export function wordCount(s: string): number {
  let n = 0;
  for (const ch of s) if (!/\s/.test(ch)) n++;
  return n;
}

/** 与 Rust content_hash 一致：UTF-8 字节的 SHA-256 小写 hex。 */
export async function sha256Hex(text: string): Promise<string> {
  const bytes = new TextEncoder().encode(text);
  const digest = await crypto.subtle.digest('SHA-256', bytes);
  return Array.from(new Uint8Array(digest))
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('');
}

export interface DiffRow {
  kind: 'same' | 'add' | 'del';
  text: string;
}

/** 行级差异（长文本用），超长时退化为摘要避免卡顿。 */
export function lineDiff(a: string, b: string, maxChars = 200_000): DiffRow[] {
  if (a.length + b.length > maxChars) {
    return [{ kind: 'same', text: `（文本过长：原文 ${a.length} 字符，新文 ${b.length} 字符，请在编辑器中分段比较）` }];
  }
  return diffLines(a, b).map((c: Change) => ({ kind: c.added ? 'add' : c.removed ? 'del' : 'same', text: c.value }));
}

/** 词级差异（片段/选区用）。 */
export function wordDiff(a: string, b: string): DiffRow[] {
  return diffWordsWithSpace(a, b).map((c: Change) => ({ kind: c.added ? 'add' : c.removed ? 'del' : 'same', text: c.value }));
}

export function diffStats(rows: DiffRow[]): { add: number; del: number } {
  let add = 0;
  let del = 0;
  for (const r of rows) {
    if (r.kind === 'add') add += wordCount(r.text);
    if (r.kind === 'del') del += wordCount(r.text);
  }
  return { add, del };
}

export function shortHash(h: string | null | undefined): string {
  return h ? h.slice(0, 8) : '—';
}

export function timeAgo(ms: number | null | undefined, now = Date.now()): string {
  if (!ms) return '';
  const d = Math.max(0, now - ms);
  if (d < 60_000) return '刚刚';
  if (d < 3_600_000) return `${Math.floor(d / 60_000)} 分钟前`;
  if (d < 86_400_000) return `${Math.floor(d / 3_600_000)} 小时前`;
  const dt = new Date(ms);
  return `${dt.getMonth() + 1}月${dt.getDate()}日`;
}

export function formatCount(n: number | null | undefined): string {
  const v = n ?? 0;
  if (v >= 10000) return `${(v / 10000).toFixed(v >= 100000 ? 0 : 1)} 万`;
  return String(v);
}

/** 章号：文件名「第N章」→ N（阿拉伯数字；中文数字由服务端解析）。 */
export function chapterOf(name: string | null | undefined): number | null {
  const m = /第\s*(\d+)\s*章/.exec(name ?? '');
  return m ? Number(m[1]) : null;
}
