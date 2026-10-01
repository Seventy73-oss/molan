/**
 * 技能工坊草稿（skill_draft 产物）的前端缓存。草稿属于全局技能库，不随作品切换；
 * 状态以服务端视图为准（保存后技能被改 → 服务端投影为「已失效」）。
 */
import { create } from 'zustand';
import { api } from '../../lib/api';
import type { ArtifactView } from '../../lib/contracts';

interface DraftsState {
  drafts: ArtifactView[] | null;
  /** 保存为技能后递增：技能库据此刷新列表。 */
  savedVersion: number;
  load: () => Promise<void>;
  upsert: (v: ArtifactView) => void;
  markSaved: () => void;
}

export const useSkillDrafts = create<DraftsState>((set, get) => ({
  drafts: null,
  savedVersion: 0,
  load: async () => set({ drafts: await api.skillDrafts() }),
  upsert: (v) => {
    const cur = get().drafts ?? [];
    const rest = cur.filter((x) => x.id !== v.id);
    set({ drafts: v.lifecycle === 'discarded' ? rest : [v, ...rest] });
  },
  markSaved: () => set({ savedVersion: get().savedVersion + 1 }),
}));
