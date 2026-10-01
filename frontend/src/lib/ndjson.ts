/**
 * 增量 NDJSON 解析器（与 molan-server /ipc/:cmd 协议对齐）。
 *
 * - 网络分块任意切分：按字节流解码（TextDecoder stream 模式），UTF-8 跨块不乱码；
 * - 一行一个 JSON；兼容 \r\n；空行忽略；
 * - 末尾无换行的最后一行在 end() 时解析（旧 glue 会静默丢弃）；
 * - 坏行不吞掉也不抛出：作为 {kind:'bad'} 帧交给调用方记录。
 */
export type Frame =
  | { kind: 'event'; channel: string; event: Record<string, unknown> }
  | { kind: 'heartbeat' }
  | { kind: 'result'; value: unknown }
  | { kind: 'error'; message: string; raw: unknown }
  | { kind: 'bad'; line: string };

export const HEARTBEAT_CHANNEL = '__hb__';

export function classify(obj: unknown, line: string): Frame {
  if (obj === null || typeof obj !== 'object' || Array.isArray(obj)) {
    return { kind: 'bad', line };
  }
  const o = obj as Record<string, unknown>;
  if ('ch' in o && 'e' in o) {
    const channel = String(o.ch);
    if (channel === HEARTBEAT_CHANNEL) return { kind: 'heartbeat' };
    const ev = o.e;
    if (ev && typeof ev === 'object' && !Array.isArray(ev)) {
      return { kind: 'event', channel, event: ev as Record<string, unknown> };
    }
    return { kind: 'bad', line };
  }
  if ('err' in o) {
    const err = o.err as Record<string, unknown> | string | null;
    const message =
      typeof err === 'string'
        ? err
        : String((err && (err.message ?? err.msg)) || '服务端返回错误');
    return { kind: 'error', message, raw: err };
  }
  if ('r' in o) return { kind: 'result', value: o.r };
  return { kind: 'bad', line };
}

export class NdjsonParser {
  private decoder = new TextDecoder('utf-8');
  private buffer = '';

  /** 喂入一块数据（Uint8Array 或已解码字符串），返回本块完成的帧。 */
  push(chunk: Uint8Array | string): Frame[] {
    this.buffer += typeof chunk === 'string' ? chunk : this.decoder.decode(chunk, { stream: true });
    const frames: Frame[] = [];
    let nl = this.buffer.indexOf('\n');
    while (nl >= 0) {
      const line = this.buffer.slice(0, nl);
      this.buffer = this.buffer.slice(nl + 1);
      const f = this.parseLine(line);
      if (f) frames.push(f);
      nl = this.buffer.indexOf('\n');
    }
    return frames;
  }

  /** 流结束：冲刷解码器并解析末尾无换行的最后一行。 */
  end(): Frame[] {
    this.buffer += this.decoder.decode();
    const rest = this.buffer;
    this.buffer = '';
    const f = this.parseLine(rest);
    return f ? [f] : [];
  }

  private parseLine(raw: string): Frame | null {
    const line = raw.endsWith('\r') ? raw.slice(0, -1) : raw;
    if (!line.trim()) return null;
    try {
      return classify(JSON.parse(line), line);
    } catch {
      return { kind: 'bad', line };
    }
  }
}
