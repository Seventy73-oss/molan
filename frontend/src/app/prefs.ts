/** 界面偏好（仅本机）：主题、书稿字号/行高/栏宽、自动保存。不影响服务端数据。 */
import { create } from 'zustand';

export type ThemePref = 'system' | 'light' | 'dark';

export interface Prefs {
  theme: ThemePref;
  fontSize: number;
  lineHeight: number;
  measure: number;
  autosave: boolean;
  autosaveDelay: number;
}

const KEY = 'molan.prefs';
const DEFAULTS: Prefs = { theme: 'system', fontSize: 18, lineHeight: 1.9, measure: 42, autosave: true, autosaveDelay: 2500 };

function load(): Prefs {
  try {
    const raw = localStorage.getItem(KEY);
    const p = raw ? { ...DEFAULTS, ...(JSON.parse(raw) as Partial<Prefs>) } : DEFAULTS;
    const t = localStorage.getItem('molan.theme');
    if (t === 'light' || t === 'dark') p.theme = t;
    return p;
  } catch {
    return DEFAULTS;
  }
}

export function applyTheme(t: ThemePref) {
  const el = document.documentElement;
  if (t === 'system') delete el.dataset.theme;
  else el.dataset.theme = t;
  try {
    if (t === 'system') localStorage.removeItem('molan.theme');
    else localStorage.setItem('molan.theme', t);
  } catch {
    /* 存储不可用时仅本次生效 */
  }
}

export function applyEditorVars(p: Prefs) {
  const s = document.documentElement.style;
  s.setProperty('--fs-manuscript', `${p.fontSize}px`);
  s.setProperty('--lh-manuscript', String(p.lineHeight));
  s.setProperty('--measure', `${p.measure}em`);
}

export const usePrefs = create<Prefs & { set: (p: Partial<Prefs>) => void }>((set, get) => ({
  ...load(),
  set: (p) => {
    const next = { ...get(), ...p };
    set(p);
    const { theme, fontSize, lineHeight, measure, autosave, autosaveDelay } = next;
    try {
      localStorage.setItem(KEY, JSON.stringify({ theme, fontSize, lineHeight, measure, autosave, autosaveDelay }));
    } catch {
      /* 忽略 */
    }
    if (p.theme) applyTheme(p.theme);
    applyEditorVars(next);
  },
}));
