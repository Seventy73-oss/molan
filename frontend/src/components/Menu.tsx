import { useEffect, useRef, useState, type ReactNode } from 'react';
import { Icon } from './Icon';

export interface MenuItem {
  key: string;
  label: string;
  icon?: string;
  danger?: boolean;
  disabled?: boolean;
  onSelect: () => void;
}

/** 下拉菜单：点击外部/Esc 关闭，方向键移动焦点。 */
export function Menu(props: { items: (MenuItem | 'sep')[]; label: string; trigger?: ReactNode; align?: 'left' | 'right'; small?: boolean }) {
  const [open, setOpen] = useState(false);
  const wrap = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (!wrap.current?.contains(e.target as Node)) setOpen(false);
    };
    const key = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        const items = Array.from(wrap.current?.querySelectorAll<HTMLButtonElement>('.menu__item:not(:disabled)') ?? []);
        const i = items.indexOf(document.activeElement as HTMLButtonElement);
        const n = e.key === 'ArrowDown' ? (i + 1) % items.length : (i - 1 + items.length) % items.length;
        items[n]?.focus();
        e.preventDefault();
      }
    };
    document.addEventListener('mousedown', close);
    document.addEventListener('keydown', key);
    requestAnimationFrame(() => wrap.current?.querySelector<HTMLButtonElement>('.menu__item:not(:disabled)')?.focus());
    return () => {
      document.removeEventListener('mousedown', close);
      document.removeEventListener('keydown', key);
    };
  }, [open]);
  return (
    <div ref={wrap} style={{ position: 'relative', display: 'inline-flex' }}>
      <button
        className={props.trigger ? 'btn btn--sm' : `icon-btn${props.small ? ' icon-btn--sm' : ''}`}
        aria-label={props.label}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        {props.trigger ?? <Icon name="more" />}
      </button>
      {open ? (
        <div className="menu" role="menu" style={{ top: 'calc(100% + 4px)', [props.align === 'left' ? 'left' : 'right']: 0 }}>
          {props.items.map((it, i) =>
            it === 'sep' ? (
              <div key={`sep${i}`} className="menu__sep" />
            ) : (
              <button
                key={it.key}
                role="menuitem"
                className={`menu__item${it.danger ? ' menu__item--danger' : ''}`}
                disabled={it.disabled}
                onClick={() => {
                  setOpen(false);
                  it.onSelect();
                }}
              >
                {it.icon ? <Icon name={it.icon} size={16} /> : null}
                <span className="grow">{it.label}</span>
              </button>
            ),
          )}
        </div>
      ) : null}
    </div>
  );
}
