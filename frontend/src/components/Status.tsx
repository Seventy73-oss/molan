import type { ArtifactState } from '../lib/contracts';
import { Icon } from './Icon';

type Tone = 'neutral' | 'ok' | 'pending' | 'bad' | 'accent' | 'info';

const STATE_TONE: Record<ArtifactState, { tone: Tone; icon: string }> = {
  generating: { tone: 'info', icon: 'refresh' },
  generated: { tone: 'neutral', icon: 'file' },
  interrupted: { tone: 'pending', icon: 'pause' },
  failed: { tone: 'bad', icon: 'alert' },
  discarded: { tone: 'neutral', icon: 'x' },
  base_changed: { tone: 'pending', icon: 'diff' },
  saved: { tone: 'ok', icon: 'check' },
  partial: { tone: 'pending', icon: 'alert' },
  pending_review: { tone: 'pending', icon: 'inbox' },
  confirmed: { tone: 'ok', icon: 'check' },
  approved: { tone: 'ok', icon: 'check' },
  rejected: { tone: 'neutral', icon: 'x' },
  stale: { tone: 'pending', icon: 'clock' },
  conflict: { tone: 'bad', icon: 'diff' },
};

/** 状态徽标：图标 + 文案 + 局部状态色（不是大面积红绿），不依赖 hover 才可见。 */
export function StatusBadge({ state, label }: { state: ArtifactState | string; label: string }) {
  const t = STATE_TONE[state as ArtifactState] ?? { tone: 'neutral' as Tone, icon: 'info' };
  return (
    <span className={`status status--${t.tone}`}>
      <Icon name={t.icon} size={13} className={state === 'generating' ? 'spin' : undefined} />
      {label}
    </span>
  );
}

export function Pill({ tone = 'neutral', icon, children }: { tone?: Tone; icon?: string; children: React.ReactNode }) {
  return (
    <span className={`status status--${tone}`}>
      {icon ? <Icon name={icon} size={13} /> : null}
      {children}
    </span>
  );
}

export function Spinner({ size = 16, label }: { size?: number; label?: string }) {
  return (
    <span className="row muted" role="status">
      <Icon name="refresh" size={size} className="spin" />
      {label ? <span className="small">{label}</span> : <span className="visually-hidden">加载中</span>}
    </span>
  );
}
