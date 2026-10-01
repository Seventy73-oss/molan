import { Icon } from '../../components/Icon';
import { Spinner } from '../../components/Status';
import type { TaskPreview } from '../../lib/contracts';
import { shortHash } from '../../lib/text';

const SOURCE: Record<string, string> = {
  explicit: '本次选择',
  book_primary: '作品默认',
  book_support: '作品默认',
  auto_match: '按名称识别',
};

/** 发起前的实际生效计划：服务端按同一规则解析；执行时会重新解析并冻结，预览不是授权。 */
export function PlanPreview({ preview, error }: { preview: TaskPreview | null; error: string | null }) {
  if (error) return <div className="notice notice--bad">计划预览失败：{error}</div>;
  if (!preview) return <Spinner label="解析生效计划" />;
  const { plan, context } = preview;
  return (
    <div className="plan">
      <div className="plan__row">
        <span className="plan__k">主技能</span>
        <span className="plan__v">
          {plan.skills.filter((s) => s.role === 'primary').map((s) => (
            <span key={s.id} className="tag" title={`版本 r${s.rev}`}>
              {s.name}
              <span className="faint">· {SOURCE[s.source] ?? s.source}</span>
            </span>
          ))}
          {!plan.skills.some((s) => s.role === 'primary') ? <span className="faint">无（按通用写法）</span> : null}
        </span>
      </div>
      <div className="plan__row">
        <span className="plan__k">辅助</span>
        <span className="plan__v">
          {plan.skills.filter((s) => s.role === 'support').map((s) => (
            <span key={s.id} className="tag">
              {s.name}
              <span className="faint">· {SOURCE[s.source] ?? s.source}</span>
            </span>
          ))}
          {!plan.skills.some((s) => s.role === 'support') ? <span className="faint">无</span> : null}
        </span>
      </div>
      <div className="plan__row">
        <span className="plan__k">文风</span>
        <span className="plan__v">
          {plan.style.label}
          {plan.style.fromOverride ? <span className="faint">（本次覆盖）</span> : null}
          {plan.style.note ? <span className="faint">· {plan.style.note}</span> : null}
        </span>
      </div>
      <div className="plan__row">
        <span className="plan__k">去AI味</span>
        <span className="plan__v">
          {plan.humanize.label}
          {plan.humanize.note ? <span className="faint">· {plan.humanize.note}</span> : null}
        </span>
      </div>
      {plan.excluded.length ? (
        <div className="plan__row">
          <span className="plan__k">未使用</span>
          <ul className="plan__excluded">
            {plan.excluded.map((e) => (
              <li key={`${e.id}${e.code}`}>
                <Icon name="info" size={13} /> {e.reason}
              </li>
            ))}
          </ul>
        </div>
      ) : null}
      {plan.notes.length ? (
        <div className="plan__row">
          <span className="plan__k">说明</span>
          <span className="plan__v">{plan.notes.join('；')}</span>
        </div>
      ) : null}
      <div className="plan__row">
        <span className="plan__k">上下文</span>
        <ul className="plan__blocks">
          {context.blocks.map((b, i) => (
            <li key={`${b.label}${i}`}>
              <span className="ellipsis">{b.label}</span>
              <span className="faint nowrap">
                {b.skipped ? b.skipped : `${b.chars} 字${b.omitted ? `（截断 ${b.omitted}）` : ''}`}
                {b.required ? ' · 必需' : ''}
              </span>
            </li>
          ))}
        </ul>
      </div>
      <p className="hint">
        计划 {shortHash(plan.planHash)} · 共 {context.totalChars} 字上下文。{preview.note}
      </p>
    </div>
  );
}
