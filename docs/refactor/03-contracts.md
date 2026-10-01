# 03 · 契约：任务、技能计划、上下文、产物、写入回执与事件

契约版本：`app_info.contract = 2`。规范样例是 `contracts/fixtures/*.json`，由 Rust 测试（`handlers/v2_tests.rs`）从真实 IPC 输出生成并归一化（UUID → `<id>`、64 位 hex → `<sha256>`、时间戳 → 0），前端 `src/lib/contracts.test.ts` 用运行时校验器逐个解析。改动任一侧都会让对方测试失败。

```bash
MOLAN_UPDATE_CONTRACTS=1 cargo test -p molan-server v2::tests
```

所有 IPC 统一为 `POST /ipc/:cmd`，请求体为 JSON 参数对象；非流式返回 JSON，流式返回 NDJSON（见 §7）。

## 1. 新增 IPC（全部为加法）

| 命令 | 参数 | 返回 |
|---|---|---|
| `app_info` | — | `{app, ui, contract, version, authRequired, tasks[], writeOps[]}` |
| `doc_read` | `bookId, group, name` | `{exists, content, hash, revision, utf16Len, locked, aiOff}` |
| `doc_write` | WritePlan（§4） | WriteReceipt + `durationMs`（服务端写入耗时；失败也返回回执，不抛异常） |
| `doc_history` | `bookId, group, name, limit?(30)` | WriteReceipt[]（新→旧） |
| `task_preview` | `bookId, task, target?, skillSelection?, contextFiles?` | `{task, taskLabel, role, plan, context, recommend, note}`；不创建运行、不写书稿；计划按 planHash 内容寻址写入快照表（`INSERT OR IGNORE`，重复无副作用） |
| `skill_recommend` | `bookId, task` | 推荐列表（按任务适用性与题材的确定性规则，不调用模型） |
| `artifact_list` | `bookId, sessionId?, limit?` | ArtifactView[]（给 sessionId 时含旧数据投影） |
| `artifact_get` | `bookId, artifactId` | ArtifactView（跨书访问拒绝） |
| `artifact_revise` | `bookId, artifactId, baseRev, content, note?, items?` | ArtifactView（新修订 `origin=user_edit`；`REV_CONFLICT` 防并发覆盖） |
| `artifact_deliver` | Deliver（§5.3） | `{delivery, artifact}` |
| `read_version` | `bookId, group, name, ts` | `{ts, content}`（版本快照全文） |
| `skill_revisions` | `id` | 技能历史版本 |
| `book_skill_bindings` | `bookId` | `{<task>: {primary, supports[]}}` |
| `channel_test` | `id, model?` | 用服务端已保存密钥做一次最小连通测试（作者手动触发；前端永不回显密钥） |
| `run_status` | `bookId, sessionId, requestId?` | RunState（§6）或 null；会话不属于该书则拒绝 |
| `review_pending` | `bookId, ch, skillSelection?` | `{review, summary}`：按「审稿」子阶段计划审当前待审稿，结论绑定其 hash；不改稿、不定稿；无待审稿 / 少于 300 字报错 |
| `skill_draft` | `name, description?, task?, usage?` | ArtifactView（`kind=skill_draft`，`bookId` 为空：属于全局技能库） |
| `skill_drafts` | `limit?` | ArtifactView[]（不含已放弃） |
| `skill_draft_revise` | `artifactId, baseRev, content, note?` | ArtifactView（新修订；`REV_CONFLICT` 防并发） |
| `skill_draft_save` | `artifactId, idempotencyKey, rev?, name?, targets?` | `{delivery{ok, result{skillId, name, targets}}, artifact}`；同键重放不重复建技能 |
| `skill_draft_discard` | `artifactId` | ArtifactView |

旧命令的加法字段：`list_pending_chapters[].review = {state: current|stale|none, ok, flagged, issues[], note, bodyHash, planHash, source, createdAt}`；`approve_chapter` 返回 `writeId`；`chat_stream` 的 `meta` 增加 `contextFiles[{file, status: ok|truncated|skipped|missing, chars?, used?, reason?}]`；`draft_chapter` 回执增加 `stagePlans{humanize, review}` 与 `promptChars`。

`agent_turn` 新增可选参数：`task`、`target`、`skillSelection`、`contextFiles`、`mode`（`agent`/`direct`）。不传 `task` 时保持旧助手语义（工具集去掉确认/定稿）。`draft_chapter` 新增可选 `skillSelection`、`sessionId`（传入时把待审稿收编为产物卡）。

## 2. Task

`id ∈ chat | plot | outline | body | revise | review | humanize | summary | distill`，别名（`chapter`→`body`，中文名等）由 `TaskKind::parse` 归一。每项给出 `label`、`role`（使用哪个模型角色）、`artifactKind`、`rewritesTarget`（是否允许覆盖/替换目标；不允许的任务只能新建/追加）。

`target`：`{ch?, group?, name?, baseHash?, start?, end?, selectionText?, text?}`。`start/end` 一律是 **UTF-16 偏移**（与浏览器 `selectionStart` 一致），服务端换算字节，不得落在代理对中间。

## 3. SkillPlan（`plan` 事件 / `task_preview.plan`）

```text
skillSelection = { primarySkillId?, supportSkillIds?[], styleKey?, humanize? }   // 只影响本次
plan = { bookId, task, taskLabel, genre, planHash,
         skills[{id, name, role: primary|support, source: explicit|book_primary|book_support|legacy|auto_match,
                 rev, contentHash, templateChars, targets[], usageMode, enabled, builtinKey, kind, origin}],
         excluded[{id, name, source, code, reason}],
         style{key, label, source, chars, contentHash, fromOverride, note},
         humanize{method, label, chars, contentHash, fromOverride, note},
         notes[], humanizeOverride }
```

排除码：`NOT_FOUND / DISABLED / EMPTY_TEMPLATE / NOT_APPLICABLE / STYLE_CHANNEL / MULTI_PRIMARY / REPLACED / AUTO_MATCH_NO_REPLACE`。公开视图不含模板全文（只给 `templateChars`）；完整计划按 `planHash` 冻结在 `skill_plan_snapshot`。

## 4. WritePlan / WriteReceipt

```text
WritePlan    = { bookId, group, name, op: create|replace|append|insert|replace_range,
                 content, baseHash?, start?, end?, expected?(原选区文本),
                 idempotencyKey?, source? }
WriteReceipt = { writeId, idempotencyKey, bookId, group, name, op, actor: user|ai,
                 commit: committed|noop|conflict|failed,
                 beforeHash, afterHash, revision, chars,
                 index: ok|failed|skipped, indexError,
                 error{code, message, currentHash}?,
                 replayed, recovered, source, ts }
```

其他写入服务的回执与上面同形状，`idempotencyKey` 为 `ext:<writeId>`，`source.service` 取值：`chapter_commit.submit_pending`、`chapter_commit.review_write`、`chapter_commit.approve`（`actor=user`）、`chapter_service.humanize`、`decompose.scene_append`、`auto_write.full_auto`、`deepwrite.accept_proposal`、`book_setup`。

`commit=committed` + `index=failed` 表示**文件已写入、派生索引失败**——前端显示「已保存，但索引未更新」，不得当成失败重写。错误码见 02-architecture §6。

## 5. Artifact

### 5.1 视图

```text
ArtifactView = { id, bookId, sessionId, messageId, runId, kind, kindLabel, task, taskLabel,
                 title, summary, scope: document|fragment, format, origin: model|tool|user_edit|legacy,
                 rev, content, contentHash, chars, truncated, target, provenance,
                 lifecycle, state, stateLabel,      // state 由服务端投影
                 item, items[],                      // 多文件产物逐项状态
                 deliveries[], actions[{id, label, primary}], legacy, createdAt, updatedAt }
```

`kind`：`plot_note / outline_draft / body_draft / revision / review_report / humanize_rewrite / summary / distill_note / skill_draft / book_setup / proposal / multi_file`（旧数据中的单文档结果投影为 `multi_file` 或按目标推断的类型）。

### 5.2 状态投影（`artifact_view::item_state`）

| state | 判定 |
|---|---|
| generating / interrupted / failed / discarded | 产物生命周期 |
| saved | 有成功的 save 交付，且目标文件当前 hash = 交付 afterHash |
| stale | 交付过，但目标文件已被其他写入改变 |
| conflict / partial | 交付回执为冲突；多文件中部分成功 |
| base_changed | 片段产物的源文件 hash 已不等于生成时基线 |
| pending_review | 提交待审成功，且待审队列仍是该 hash |
| confirmed | 细纲确认回执绑定的 hash = 保存时 afterHash |
| approved / rejected | 批准回执 / 退回记录 |
| generated | 以上皆无 |

### 5.3 交付（`artifact_deliver`）

```text
Deliver = { artifactId, bookId, rev, item?, action, idempotencyKey,
            group?, name?, op?, baseHash?, start?, end?, expected?, ch? }
action ∈ save | submit_pending | confirm_outline | approve | reject | discard
```

- `save` → DocumentWriteService（`actor=ai`，幂等键 `artifact:<id>:<key>`）；片段产物不得整篇 `replace`；`rewritesTarget=false` 的任务只能 create/append。
- `submit_pending` → `chapter_commit::submit_pending`（写盘与登记同锁）。
- `confirm_outline` → 绑定该产物已保存的 afterHash；文件之后被改则拒绝。
- `approve` / `reject` → 以提交待审时的 hash 为 expected，走唯一的 `chapter_commit::approve/reject`。
- 每次交付写 `artifact_delivery` 一行；同 `(artifactId, idempotencyKey)` 重放返回原记录。
- 技能草稿走 `skill_draft_save`（服务端经 `create_from_draft` 建技能并记 `save_skill` 交付）；状态：技能模板仍等于保存时 hash → `saved`「已保存为技能「x」」，被改或删除 → `stale`。动作：`save_skill / open_skill / edit / copy / discard`（不提供「另存为文档」）。
- 定稿后的 `approved` 措辞附记忆同步状态（按正文 hash 绑定的 memory_job）：「，记忆更新排队中 / 记忆已更新 / 记忆更新失败（可在生产线重试）」。

## 6. RunState（`run_status` / `agent_session_state.state`）

```text
{ runId, sessionId, requestId, task, mode, model, status(原值), state, stateLabel, live,
  toolRound, maxToolRounds, usedTokens, budgetTokens, usageEstimated, planHash, manifestId,
  code, error, createdAt, updatedAt }
state ∈ running | waiting | interrupted | completed | failed | budget_exhausted
metrics = { totalMs, firstTokenMs|null, toolMs, cacheHits, retries, usageEstimated,
            modelCalls[{firstTokenMs, firstTextMs, totalMs, promptChars, outputChars,
                        outcome: toolCalls|ok|empty|error: …, usage|null, usageSource: upstream|estimated}],
            tools[{name, ms, cached, parallel, ok}] }   // 旧运行为 {}
```

`live` 表示本进程在飞登记中确有该运行；`status=running` 而 `live=false` 只可能出现在重启对账之前。

## 7. NDJSON 事件

帧：`{"ch":"<channel>","e":{...}}`（事件）、`{"r":<result>}`（结束）、`{"err":{"message"}}`（失败）；`ch="__hb__"` 为心跳（`progress chars=-1`），前端忽略。解析器容忍 UTF-8 跨块、CRLF、末行无换行，坏帧计数而不中断。

`agent_turn` 事件（按出现顺序）：

| type | 字段 | 说明 |
|---|---|---|
| `meta` | task, taskLabel, model, mode, runId, planHash, target | 运行开始 |
| `plan` | plan（§3 公开视图） | 已冻结的技能计划 |
| `context` | manifestId, blocks[{label, source, chars, hash?, truncated, omitted, required}] | 上下文清单 |
| `delta` / `reasoning` | text | 模型输出 / 思考（只计字数） |
| `tool` | callId, name, status: running/ok/error, summary?, artifact? | 工具调用（按 callId 配对） |
| `artifact` | artifact（ArtifactView） | 产物生成或更新 |
| `notice` | code（如 `ROUNDS_EXHAUSTED`）, message | 非错误提示 |
| `error` | code, message, blockers?, runId? | `SESSION_BUSY / CONTEXT_BLOCKED / BUDGET_EXHAUSTED / RETRY / EMPTY_OUTPUT / TOOLS_UNSUPPORTED` |
| `interrupted` | reason, partialChars | 取消或中断；已生成部分保存为 interrupted 产物 |
| `done` | status, runId, metrics（§6）, … | `done / error / interrupted / budget_exhausted / tools_unsupported / session_busy / already_running` |

`draft_chapter` 事件：`step(index,title)`、`meta`、`plan`、`progress(chars)`、`delta`、`review`、`artifact`（传 sessionId 时）、`done(receipt)`、`error`。回执含 `timings{precheckMs, generateMs, humanizeMs, writeMs, reviewMs, totalMs}`。

同一轮多个工具调用时，`tool` 的 running 事件按组发出（连续只读调用一组），结果事件按模型给出的原顺序发出。`artifact_deliver` 返回值附 `ms`（服务端交付耗时）。

## 8. 旧数据适配

| 旧数据 | 适配方式 |
|---|---|
| `messages.result_json.doc/docs` | `artifact_legacy` 只读投影为 `doc` 产物卡，动作 `legacy_save_doc`（走 doc_write，需确认去向） |
| `result_json.bookSetup` | `book_setup` 卡，动作 `legacy_book_setup`（逐项预览） |
| Agent `steps_json` 中的细纲/正文/提案工件 | 对应卡片，动作 `legacy_confirm_outline / legacy_approve / legacy_reject / legacy_accept_proposal / legacy_reject_proposal`，状态同样按磁盘与回执复核 |
| 已有 `artifact` 行的消息 | 不再重复投影 |
| `effective_skills(task, skills)` | 兼容包装，内部走 `skill_resolver::resolve` |
| 旧 IPC（chat_stream、chat_save、approve_chapter、auto_write_*、set_book_*_skill…） | 参数与返回结构不变；`approve_chapter` 新增可选 `expectedHash`；技能绑定 task 统一归一（`chapter`→`body`） |
