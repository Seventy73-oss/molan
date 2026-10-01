/**
 * 产物卡片动作 → 服务端交付。规则：
 * - 每次点击一个幂等键；执行中由卡片禁用按钮防重入；
 * - 成功文案以服务端回执为准（写到哪里、提交到哪里），不说「已全部保存」之类无证据的话；
 * - 失败保留在卡片上（不只是 toast），作者可重试或另存。
 */
import { toast } from '../../components/Toasts';
import { api } from '../../lib/api';
import type { ArtifactView, DeliverResult } from '../../lib/contracts';
import { errorText, newRequestId } from '../../lib/ipc';
import { navigate, navigateFrom } from '../../app/router';
import { useWs } from '../../state/workspace';
import { useSkillDrafts } from '../skills/drafts';
import type { DialogKind } from './ArtifactDialogs';

interface Ctx {
  setBusy: (b: boolean) => void;
  setError: (e: string | null) => void;
}

async function afterDelivery(r: DeliverResult) {
  const ws = useWs.getState();
  ws.upsertArtifact(r.artifact);
  void ws.refreshTree();
  void ws.refreshPendingCount();
  const doc = ws.doc;
  const loc = r.delivery.receipt ?? (r.delivery.result as { group?: string; name?: string } | undefined);
  if (doc && loc && doc.group === loc.group && doc.name === loc.name && doc.save === 'clean') {
    await ws.reloadDoc().catch(() => undefined);
  }
}

function describe(r: DeliverResult, bookTitle?: string): string {
  const d = r.delivery;
  const book = bookTitle ? `《${bookTitle}》` : '';
  if (d.action === 'save' && d.receipt) {
    const rc = d.receipt;
    const where = `${book}/${rc.group}/${rc.name}`;
    const op = { create: '新建', replace: '替换全文', append: '追加到', insert: '插入到', replace_range: '替换选区于' }[rc.op] ?? rc.op;
    if (rc.commit === 'noop') return `内容与 ${where} 相同，未改动`;
    if (rc.index === 'failed') return `已${op} ${where}，但索引登记失败（文件完好）`;
    return `已${op} ${where}`;
  }
  const res = (d.result ?? {}) as Record<string, unknown>;
  if (d.action === 'submit_pending') return `已提交待审：正文待审/${String(res.name ?? '')}（尚未定稿）`;
  if (d.action === 'confirm_outline') return `细纲已确认：${String(res.name ?? '')}`;
  if (d.action === 'approve') return res.alreadyApproved ? `该章此前已定稿：正文/${String(res.finalName ?? '')}` : `定稿完成：正文/${String(res.finalName ?? '')}，记忆更新排队中`;
  if (d.action === 'reject') return '已驳回（待审稿已移到回收站，可恢复）';
  if (d.action === 'discard') return '已放弃该产物';
  return '已完成';
}

function failure(r: DeliverResult): string {
  const d = r.delivery;
  if (d.receipt?.error) return d.receipt.error.message;
  const res = d.result as { error?: string } | undefined;
  return res?.error ?? `${d.action} 未成功（${d.status}）`;
}

export async function deliver(a: ArtifactView, p: Record<string, unknown>, ctx: Ctx): Promise<DeliverResult | null> {
  const ws = useWs.getState();
  if (!ws.bookId) return null;
  ctx.setBusy(true);
  try {
    const r = await api.deliver({ bookId: ws.bookId, artifactId: a.id, idempotencyKey: newRequestId('deliver'), ...(p as { action: string }) });
    await afterDelivery(r);
    if (r.delivery.ok) {
      toast.ok(describe(r, ws.book?.title));
      ctx.setError(null);
    } else {
      ctx.setError(failure(r));
    }
    return r;
  } catch (e) {
    ctx.setError(errorText(e));
    return null;
  } finally {
    ctx.setBusy(false);
  }
}

async function fullContent(a: ArtifactView): Promise<string> {
  const ws = useWs.getState();
  if (!a.truncated || a.legacy || !ws.bookId) return a.content;
  return (await api.artifact(ws.bookId, a.id)).content;
}

async function legacy(a: ArtifactView, fn: () => Promise<unknown>, ok: string, ctx: Ctx) {
  ctx.setBusy(true);
  try {
    await fn();
    toast.ok(ok);
    await useWs.getState().reloadSession();
    void useWs.getState().refreshTree();
    void useWs.getState().refreshPendingCount();
  } catch (e) {
    ctx.setError(errorText(e));
  } finally {
    ctx.setBusy(false);
  }
  void a;
}

/** 技能草稿（全局技能库，不需要打开作品）：保存为技能 / 查看技能 / 放弃 / 编辑 / 复制。 */
async function runSkillDraft(a: ArtifactView, id: string, ctx: Ctx): Promise<DialogKind | null> {
  const drafts = useSkillDrafts.getState();
  const skillId = () => {
    const d = [...a.deliveries].reverse().find((x) => x.action === 'save_skill' && x.status === 'committed');
    return (d?.detail as { skillId?: string } | undefined)?.skillId;
  };
  try {
    switch (id) {
      case 'save_skill': {
        ctx.setBusy(true);
        const from = location.hash;
        const r = await api.skillDraftSave(a.id, newRequestId('skill'));
        drafts.upsert(r.artifact);
        drafts.markSaved();
        toast.ok(r.artifact.summary || '已保存为技能');
        const sid = r.delivery.result?.skillId;
        if (sid) navigateFrom(from, { name: 'skills', skillId: sid });
        return null;
      }
      case 'open_skill': {
        const sid = skillId();
        if (sid) navigate({ name: 'skills', skillId: sid });
        return null;
      }
      case 'discard':
        ctx.setBusy(true);
        drafts.upsert(await api.skillDraftDiscard(a.id));
        toast.ok('已放弃该技能草稿');
        return null;
      case 'edit':
        return 'edit';
      case 'copy':
        await navigator.clipboard.writeText(a.content);
        toast.ok('已复制模板');
        return null;
      default:
        ctx.setError(`技能草稿不支持该动作：${id}`);
        return null;
    }
  } catch (e) {
    ctx.setError(errorText(e));
    return null;
  } finally {
    ctx.setBusy(false);
  }
}

/** 执行动作；需要先弹对话框的动作返回对话框类型。 */
export async function runAction(a: ArtifactView, id: string, ctx: Ctx): Promise<DialogKind | null> {
  if (a.kind === 'skill_draft') return runSkillDraft(a, id, ctx);
  const ws = useWs.getState();
  const bookId = ws.bookId;
  if (!bookId) return null;
  const t = a.target ?? {};
  const ref = (a.legacyRef ?? {}) as { artifact?: Record<string, unknown> };
  switch (id) {
    case 'save_outline': {
      const ch = t.ch;
      if (!ch) return 'destination';
      const r = await deliver(a, { action: 'save', group: '细纲', name: `细纲_第${ch}章.md`, op: 'create' }, ctx);
      return r && !r.delivery.ok && r.delivery.receipt?.error?.code === 'TARGET_EXISTS' ? 'destination' : null;
    }
    case 'save_as':
      return 'destination';
    case 'submit_pending':
      await deliver(a, { action: 'submit_pending', ch: t.ch }, ctx);
      return null;
    case 'confirm_outline':
    case 'approve':
    case 'reject':
    case 'discard':
      await deliver(a, { action: id }, ctx);
      return null;
    case 'apply_selection':
      await deliver(a, { action: 'save', op: 'replace_range', group: t.group, name: t.name, baseHash: t.baseHash, start: t.start, end: t.end, expected: t.selectionText }, ctx);
      return null;
    case 'insert_after':
      await deliver(a, { action: 'save', op: 'insert', group: t.group, name: t.name, baseHash: t.baseHash, start: t.end }, ctx);
      return null;
    case 'apply_replace':
    case 'compare':
      return id === 'apply_replace' ? 'apply' : 'compare';
    case 'edit':
      return 'edit';
    case 'open_file': {
      const last = [...a.deliveries].reverse().find((d) => d.status === 'committed' && d.name);
      const loc = last ? { group: last.group, name: last.name } : t.group && t.name ? { group: t.group, name: t.name } : null;
      const item = a.items?.find((x) => x.location?.name);
      const target = loc ?? (item?.location ? { group: item.location.group ?? '', name: item.location.name ?? '' } : null);
      if (target) await ws.openDoc(target.group, target.name);
      return null;
    }
    case 'draft_body':
      ws.setComposer({ task: 'body', target: { ch: t.ch, label: `第${t.ch}章` } });
      ws.setPanel('assistant');
      return null;
    case 'copy': {
      try {
        await navigator.clipboard.writeText(await fullContent(a));
        toast.ok('已复制全文');
      } catch {
        ctx.setError('浏览器不允许写入剪贴板，请展开全文手动复制');
      }
      return null;
    }
    case 'stop':
      await ws.stop();
      return null;
    // ---- 旧形状卡片（服务端仍从持久化消息取内容） ----
    case 'legacy_save_doc':
      await legacy(a, () => api.legacySaveDoc(bookId, a.messageId), '已按原卡片保存到书稿', ctx);
      return null;
    case 'legacy_book_setup':
      return 'bookSetup';
    case 'legacy_confirm_outline': {
      const art = ref.artifact ?? {};
      await legacy(a, () => api.confirmOutline(bookId, Number(art.ch), String(art.hash ?? '')), '细纲已确认', ctx);
      return null;
    }
    case 'legacy_approve': {
      const art = ref.artifact ?? {};
      await legacy(a, () => api.approve(bookId, String(art.name ?? ''), String(art.hash ?? '') || undefined), '定稿完成，记忆更新排队中', ctx);
      return null;
    }
    case 'legacy_reject': {
      const art = ref.artifact ?? {};
      await legacy(a, () => api.reject(bookId, Number(art.ch), String(art.name ?? '')), '已驳回（可在回收站恢复）', ctx);
      return null;
    }
    case 'legacy_accept_proposal': {
      const art = ref.artifact ?? {};
      return (await legacy(a, () => api.acceptProposal(bookId, String(art.proposalId ?? '')), '提案已接受并写入', ctx), null);
    }
    case 'legacy_reject_proposal': {
      const art = ref.artifact ?? {};
      await legacy(a, () => api.rejectProposal(bookId, String(art.proposalId ?? ''), '作者拒绝'), '提案已拒绝', ctx);
      return null;
    }
    default:
      ctx.setError(`未知动作：${id}`);
      return null;
  }
}
