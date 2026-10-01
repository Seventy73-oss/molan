import { useMemo, useState } from 'react';
import { diffStats, lineDiff, wordDiff } from '../lib/text';

/** 真实差异视图（不是示意）：短文本按词，长文本按行；删除线 + 底色 + 统计，不只靠颜色区分。 */
export function DiffView({ before, after, beforeLabel = '原文', afterLabel = '新文' }: { before: string; after: string; beforeLabel?: string; afterLabel?: string }) {
  const [mode, setMode] = useState<'auto' | 'line' | 'word'>('auto');
  const rows = useMemo(() => {
    const useWord = mode === 'word' || (mode === 'auto' && before.length + after.length < 12_000);
    return useWord ? wordDiff(before, after) : lineDiff(before, after);
  }, [before, after, mode]);
  const st = diffStats(rows);
  const same = before === after;
  return (
    <div className="stack stack--tight">
      <div className="row row--between row--wrap">
        <span className="diff__stats">
          {beforeLabel} → {afterLabel}：{same ? '内容相同' : `新增 ${st.add} 字，删除 ${st.del} 字`}
        </span>
        <div className="tabs" role="tablist" aria-label="差异粒度">
          {(['auto', 'word', 'line'] as const).map((m) => (
            <button key={m} className="tab" role="tab" aria-selected={mode === m} onClick={() => setMode(m)}>
              {m === 'auto' ? '自动' : m === 'word' ? '按词' : '按行'}
            </button>
          ))}
        </div>
      </div>
      <div className="diff" aria-label="差异内容">
        {rows.map((r, i) =>
          r.kind === 'same' ? (
            <span key={i}>{r.text}</span>
          ) : r.kind === 'add' ? (
            <ins key={i} className="diff__add">
              {r.text}
            </ins>
          ) : (
            <del key={i} className="diff__del">
              {r.text}
            </del>
          ),
        )}
      </div>
    </div>
  );
}
