import { memo, useState } from 'react';
import { Icon } from '../../components/Icon';
import type { ArtifactAction, ArtifactView } from '../../lib/contracts';
import { useWs } from '../../state/workspace';
import { ArtifactDialogs, type DialogKind } from './ArtifactDialogs';
import { runAction } from './actions';
import { ActionBar, ArtifactPreview, CardShell, ItemList, ReceiptDetails, SkillSummary } from './parts';

export { actionLabel } from './parts';

/**
 * 交付卡片：由共用部件组装（见 parts.tsx）。各类型产物的差异全部来自服务端视图：
 * kind / scope / items / actions / state，而不是在组件里按 JSON 形状猜测。
 */
export const ArtifactCard = memo(function ArtifactCard({ a }: { a: ArtifactView }) {
  const book = useWs((s) => s.book);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState(false);
  const [dialog, setDialog] = useState<DialogKind | null>(null);

  const doAction = async (act: ArtifactAction) => {
    if (busy) return; // 执行中防重入
    setError(null);
    const needDialog = await runAction(a, act.id, {
      setBusy: (b) => setBusy(b ? act.id : null),
      setError,
    });
    if (needDialog) setDialog(needDialog);
  };

  return (
    <CardShell a={a} book={a.kind === 'skill_draft' ? null : book?.title}>
      <ItemList a={a} />
      <ArtifactPreview a={a} expanded={expanded} onToggle={() => setExpanded((e) => !e)} onRead={() => setDialog('read')} />
      <SkillSummary a={a} />
      <ReceiptDetails a={a} />
      {error ? (
        <div className="notice notice--bad" role="alert">
          <Icon name="alert" size={16} />
          <span className="grow break">{error}</span>
        </div>
      ) : null}
      <ActionBar actions={a.actions} busy={busy} onAction={(x) => void doAction(x)} />
      {dialog ? <ArtifactDialogs a={a} kind={dialog} onClose={() => setDialog(null)} /> : null}
    </CardShell>
  );
});
