import { describe, expect, it } from 'vitest';
import { chapterOf, diffStats, lineDiff, sha256Hex, snapOffset, splitsSurrogate, wordCount, wordDiff } from './text';
import { MAX_DRAFTS, judgeDraft, loadDraft, saveDraft, type LocalDraft } from './drafts';

class MemStorage {
  private m = new Map<string, string>();
  get length() {
    return this.m.size;
  }
  key(i: number) {
    return [...this.m.keys()][i] ?? null;
  }
  getItem(k: string) {
    return this.m.has(k) ? this.m.get(k)! : null;
  }
  setItem(k: string, v: string) {
    this.m.set(k, v);
  }
  removeItem(k: string) {
    this.m.delete(k);
  }
}

describe('utf16 offsets', () => {
  it('detects surrogate splits and snaps', () => {
    const s = 'a😀b';
    expect(s.length).toBe(4);
    expect(splitsSurrogate(s, 2)).toBe(true);
    expect(splitsSurrogate(s, 1)).toBe(false);
    expect(snapOffset(s, 2)).toBe(3);
    expect(snapOffset(s, 99)).toBe(4);
  });

  it('counts chinese words without whitespace, emoji as one', () => {
    expect(wordCount('第一章 \n  山风😀。')).toBe(7);
  });
});

describe('hash matches Rust content_hash (sha256 of UTF-8)', () => {
  it('hashes known vectors', async () => {
    expect(await sha256Hex('')).toBe('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855');
    expect(await sha256Hex('阿青\n')).toHaveLength(64);
    expect(await sha256Hex('abc')).toBe('ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad');
  });
});

describe('diff', () => {
  it('line and word diffs report adds and deletions', () => {
    const rows = lineDiff('甲\n乙\n', '甲\n丙\n');
    expect(rows.some((r) => r.kind === 'del' && r.text.includes('乙'))).toBe(true);
    expect(rows.some((r) => r.kind === 'add' && r.text.includes('丙'))).toBe(true);
    expect(diffStats(wordDiff('春风 拂面', '秋风 拂面')).add).toBeGreaterThan(0);
  });

  it('parses chapter numbers', () => {
    expect(chapterOf('第12章.md')).toBe(12);
    expect(chapterOf('细纲_第3章.md')).toBe(3);
    expect(chapterOf('人物.md')).toBeNull();
  });
});

describe('local drafts', () => {
  const d = (name: string, savedAt: number, text = 'x'): LocalDraft => ({ bookId: 'b', group: '正文', name, baseHash: 'h1', text, savedAt });

  it('saves, loads and evicts oldest beyond cap', () => {
    const s = new MemStorage();
    for (let i = 0; i < MAX_DRAFTS + 3; i++) expect(saveDraft(d(`第${i}章.md`, i), s)).toBe(true);
    expect(s.length).toBe(MAX_DRAFTS);
    expect(loadDraft('b', '正文', '第0章.md', s)).toBeNull();
    expect(loadDraft('b', '正文', `第${MAX_DRAFTS + 2}章.md`, s)?.text).toBe('x');
  });

  it('refuses oversized drafts', () => {
    expect(saveDraft(d('大.md', 1, 'x'.repeat(400_001)), new MemStorage())).toBe(false);
  });

  it('judges against server version', () => {
    expect(judgeDraft(null, 'h1', 'a').kind).toBe('none');
    expect(judgeDraft(d('a', 1, 'same'), 'h1', 'same').kind).toBe('same');
    expect(judgeDraft(d('a', 1, 'mine'), 'h1', 'server').kind).toBe('restorable');
    expect(judgeDraft(d('a', 1, 'mine'), 'h2', 'server').kind).toBe('diverged');
  });
});
