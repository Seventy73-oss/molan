/**
 * IPC 客户端：POST /ipc/:cmd，请求体 {args, options}，响应 NDJSON。
 *
 * - 同源 Cookie 鉴权（credentials: same-origin），绝不在页面里放 token；401 → 登录页；
 * - invoke：一次性命令，返回最后的 {r}；{err} 抛 IpcError；
 * - stream：流式命令，事件逐帧回调（不整段缓冲）；心跳只用于活性；
 * - 断开连接 ≠ 服务端取消：停止生成必须显式调用 abort_chat({requestId})。
 */
import { NdjsonParser, type Frame } from './ndjson';

export class IpcError extends Error {
  readonly cmd: string;
  readonly status?: number;
  readonly code?: string;
  constructor(cmd: string, message: string, opts: { status?: number; code?: string } = {}) {
    super(message);
    this.name = 'IpcError';
    this.cmd = cmd;
    this.status = opts.status;
    this.code = opts.code;
  }
}

export type IpcEvent = Record<string, unknown> & { type?: string };

export interface StreamOptions {
  signal?: AbortSignal;
  /** 每收到任意帧（含心跳）调用，用于活性指示。 */
  onActivity?: () => void;
  /** 坏行记录（不中断流）。 */
  onBadLine?: (line: string) => void;
}

const CHANNEL = '__CHANNEL__:1';

function onUnauthorized() {
  if (typeof window !== 'undefined' && !location.pathname.startsWith('/login')) {
    location.assign('/login');
  }
}

async function post(cmd: string, args: Record<string, unknown>, signal?: AbortSignal): Promise<Response> {
  let resp: Response;
  try {
    resp = await fetch(`/ipc/${encodeURIComponent(cmd)}`, {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ args, options: {} }),
      signal,
    });
  } catch (e) {
    if ((e as Error)?.name === 'AbortError') throw e;
    throw new IpcError(cmd, '无法连接墨澜服务，请确认服务已启动');
  }
  if (resp.status === 401) {
    onUnauthorized();
    throw new IpcError(cmd, '登录已失效，请重新登录', { status: 401 });
  }
  if (!resp.ok) {
    const text = await resp.text().catch(() => '');
    throw new IpcError(cmd, `服务返回 HTTP ${resp.status}${text ? `：${text.slice(0, 200)}` : ''}`, {
      status: resp.status,
    });
  }
  return resp;
}

async function pump(
  cmd: string,
  resp: Response,
  onFrame: (f: Frame) => void,
  opts: StreamOptions,
): Promise<unknown> {
  const parser = new NdjsonParser();
  let result: unknown = undefined;
  let error: IpcError | null = null;
  const handle = (frames: Frame[]) => {
    for (const f of frames) {
      opts.onActivity?.();
      if (f.kind === 'result') result = f.value;
      else if (f.kind === 'error') error = new IpcError(cmd, f.message);
      else if (f.kind === 'bad') opts.onBadLine?.(f.line);
      else onFrame(f);
    }
  };
  if (!resp.body) {
    handle(parser.push(await resp.text()));
  } else {
    const reader = resp.body.getReader();
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      if (value) handle(parser.push(value));
    }
  }
  handle(parser.end());
  if (error) throw error;
  return result;
}

/** 一次性命令。 */
export async function invoke<T = unknown>(
  cmd: string,
  args: Record<string, unknown> = {},
  opts: { signal?: AbortSignal } = {},
): Promise<T> {
  const resp = await post(cmd, args, opts.signal);
  return (await pump(cmd, resp, () => {}, {})) as T;
}

/** 流式命令：事件逐帧回调，最终返回 {r}。 */
export async function stream<T = unknown>(
  cmd: string,
  args: Record<string, unknown>,
  onEvent: (e: IpcEvent) => void,
  opts: StreamOptions = {},
): Promise<T> {
  const resp = await post(cmd, { ...args, onEvent: CHANNEL }, opts.signal);
  return (await pump(
    cmd,
    resp,
    (f) => {
      if (f.kind === 'event') onEvent(f.event as IpcEvent);
    },
    opts,
  )) as T;
}

export function newRequestId(prefix = 'req'): string {
  const rnd =
    typeof crypto !== 'undefined' && 'randomUUID' in crypto
      ? crypto.randomUUID()
      : `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
  return `${prefix}-${rnd}`;
}

export function errorText(e: unknown): string {
  if (e instanceof Error) return e.message;
  return String(e ?? '未知错误');
}
