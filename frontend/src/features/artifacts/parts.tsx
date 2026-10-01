/**
 * 交付卡片的共用部件：CardShell / ArtifactPreview / FragmentDiff / ItemList / SkillSummary / ReceiptDetails / ActionBar。
 * 卡片只负责呈现服务端投影出的状态与动作；任何「已保存」都来自交付回执与磁盘复核，不在前端推断。
 */
import { useMemo, type ReactNode } from 'react';
import { Icon } from '../../components/Icon';
import { Markdown } from '../../components/Markdown';
import { Menu } from '../../components/Menu';
import { StatusBadge } from '../../components/Status';
import type { ArtifactAction, ArtifactView, TaskId } from '../../lib/contracts';
import { diffStats, shortHash, timeAgo, wordDiff } from '../../lib/text';
import { TASK_LABELS } from '../../state/workspace';

export const KIND_ICON: Record<string, string> = {
  outline_draft: 'list',
  body_draft: 'feather',
  revision: 'edit',
  humanize_rewrite: 'feather',
  review_report: 'search',
  book_setup: 'books',
  proposal: 'diff',
  multi_file: 'folder',
  plot_note: 'route',
  summary: 'list',
  skill_draft: 'layers',
};

export const ACTION_ICON: Record<string, string> = {
  save_outline: 'save',
  save_as: 'save',
  save_skill: 'save',
  open_skill: 'layers',
  submit_pending: 'inbox',
  confirm_outline: 'check',
  approve: 'check',
  reject: 'x',
  apply_selection: 'edit',
  insert_after: 'plus',
  apply_replace: 'edit',
  compare: 'diff',
  open_file: 'file',
  draft_body: 'feather',
  edit: 'edit',
  copy: 'copy',
  discard: 'trash',
  legacy_save_doc: 'save',
  legacy_book_setup: 'books',
  legacy_confirm_outline: 'check',
  legacy_approve: 'check',
  legacy_reject: 'x',
  legacy_accept_proposal: 'check',
  legacy_reject_proposal: 'x',
};

export function actionLabel(action: string): string {
  return (
    {
      save: '保存',
      save_skill: '保存为技能',
      submit_pending: '提交待审',
      confirm_outline: '确认细纲',
      approve: '定稿',
      reject: '驳回',
      discard: '放弃',
    } as Record<string, string>
  )[action] ?? action;
}

/** 作品 / 章节 / 目标路径 / 片段标注；技能草稿显示适用任务。 */
export function targetLine(a: ArtifactView, book?: string | null): string {
  const t = (a.target ?? {}) as Record<string, unknown> & { ch?: number; group?: string; name?: string };
  const parts: string[] = [];
  if (a.kind === 'skill_draft') {
    parts.push('技能库');
    if (typeof t.skillTask === 'string') parts.push(`适用：${TASK_LABELS[t.skillTask as TaskId] ?? t.skillTask}`);
    return parts.join(' · ');
  }
  if (book) parts.push(`《${book}》`);
  if (t.ch) parts.push(`第${t.ch}章`);
  if (t.name) parts.push(`${t.group ?? ''}/${t.name}`);
  if (a.scope === 'fragment') parts.push('片段，非全文');
  return parts.join(' · ');
}

/** 外壳：类型图标、标题、副信息与明确的状态徽标。 */
export function CardShell({ a, book, children }: { a: ArtifactView; book?: string | null; children: ReactNode }) {
  const line = targetLine(a, book);
  return (
    <article className={`acard acard--${a.state}`} aria-label={`${a.kindLabel}：${a.title}`}>
      <header className="acard__head">
        <span className="acard__icon" aria-hidden>
          <Icon name={KIND_ICON[a.kind] ?? 'file'} size={17} />
        </span>
        <div className="stack stack--tight grow" style={{ gap: 2, minWidth: 0 }}>
          <span className="acard__title break">{a.title || a.kindLabel}</span>
          <span className="acard__meta">
            <span>{a.kindLabel}</span>
            {line ? <span className="break">{line}</span> : null}
            <span>{a.chars.toLocaleString()} 字</span>
            {a.rev > 1 ? <span>修订 {a.rev}</span> : null}
            {a.legacy ? <span>旧版记录</span> : null}
          </span>
        </div>
        <StatusBadge state={a.state} label={a.stateLabel} />
      </header>
      {a.summary ? <p className={`acard__summary acard__summary--${a.state} break`}>{a.summary}</p> : null}
      {children}
    </article>
  );
}

/** 多文件交付：逐项状态，绝不按一次总 ok 把所有项刷绿。 */
export function ItemList({ a }: { a: ArtifactView }) {
  const items = a.items ?? [];
  if (!items.length) return null;
  return (
    <ul className="acard__items">
      {items.map((it) => (
        <li key={it.index} className="acard__item">
          <StatusBadge state={it.state} label={it.stateLabel} />
          <span className="grow ellipsis" title={it.note || undefined}>
            {it.title || it.name}
          </span>
          <span className="faint small break">{it.location?.name ? `${it.location.group}/${it.location.name}` : it.group && it.name ? `建议：${it.group}/${it.name}` : ''}</span>
        </li>
      ))}
    </ul>
  );
}

/** 选区改写的真实差异（按词）：删除线为原选区内容，下划线为新内容。 */
export function FragmentDiff({ before, after }: { before: string; after: string }) {
  const rows = useMemo(() => wordDiff(before, after), [before, after]);
  const st = diffStats(rows);
  return (
    <div className="acard__fragment">
      <span className="section-title">
        选区改动 · 新增 {st.add} 字，删除 {st.del} 字
      </span>
      <div className="diff diff--inline" aria-label="选区改动">
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

/** 预览区：窄卡默认折叠，长文进阅读对话框；片段显示真实差异。 */
export function ArtifactPreview({ a, expanded, onToggle, onRead }: { a: ArtifactView; expanded: boolean; onToggle: () => void; onRead: () => void }) {
  if (!a.content) return null;
  const t = a.target ?? {};
  const fragment = a.scope === 'fragment' && typeof t.selectionText === 'string';
  return (
    <>
      <div className={`acard__preview${expanded ? ' acard__preview--open' : ''}`}>
        {fragment ? <FragmentDiff before={t.selectionText as string} after={a.content} /> : <Markdown text={a.content} />}
      </div>
      {a.chars > 600 || a.truncated ? (
        <button className="btn btn--sm btn--ghost acard__more" onClick={a.truncated ? onRead : onToggle}>
          <Icon name={expanded ? 'chevronDown' : 'book'} size={14} />
          {a.truncated ? '阅读全文' : expanded ? '收起' : '展开'}
        </button>
      ) : null}
    </>
  );
}

/** 生成依据：技能（主/辅、版本）、文风、去味、模型、计划 hash。默认收起。 */
export function SkillSummary({ a }: { a: ArtifactView }) {
  const prov = a.provenance;
  if (!prov || !(prov.skills?.length || prov.model)) return null;
  const rows: [string, ReactNode][] = [];
  if (prov.skills?.length) rows.push(['技能', prov.skills.map((s) => `${s.name}${s.role === 'primary' ? '（主）' : ''} r${s.rev}`).join('、')]);
  if (prov.style?.label) rows.push(['文风', prov.style.label]);
  if (prov.humanize?.label) rows.push(['去AI味', prov.humanize.label]);
  if (prov.model) rows.push(['模型', prov.model]);
  if (prov.planHash) rows.push(['计划', <span className="mono">{shortHash(prov.planHash)}</span>]);
  return (
    <details className="acard__details">
      <summary className="small muted">生成依据</summary>
      <dl className="kv">
        {rows.map(([k, v]) => (
          <div key={k} style={{ display: 'contents' }}>
            <dt>{k}</dt>
            <dd>{v}</dd>
          </div>
        ))}
      </dl>
    </details>
  );
}

function deliveryError(detail: unknown): string {
  const d = (detail ?? {}) as { error?: string | { message?: string } };
  if (typeof d.error === 'string') return d.error;
  return d.error?.message ?? '';
}

/** 交付回执：每次交付的结果、去向与时间（失败原因原样保留）。 */
export function ReceiptDetails({ a }: { a: ArtifactView }) {
  if (!a.deliveries.length) return null;
  const label = (s: string) => ({ committed: '成功', noop: '无变化', conflict: '冲突' })[s] ?? '失败';
  return (
    <details className="acard__details">
      <summary className="small muted">交付回执（{a.deliveries.length}）</summary>
      <ul className="receipts">
        {a.deliveries.map((d) => {
          const err = deliveryError(d.detail);
          return (
            <li key={d.id} className="receipt-row small">
              <span className={`tag tag--${d.status}`}>{label(d.status)}</span>
              <span className="grow break">
                {actionLabel(d.action)} {d.group && d.name ? `${d.group}/${d.name}` : ''}
                {err ? `：${err}` : ''}
              </span>
              <span className="faint nowrap">{timeAgo(d.createdAt)}</span>
            </li>
          );
        })}
      </ul>
    </details>
  );
}

/** 动作栏：一个与状态对应的主动作，最多两个次动作，其余进菜单；执行中整栏防重入。 */
export function ActionBar({ actions, busy, onAction }: { actions: ArtifactAction[]; busy: string | null; onAction: (a: ArtifactAction) => void }) {
  const primary = actions.find((x) => x.primary);
  const secondary = actions.filter((x) => !x.primary);
  if (!primary && !secondary.length) return null;
  return (
    <footer className="acard__actions">
      {primary ? (
        <button className="btn btn--primary btn--sm" onClick={() => onAction(primary)} disabled={!!busy}>
          {busy === primary.id ? <Icon name="refresh" size={14} className="spin" /> : <Icon name={ACTION_ICON[primary.id] ?? 'check'} size={14} />}
          {primary.label}
        </button>
      ) : null}
      {secondary.slice(0, 2).map((s) => (
        <button key={s.id} className="btn btn--sm" onClick={() => onAction(s)} disabled={!!busy}>
          {busy === s.id ? <Icon name="refresh" size={14} className="spin" /> : null}
          {s.label}
        </button>
      ))}
      {secondary.length > 2 ? (
        <Menu
          label="更多操作"
          small
          items={secondary.slice(2).map((s) => ({
            key: s.id,
            label: s.label,
            icon: ACTION_ICON[s.id],
            danger: s.id === 'discard' || s.id === 'reject',
            disabled: !!busy,
            onSelect: () => onAction(s),
          }))}
        />
      ) : null}
    </footer>
  );
}
