# 01 · 迁移基线：现状、调用链与功能矩阵

> 基于检出版本 `ec2b5d1`（2026-09-29「技能库与书稿最终备份」）逐文件核对。所有结论附源码位置，未核实的不写。
> 结论口径：**已实现** / **需重构** / **需补齐** / **后端占位** / **当前无法验证**。

## 0. 基线事实（工程层）

| 项 | 实测 | 结论 |
|---|---|---|
| Rust 工作区 | 4 crate，`cargo test --workspace` 325 项全过（core 144+2、llm 36、server 143），fmt/clippy 干净 | 已实现 |
| 行数门禁 `tools/check_line_budget.sh` | Windows `core.autocrlf=true` 检出后 **14 条登记全部报 `integer expression expected` 被静默跳过**，仍打印 OK | 门禁失效 → 已修（剥 CR、非数字登记值判失败） |
| 前端源码 | 仓库**无** React 源、无 `package.json`、无 `index.html`；`web-recovery/bundle.js` 是**未打补丁**的主 chunk，EditorPane/SkillPlaza/Mobile*/tiptap 等懒加载 chunk 不在仓库 | 从干净检出**无法构建也无法启动旧 UI** → 需补齐 |
| 静态树 | `main.rs` 托管 `$MOLAN_ROOT/web`，仓库与部署包均不含 | 当前无法验证旧 UI |
| 注入脚本 | `glue.js`(3210 行)/`pipeline_ui.js`/`agent_ui.js` 依赖补丁包私有钩子 `window.__molanCurrentBookId` 等与 DOM 类名/文案查找 | 需重构（新入口不依赖） |
| 测试资产 | `verify.sh` 引用的 `../production-flow/ui-check.cjs`、`molan-work/` 截图与脚本均不在仓库 | 当前无法验证 |
| 模型 | 存在确定性 `mock://` 渠道（含 `[call:<工具>]` 工具调用协议），可做协议/流程验证 | 可用 |
| 磁盘 | 工作盘 D: 为 exFAT，不支持符号链接（pnpm 默认 linker 失败）→ 前端用 npm + `package-lock.json` | 记录 |

## 1. 关键调用链（实测）

```
UI(旧 bundle / glue / agent_ui) ──POST /ipc/:cmd {args,options}──▶ main.rs ipc_handler
   └─ handlers::dispatch (mod.rs, 160+ 臂) ──未命中──▶ stream::dispatch_stream
        ├─ chat_stream ─▶ chat.rs: selected_skills+auto_match ▶ effective_skills ▶ build_system
        │                 ▶ auto_book_context ▶ 模型 ▶ (allow_save) chat_save 拆章 ▶ write_ai_file_checked ▶ register_review_queue
        ├─ agent_turn ──▶ agent_loop.rs: system_prompt(仅书名/题材) ▶ 工具循环(12 工具) ▶ steps_json
        │                 └─ draft_chapter_body ─▶ chapter_service::draft_chapter（与 IPC draft_chapter 共享）
        ├─ draft_chapter ▶ chapter_service::draft_chapter：前置门(上章正式稿/细纲已确认/记忆/指纹) ▶ 生成 ▶ humanize ▶ 正文待审 ▶ 审稿
        ├─ auto_write_* ─▶ auto_write.rs：自有细纲/正文/审稿/去味/写入链；全自动直写「正文」绕过审批 saga
        └─ gen_scene_body ▶ decompose.rs：无技能解析、忽略 aiOff、裸 INSERT pending_chapter
```

**结论：手动单章与 Agent 工具共享 `chapter_service::draft_chapter`；聊天写章、自动写作、场景写作各有一套生成/写入/门禁。** 注释里的「统一」不成立。

## 2. 已确认的真实问题（按影响排序）

### 2.1 权限与状态可信度
1. **模型可自行确认细纲与定稿**：`confirm_chapter_outline` 不传 `expectedHash` 时绑定当前内容（agent_tools.rs:252-266）；`finalize_chapter_draft` 可直接批准入正文（agent_tools.rs:270）。用户动作不是前提。
2. **Agent 运行无重启对账**：遗留 `running` 行永远返回 `already_running`（agent_run.rs 无恢复；db.rs:64 只建表）。
3. **同会话并发不同 requestId 无防护**：`SESSION_TOKENS` 被覆盖，先结束的运行移除后者的会话别名（agent_loop.rs:75-81,239-241）。
4. **空输出记为 done**（agent_loop.rs:470-474）；`done.full` 累计全部轮次而落库只存最后一轮（chat.rs:115 vs agent_loop.rs:324）。
5. **轮数耗尽在模型调用前检查**：第 N 轮工具后模型拿不到收尾文本轮（agent_loop.rs:317-323）。
6. **usage 缺失按 0 计费**（agent_run.rs:177-197），预算可被绕过；`draft_chapter_body` 的模型用量不计入 run 预算。

### 2.2 技能与上下文
7. **Agent 路径完全不注入技能/文风/作品配置**，`get_effective_skills` 工具传空选择（agent_tools.rs:205,370-390）；`agent_turn` 忽略 `skills/skillSelection/task/contextFiles`。
8. **planHash 只是预览**：`plan_stage` 不落库、无运行引用；运行中每章重新读最新模板（auto_write.rs:666,803）。真正的防护是哈希**整张 skills 表**的 `input_fingerprint`，改任意技能都会拒绝在飞章节（continuity.rs:793-841）。
9. **UI 选的主技能可能不替代作品主技能**：替代依赖技能自身 `usage_mode=="primary"`，前端把 standalone 技能作为 `primarySkillId` 发送时两者都注入（stream/mod.rs:265-272）。名称自动匹配的技能算「显式」反而能压掉作品主技能。
10. **显式选择不适用时静默丢弃**，仅 info 日志（stream/mod.rs:257-264）。
11. **任务表复制三份**（skill_plan.rs:17 / chat_contract.rs:7 / agent_tools.rs:30），别名 chapter→body 只在 chat_contract 生效；`resolve_effective_skills` IPC 不认别名（旧 glue 查 `chapter` 显示「无」）。
12. **细纲双重注入**：`append_target_outline` 去重标记与 `auto_book_context` 的标题格式不一致（stage_context.rs:63 vs stream/mod.rs:1364）。
13. 文风卡被当技能选中时与文风通道重复注入；停用的 `style:<id>`/`humanize skill:<id>` 仍生效（auto_write.rs:2823-2833,3056-3071）。
14. `context_text` 截断（4000/文件、8000 总）不报告；上下文清单只记上下文字符串，不含系统提示/技能/历史，且 Agent 不写清单。

### 2.3 写入与交付
15. **前端拿不到可做 CAS 的版本**：`read_file` 只回字符串（缺失与空无法区分），`scan_tree` 只有 mtime/大小；`file_revision.content_hash` 存在但从不暴露或校验。
16. **无局部写入**：无插入/追加/选区替换；`saved_output` 明确拒绝 `section/op`（saved_output.rs:19-21）；`splice_once` 为死代码（chat.rs:1563-1602）。
17. **可无基线覆盖正式稿**：`apply_asset_update` 按 正文>细纲>设定>参考 优先级挑组后 `write_file` 覆盖，客户端内容、无基线、缺参也报成功（handlers/mod.rs:939-987）。`save_chat_output`/`save_capability_doc` 接受客户端内容。
18. **待审登记与写盘分属两把锁**：写盘成功但 `register_review_queue` 失败会留下「挡住重写但无法批准」的孤儿稿（chapter_service:372-405、auto_write.rs:1311-1376、chat.rs:922）。自动写作/场景用裸 `INSERT OR REPLACE pending_chapter` 重置 saga 列（auto_write.rs:1197,1251,1364；decompose.rs:807）。
19. **审稿结论不入库、文本变化后服务端不失效**。
20. 提案接受：CAS 写成功但索引失败时状态回滚为 pending，之后 CAS 永远失败（deepwrite.rs accept 路径）。
21. 版本快照非原子（`fs::write`）、同毫秒覆盖、无保留策略；阻塞文件 IO 与 `thread::sleep` 重试跑在 tokio worker 上。

### 2.4 交付卡片
22. 「已保存」散落在 `messages.result_json` 的 `saved/savedDocs/savedBookId/bookSetup.saved` 等形状；Agent 工件只存在 `steps_json`；没有统一产物模型，卡片状态由各组件自猜。

## 3. 功能与链路迁移矩阵

| 用户动作 | 旧入口 | IPC | 应用服务 | 技能/上下文 | 模型/工具 | 文本产物 | 文件/库回执 | 交付卡片 | 状态 → 本次处置 |
|---|---|---|---|---|---|---|---|---|---|
| 书库浏览/新建/删除/回收 | bundle librail | list_books, create_book, delete_book, list_trash, restore_trash | books.rs | — | — | — | books 行 | — | 已实现 → 新 UI 接线 |
| 导入 txt | bundle import | import_book | files::import_book | — | — | — | 非事务（半建书） | — | 需重构（记录，新 UI 接线并显示结果） |
| 导出 | glue→web_export | web_export | exports.rs | — | — | zip/txt/all | base64 | — | 已实现 → 新 UI 接线 |
| 打开/编辑/保存文档 | EditorPane(缺) | read_file, write_file | files::write_file | — | — | 全文 | 版本快照；**无基线** | — | 需重构 → `doc_read`(带 hash/revision) + `doc_write`(CAS) |
| 选区改写/插入/追加 | inline_chat(结果由前端拼) | inline_chat | chat.rs | 忽略 skillSelection | 模型 | 片段 | **无服务端局部写** | — | 需补齐 → DocumentWriteService `replace_range/insert/append` |
| 聊天 | dock | chat_stream | chat.rs | effective_skills+自动匹配 | 模型 | 消息 | — | — | 已实现 → 保留旧 IPC；新 UI 走统一任务运行 |
| Agent 任务 | agent_ui | agent_turn | agent_loop.rs | **无** | 12 工具 | steps_json | 各工具自写 | agent_ui 自猜 | 需重构 → 技能/上下文注入、作用域、产物模型 |
| 细纲生成/确认 | pipeline_ui / 工具 | gen_chapter_outline, confirm_outline | outline_confirm.rs | 部分 | 模型 | 细纲 | outline_confirm 回执 | — | 需重构 → 确认只能由用户动作触发 |
| 单章起草 | pipeline_ui | draft_chapter | chapter_service | effective_skills(body)+DW | 模型 | 正文待审 | pending_chapter+review | done 事件 | 已实现 → 收敛为 ChapterService 唯一实现 |
| 自动写作 | glue 面板 | auto_write_* | auto_write.rs | 无本次技能 | 模型 | 正文/待审 | 裸 INSERT；全自动绕 saga | 进度卡 | 需重构 → 待审登记统一；本次技能可传 |
| 待审批准/驳回 | glue 浮条/agent_ui | approve_chapter, reject_chapter, approve_all_pending | approval.rs saga | — | — | — | chapter_approved 回执 | — | 已实现（重入逻辑三份）→ 收敛 |
| 提案接受/拒绝 | glue DW 工作台 | dw_* | deepwrite.rs | — | — | 提案 | CAS(全串比较) | — | 已实现 → 接入产物卡+修复回滚 bug |
| 建书资料保存 | BookSetupCard | save_book_setup_selection | setup_destination.rs | — | — | 多文件 | 逐项回执 | 卡片 | 已实现 → 产物模型适配 |
| 消息结果保存 | savedoc 卡 | save_doc/save_section | saved_output.rs | — | — | doc/docs | result_json.saved | 卡片 | 已实现 → 产物模型适配（旧形状只读投影） |
| 技能库/工坊 | bundle | list/create/update/delete_skill, draft_skill… | handlers | — | 模型(草稿) | 技能 | skill_revision | — | 已实现（`write_skill_ref` 不升版本）→ 修 |
| 作品技能绑定 | bundle bookcfg | set_book_primary_skill, set_book_support_skills, set_book_style, set_book_humanize | books.rs settings | — | — | — | settings 键 | — | 已实现（不校验 task/id）→ 加校验 |
| 生效技能预览 | glue 写作面板 | resolve_effective_skills, plan_stage | stream/mod.rs, skill_plan.rs | — | — | — | 不落库 | — | 需重构 → SkillResolver 统一 + 冻结快照 |
| 模型/渠道 | bundle 设置 | get_settings, set_setting(s), set_channel_key, list_models_*, test_chat_ex | molan-llm | — | — | — | settings | — | 已实现 → 新 UI 接线 |
| 角色模型 | glue 写作面板 | get_agent_profiles, set_agent_profile | molan-llm | — | — | — | settings | — | 已实现（无 chat/editor 角色）→ 记录 |
| 书源 | bundle decompose | list_book_sources, search_books, fetch_book_catalog, fetch_chapter_texts | molan-sources | — | — | 参考文本 | — | — | 已实现 → 新 UI 接线 |
| 记忆/事实 | glue | memory_status, rebuild_memory, list_story_facts… | continuity/facts | — | 模型 | 记忆 | memory_job | — | 已实现 → 面板只读+重建 |
| 账号/支付/广场/云同步/授权 | bundle | account_*, payment_*, plaza_*, sync_*, license_status | 固定值/离线错误 | — | — | — | — | — | **后端占位 → 不迁移** |
| chapter_asset_footprint / rollback_chapter_assets / read_import_folder / import_book_zip / test_run_skill_script | bundle | 同名 | 假值或参数不匹配 | — | — | — | — | — | **后端占位/坏 → 不迁移，记录** |

## 4. 重构方向（确认后执行）

1. **新前端 `frontend/`**（React 19 + TS + Vite），构建产物由 Rust 单端口托管；新入口以 `<meta name="molan-app" content="paper-studio">` 标识，**不再注入** glue/pipeline_ui/agent_ui；旧静态树放回 `web/` 即自动走旧注入逻辑（回退）。
2. **molan-core 新增应用服务**：`task_kind`（任务 ID/别名/中文名/角色单一来源）、`skill_resolver`（SkillPlan + 冻结快照）、`doc_write`（DocumentWriteService：create/replace/append/insert/replace_range，hash CAS，两阶段写入账本，UTF-16 偏移）、`artifact`（产物/修订/交付回执与状态投影，兼容旧 result_json 形状）。
3. **Agent 运行时**：作用域化工具（确认/定稿从模型工具中移除，改为用户在卡片上执行）、按任务注入技能与上下文、重启对账、会话级并发防护、空输出拒绝、收尾文本轮、产物落库与 `artifact` 事件。
4. **章节服务收敛**：待审提交在同一把锁内完成「写盘 + 待审登记 + 状态」；自动写作/场景/聊天写章全部改走它；批准重入逻辑合并为一处。
5. **新增 IPC 全部为加法**（`app_info`、`doc_read`、`doc_write`、`task_preview`、`artifact_*`、`run_status`），旧命令语义不变；旧 `messages.result_json` 形状由产物适配器只读投影。
