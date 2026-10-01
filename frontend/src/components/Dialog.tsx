import { useEffect, useId, useRef, type ReactNode } from 'react';
import { Icon } from './Icon';

/** 可访问的模态框：Esc 关闭、焦点进入与归还、Tab 循环、点击遮罩关闭（可禁用）。 */
export function Dialog(props: {
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  wide?: boolean;
  dismissable?: boolean;
  labelledBy?: string;
}) {
  const { title, onClose, children, footer, wide, dismissable = true } = props;
  const ref = useRef<HTMLDivElement>(null);
  const id = useId();
  useEffect(() => {
    const prev = document.activeElement as HTMLElement | null;
    const el = ref.current;
    const first = el?.querySelector<HTMLElement>('[data-autofocus], input, textarea, select, button:not([data-close])');
    (first ?? el)?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && dismissable && !e.isComposing) {
        e.stopPropagation();
        onClose();
      }
      if (e.key === 'Tab' && el) {
        const f = Array.from(el.querySelectorAll<HTMLElement>('button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])')).filter(
          (x) => !x.hasAttribute('disabled'),
        );
        if (!f.length) return;
        const [a, z] = [f[0], f[f.length - 1]];
        if (e.shiftKey && document.activeElement === a) {
          e.preventDefault();
          z.focus();
        } else if (!e.shiftKey && document.activeElement === z) {
          e.preventDefault();
          a.focus();
        }
      }
    };
    document.addEventListener('keydown', onKey, true);
    return () => {
      document.removeEventListener('keydown', onKey, true);
      prev?.focus?.();
    };
  }, [onClose, dismissable]);
  return (
    <div
      className="backdrop"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget && dismissable) onClose();
      }}
    >
      <div className={`dialog${wide ? ' dialog--wide' : ''}`} role="dialog" aria-modal="true" aria-labelledby={id} ref={ref} tabIndex={-1}>
        <div className="dialog__head">
          <h2 className="dialog__title" id={id}>
            {title}
          </h2>
          {dismissable ? (
            <button className="icon-btn" data-close onClick={onClose} aria-label="关闭">
              <Icon name="x" />
            </button>
          ) : null}
        </div>
        <div className="dialog__body">{children}</div>
        {footer ? <div className="dialog__foot">{footer}</div> : null}
      </div>
    </div>
  );
}

/** 二次确认：危险动作必须明确告知后果。 */
export function ConfirmDialog(props: {
  title: string;
  body: ReactNode;
  confirmLabel: string;
  danger?: boolean;
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
    <Dialog
      title={props.title}
      onClose={props.onCancel}
      footer={
        <>
          <button className="btn" onClick={props.onCancel} disabled={props.busy}>
            取消
          </button>
          <button className={`btn ${props.danger ? 'btn--danger' : 'btn--primary'}`} onClick={props.onConfirm} disabled={props.busy} data-autofocus>
            {props.busy ? <Icon name="refresh" className="spin" size={16} /> : null}
            {props.confirmLabel}
          </button>
        </>
      }
    >
      <div className="muted">{props.body}</div>
    </Dialog>
  );
}
