# 05 · Molan Paper Studio：设计令牌、布局与截图

源码：`frontend/src/styles/{tokens,base,components,layout,features}.css`。不引入 UI 框架；全部颜色、间距、字号走 CSS 变量，组件只引用令牌。

## 1. 设计原则（落到实现上）

- **书稿是主角**：编辑区用宋体系衬线字、18px、行高 1.9、行宽 42em（均可在「设置 → 外观」调整，写入 CSS 变量，不改布局结构）；界面文字用系统无衬线。
- **纸、墨、朱**：浅色背景 `#f8f7f4` 近似纸色；正文墨色 `#262522`；唯一强调色为朱红 `#a94432`，只用于主操作、焦点与当前项。
- **状态色只表达状态**：成功（绿）= 已保存/已定稿；待处理（琥珀）= 未保存/待审/过期；危险（红）= 冲突/失败；信息（蓝）= 运行中。状态徽标始终是「图标 + 文字」，不靠颜色单独传达。
- **一次只突出一个主操作**：产物卡的 `actions` 由服务端给出，`primary` 只有一个。

## 2. 令牌

| 类别 | 令牌 | 浅色 | 深色 |
|---|---|---|---|
| 背景 | `--bg / --surface / --subtle / --sunken` | `#f8f7f4 / #fff / #f0ede7 / #ebe7df` | `#1c1b19 / #242320 / #2c2a27 / #181716` |
| 文字 | `--text / --text-2 / --text-3` | `#262522 / #615f59 / #6c6962` | `#e9e6df / #b4b0a6 / #9d998f` |
| 边框 | `--border / --border-strong` | `#e4e0d8 / #d3cec3` | `#3a3834 / #4a4742` |
| 强调 | `--accent / --accent-ink / --accent-soft / --focus` | `#a94432 / #fff / #f6e9e5 / #a94432` | `#d8846f / #1c1b19 / #3a2723 / #d8846f` |
| 状态 | `--success / --pending / --danger / --info`（各带 `-soft`） | `#28734f / #8a5b09 / #b53535 / #3d5a80` | `#6cbf94 / #d9a548 / #e0807a / #8fb0d9` |
| 差异 | `--diff-add / --diff-del` | `#e3f1e8 / #f8e3e1` | `#1f3a2a / #42221f` |
| 间距 | `--sp-1 … --sp-7` | 4 / 8 / 12 / 16 / 24 / 32 / 48 px | 同 |
| 圆角 | `--r-1 / --r-2` | 8 / 12 px | 同 |
| 字体 | `--font-ui / --font-serif / --font-mono` | 系统中文无衬线 / 宋体系 / 等宽 | 同 |
| 字号 | `--fs-ui / --fs-small / --fs-title / --fs-manuscript` | 14 / 12.5 / 17 / 18 px | 同 |
| 书稿 | `--lh-manuscript / --measure` | 1.9 / 42em | 同 |
| 动效 | `--dur / --ease` | 160ms / cubic-bezier(.2,.6,.2,1) | `prefers-reduced-motion` 下归零 |

主题：默认跟随系统（`prefers-color-scheme`）；`data-theme="light|dark"` 为作者显式选择；`index.html` 内联脚本在首帧前设置，避免闪烁。对比度按 WCAG 2.x 公式逐对计算（文字色 × `--bg/--surface/--subtle`）：浅色最低为 `--text-3` on `--subtle` 4.69:1，深色最低为 `--text-3` on `--subtle` 5.03:1；状态徽标的「状态色 on 对应 -soft 底」均 ≥ 4.94:1；`--text` on `--bg` 浅 14.3:1 / 深 13.8:1。初版 `--text-3`（浅 3.3:1）、`--pending`、深色 `--accent` 不达 4.5:1，已在交付前调整。

## 3. 布局

```text
┌ rail ┐┌──────────────── 工作台（container: ws）────────────────┐
│ 书库 ││ 顶栏：书名 · 当前文档 · 保存状态 · 视图切换 · 书菜单          │
│ 技能 ││┌ 资料树 ┐┌──────── 编辑区（书稿） ────────┐┌ 助手 / 待审 / ┐│
│ 设置 │││ 设定   ││ 文档标题 · 字数 · 版本 · 操作  ││ 生产线 / 上下文││
│ 书源 │││ 细纲   ││ 书稿正文（衬线、限定行宽）      ││ 产物卡片        ││
│      │││ 正文   ││ 选区工具条：修改/去味/审稿      ││ 任务 + 技能 +   ││
│      │││ 正文待审││ 冲突/草稿恢复横幅              ││ 计划预览 + 输入 ││
└──────┘└────────────────────────────────────────────────────────────┘
```

- 三栏宽度可拖拽并记忆；使用 **容器查询**（以工作台宽度而非窗口宽度为准）：
  - `ws ≤ 1080px`：资料树收为左侧抽屉（覆盖层，`grid-column: 1 / -1`）；
  - `ws ≤ 760px`：助手面板收为右侧抽屉；
  - 窗口 `≤ 720px`：左侧导航变为底部栏，编辑区占满。
- 所有 5 个视口（1440×900、1280×800、1024×768、768×1024、390×844）× 浅/深色均通过「无横向溢出」检查（`scrollWidth == clientWidth`）。

## 4. 可访问性

- 全部交互元素可键盘到达；对话框焦点陷阱、`Esc` 关闭、关闭后焦点归还；菜单方向键导航。
- 状态区 `role="status"`/`aria-live="polite"`；错误 `role="alert"`；图标按钮均有 `aria-label`。
- 焦点环使用 `--focus`（2px 外描边），不被 `outline: none` 移除。
- 动效遵守 `prefers-reduced-motion`。
- **未做**：屏幕阅读器实机测试、中文输入法（IME）手动测试（编辑器对 `compositionstart/end` 做了保护，但未人工验证）。

## 5. 截图（`docs/refactor/screenshots/`，由 `e2e/run.mjs` 在本地 mock 环境自动生成）

| 文件 | 内容 |
|---|---|
| `library-{1440,1280,1024,768,390}-light.png`、`library-{1440,390}-dark.png` | 书库 |
| `workspace-{1440,1280,1024,768,390}-light.png`、`workspace-{1440,390}-dark.png` | 工作台 |
| `editor-saved.png` | 编辑 → Ctrl+S → 已保存 |
| `conflict.png` | 两个标签页编辑同一文档：冲突面板（不覆盖、可另存副本） |
| `composer-plan.png` | 发起前计划预览（主技能 / 文风 / 去味 / 上下文 / 阻塞项） |
| `artifact-fragment.png` / `artifact-fragment-saved.png` | 选区修改稿（片段产物）与精确替换后的状态 |
| `artifact-outline-confirmed.png` | 细纲产物：保存 → 作者确认 → 刷新后仍为「细纲已确认」 |
| `pipeline-after-draft.png` / `pending-reader.png` | 生产线起草后进入待审；待审阅读器（按 hash 定稿） |
| `skills.png`、`settings-channels.png`、`settings-book.png` | 技能库、渠道设置、作品默认 |
| `artifacts-dark.png`、`mobile-assistant.png`、`mobile-editor.png` | 深色产物卡、手机助手抽屉、手机编辑 |
| `artifact-base-changed.png` | 生成后原文被另一处修改：卡片「原文已变化，需重新比较」，主动作变为比较 |
| `artifact-conflict-destination.png` | 同名细纲已存在：不覆盖，卡片报冲突并弹出保存去向 |
| `skill-draft.png` | 技能工坊 AI 起草：技能草稿卡（已生成，尚未保存） |
| `autowrite.png`、`pending-review-on-demand.png` | 批量自动写作（单章模式）；待审阅读器手动「审稿当前版本」 |
| `library-long-title-390.png` | 窄屏长书名 |

交付卡片由共用部件组装（`features/artifacts/parts.tsx`）：CardShell（图标 / 标题 / 状态徽标）、ItemList（多文件逐项状态）、ArtifactPreview（折叠预览；片段改写显示按词差异 FragmentDiff）、SkillSummary（生成依据）、ReceiptDetails（交付回执）、ActionBar（一个主动作 + 至多两个次动作 + 菜单）。各类型的差异全部来自服务端视图（kind / scope / items / actions / state）。

截图中的模型输出全部来自 `mock://` 确定性渠道，仅用于展示界面与流程，不代表真实模型质量。
