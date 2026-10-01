import { create } from 'zustand';
import { Icon } from './Icon';

export interface Toast {
  id: number;
  tone: 'ok' | 'bad' | 'info';
  text: string;
}

interface ToastState {
  toasts: Toast[];
  push: (tone: Toast['tone'], text: string) => void;
  dismiss: (id: number) => void;
}

let seq = 0;

export const useToasts = create<ToastState>((set, get) => ({
  toasts: [],
  push: (tone, text) => {
    const id = ++seq;
    set({ toasts: [...get().toasts.slice(-3), { id, tone, text }] });
    // 失败信息停留更久，且始终可手动关闭；不会自动吞掉错误
    setTimeout(() => get().dismiss(id), tone === 'bad' ? 9000 : 4000);
  },
  dismiss: (id) => set({ toasts: get().toasts.filter((t) => t.id !== id) }),
}));

export const toast = {
  ok: (t: string) => useToasts.getState().push('ok', t),
  bad: (t: string) => useToasts.getState().push('bad', t),
  info: (t: string) => useToasts.getState().push('info', t),
};

export function ToastStack() {
  const { toasts, dismiss } = useToasts();
  return (
    <div className="toast-stack" aria-live="polite" role="status">
      {toasts.map((t) => (
        <div key={t.id} className={`toast toast--${t.tone}`}>
          <Icon name={t.tone === 'ok' ? 'check' : t.tone === 'bad' ? 'alert' : 'info'} size={16} />
          <span className="grow break">{t.text}</span>
          <button className="icon-btn icon-btn--sm" onClick={() => dismiss(t.id)} aria-label="关闭提示">
            <Icon name="x" size={14} />
          </button>
        </div>
      ))}
    </div>
  );
}
