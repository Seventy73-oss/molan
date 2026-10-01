import { Icon } from '../../components/Icon';
import { shortHash } from '../../lib/text';
import { useWs } from '../../state/workspace';

/** 本次运行实际引用的资料与技能（来自服务端冻结的计划与上下文清单），不展示密钥或整段私密书稿。 */
export function ContextPanel() {
  const run = useWs((s) => s.run);
  const remote = useWs((s) => s.remoteRun);
  if (!run?.plan && !run?.contextBlocks) {
    return (
      <div className="panel-body">
        <div className="empty">
          <Icon name="layers" size={28} />
          <p className="empty__title">还没有本次运行的上下文记录</p>
          <p className="small">发起任务后，这里会列出实际注入的技能版本、文风、去味方式和每一块资料（含截断与跳过原因）。</p>
          {remote?.planHash ? <p className="small faint">最近一次运行计划：{shortHash(remote.planHash)}</p> : null}
        </div>
      </div>
    );
  }
  const plan = run.plan;
  return (
    <div className="panel-body context-panel">
      <section className="stack stack--tight">
        <span className="section-title">运行</span>
        <dl className="kv">
          <dt>任务</dt>
          <dd>{run.taskLabel}</dd>
          <dt>模型</dt>
          <dd>{run.model ?? '—'}</dd>
          <dt>模式</dt>
          <dd>{run.mode === 'direct' ? '直接生成（无工具）' : 'Agent（作用域内工具）'}</dd>
          <dt>计划</dt>
          <dd className="mono">{shortHash(run.planHash)}</dd>
          {run.usedTokens != null ? (
            <>
              <dt>用量</dt>
              <dd>{run.usedTokens.toLocaleString()} tokens{remote?.usageEstimated ? '（含估算）' : ''}</dd>
            </>
          ) : null}
        </dl>
      </section>
      {plan ? (
        <section className="stack stack--tight">
          <span className="section-title">技能（冻结版本）</span>
          {plan.skills.length === 0 ? <span className="small faint">无</span> : null}
          {plan.skills.map((s) => (
            <div key={s.id} className="ctx-row">
              <span className="tag">{s.role === 'primary' ? '主' : '辅'}</span>
              <span className="grow ellipsis">{s.name}</span>
              <span className="faint small nowrap">r{s.rev} · {s.templateChars} 字</span>
            </div>
          ))}
          <div className="ctx-row">
            <span className="tag">文风</span>
            <span className="grow ellipsis">{plan.style.label}</span>
            <span className="faint small">{plan.style.chars} 字</span>
          </div>
          <div className="ctx-row">
            <span className="tag">去味</span>
            <span className="grow ellipsis">{plan.humanize.label}</span>
          </div>
          {plan.excluded.map((e) => (
            <div key={e.id + e.code} className="ctx-row small muted">
              <Icon name="info" size={13} />
              <span className="grow">{e.reason}</span>
            </div>
          ))}
        </section>
      ) : null}
      {run.contextBlocks ? (
        <section className="stack stack--tight">
          <span className="section-title">资料块（{run.contextBlocks.length}）</span>
          {run.contextBlocks.map((b, i) => (
            <div key={`${b.label}${i}`} className="ctx-row">
              <Icon name={b.skipped ? 'eyeOff' : b.truncated ? 'scissors' : 'file'} size={14} />
              <span className="grow ellipsis" title={b.label}>{b.label}</span>
              <span className="faint small nowrap">
                {b.skipped ?? `${b.chars} 字`}
                {b.omitted ? ` · 省略 ${b.omitted}` : ''}
              </span>
            </div>
          ))}
        </section>
      ) : null}
    </div>
  );
}
