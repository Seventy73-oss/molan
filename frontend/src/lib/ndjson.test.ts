import { describe, expect, it } from 'vitest';
import { NdjsonParser } from './ndjson';

const enc = new TextEncoder();

function feedBytes(text: string, cuts: number[]) {
  const bytes = enc.encode(text);
  const p = new NdjsonParser();
  const out = [] as ReturnType<NdjsonParser['push']>;
  let last = 0;
  for (const c of [...cuts, bytes.length]) {
    out.push(...p.push(bytes.slice(last, c)));
    last = c;
  }
  out.push(...p.end());
  return out;
}

describe('NdjsonParser', () => {
  it('handles UTF-8 split across chunks at every byte boundary', () => {
    const text = '{"ch":"1","e":{"type":"delta","text":"中文😀"}}\n{"r":{"ok":true}}\n';
    const bytes = enc.encode(text);
    for (let cut = 1; cut < bytes.length; cut++) {
      const frames = feedBytes(text, [cut]);
      expect(frames).toHaveLength(2);
      expect(frames[0]).toEqual({ kind: 'event', channel: '1', event: { type: 'delta', text: '中文😀' } });
      expect(frames[1]).toEqual({ kind: 'result', value: { ok: true } });
    }
  });

  it('parses trailing line without newline, CRLF and skips blank lines', () => {
    const frames = feedBytes('\r\n{"ch":"1","e":{"type":"a"}}\r\n\n{"r":42}', [3, 9]);
    expect(frames.map((f) => f.kind)).toEqual(['event', 'result']);
    expect(frames[1]).toEqual({ kind: 'result', value: 42 });
  });

  it('classifies heartbeat, error frame and bad lines', () => {
    const frames = feedBytes(
      '{"ch":"__hb__","e":{"type":"progress","chars":-1}}\nnot json\n{"err":{"message":"坏了"}}\n[1]\n',
      [],
    );
    expect(frames.map((f) => f.kind)).toEqual(['heartbeat', 'bad', 'error', 'bad']);
    expect(frames[2]).toMatchObject({ kind: 'error', message: '坏了' });
  });

  it('keeps multi-line JSON string content intact', () => {
    const frames = feedBytes('{"ch":"1","e":{"type":"delta","text":"第一行\\n第二行\\r\\n"}}\n', [5, 17, 30]);
    expect(frames[0]).toMatchObject({ event: { text: '第一行\n第二行\r\n' } });
  });
});
