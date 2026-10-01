# 06 · 构建、运行、回退与生产迁移（待办）

> 本次交付**未**修改生产数据、**未**部署、**未**提交或推送。以下生产步骤只是待办清单，需维护者确认后执行。

## 1. 依赖

| 组件 | 版本 / 锁文件 |
|---|---|
| Rust | stable（workspace `Cargo.lock`）；Windows 需 MSVC 工具链 |
| Node | ≥ 20（本机验证 22.x）；`frontend/package-lock.json`（npm，`npm audit` 0 漏洞） |
| 浏览器测试 | `playwright-core` 驱动**本机已安装的 Edge**（`channel: 'msedge'`），不下载浏览器 |

pnpm 在 exFAT 盘上无法建立符号链接，故使用 npm。

## 2. 构建与本地运行

```bash
cargo build --release -p molan-server
```

```bash
npm --prefix frontend ci
```

```bash
npm --prefix frontend run build
```

启动（只绑定 127.0.0.1；数据目录请用**全新**目录，不要指向生产数据）：

```bash
MOLAN_ROOT=/path/to/dev-root MOLAN_WEB_DIR=frontend/dist PORT=17381 target/release/molan-server
```

| 环境变量 | 作用 |
|---|---|
| `MOLAN_ROOT` | 数据根目录（`data/`、`books/`、版本快照）。默认当前目录 |
| `MOLAN_WEB_DIR` | 静态资源目录：绝对路径或相对 `MOLAN_ROOT`；默认 `<root>/web` |
| `PORT` / `BIND` | 默认 17381 / 127.0.0.1；非 loopback 必须设置 `MOLAN_AUTH_TOKEN`，否则拒绝启动 |
| `MOLAN_AUTH_TOKEN` | 开启 Cookie 登录鉴权 |

开发模式（Vite 热更新，代理 `/ipc /auth /login /health` 到后端）：

```bash
MOLAN_BACKEND=http://127.0.0.1:17381 npm --prefix frontend run dev
```

演示数据（只接受 127.0.0.1 地址；导入 52 个技能、设置 `mock://` 渠道、创建《青岚纪（演示）》）：

```bash
node tools/dev-seed.mjs http://127.0.0.1:17381
```

## 3. 验证

```bash
sh verify.sh
```

依次执行：`cargo fmt --check` → `clippy -D warnings` → `cargo test --workspace` → 旧 glue `node --check` → 行数预算（只减不增）→ 内联脚本安全检查 → 前端 `typecheck / test / build` → 检查 `dist/index.html` 带 `paper-studio` 标记。

加上浏览器端到端（全新临时数据目录 + 随机端口 + mock 渠道，跑完停服）：

```bash
MOLAN_E2E=1 sh verify.sh
```

或单独运行（需已 `cargo build -p molan-server` 与 `npm run build`）：

```bash
node frontend/e2e/local.mjs
```

`local.mjs` 把服务端日志写到临时数据目录的 `server.log`，结束时打印定稿耗时（「审批通过（服务端 Nms）」）。

重构前后对比（需另行构建基线可执行文件，例如 `git archive ec2b5d1 | tar -x -C 某目录` 后在该目录 `cargo build -p molan-server`）：

```bash
node tools/compare-baseline.mjs /path/to/baseline/molan-server.exe
```

两个版本各用全新临时数据目录、同一份种子数据和同一个本地桩模型（OpenAI 兼容，记录每次请求），输出模型调用次数、提供的工具数、提示字数与服务端耗时。

旧数据兼容与回退（同一数据目录：基线建数据 → 当前版本读取并定稿 → 基线再次打开）：

```bash
node tools/compat-check.mjs /path/to/baseline/molan-server.exe
```

契约样例更新：

```bash
MOLAN_UPDATE_CONTRACTS=1 cargo test -p molan-server v2::tests
```

## 4. 回退

| 回退对象 | 方法 | 影响 |
|---|---|---|
| 只回退界面 | 把旧静态树（生产当前使用的已发布目录，如 `web-reviewed`）设为 `MOLAN_WEB_DIR`，或放回 `<root>/web` | 不带 `paper-studio` 标记的 `index.html` 会自动按原逻辑注入 glue / pipeline_ui / agent_ui；新后端对旧命令保持原语义 |
| 回退后端二进制（已用 `tools/compat-check.mjs` 对基线 ec2b5d1 实测） | 换回旧版 `molan-server` | 新增表（`doc_write_log`、`skill_plan_snapshot`、`artifact*`）与 `agent_run` 新列被旧版忽略；新版写入的书稿文件仍是普通 Markdown，可读可编辑；产物卡与写入账本在旧版不可见（不丢数据） |
| 回退单篇书稿 | 编辑器「版本」抽屉或 `restore_version`；回退前会先为当前内容留快照 | — |

仓库中**没有**旧界面的源码，只有编译产物（`web-recovery/bundle.*` 与生产已发布目录），因此旧界面不能从本仓库重新构建，只能复用已发布的静态树。

## 5. 数据库兼容

- 所有 schema 变更为加法：`CREATE TABLE IF NOT EXISTS` 新表（`doc_write_log`、`skill_plan_snapshot`、`artifact`、`artifact_rev`、`artifact_delivery`、`chapter_review_log`）；`books.deleted_reason` 新增取值 `import_failed`（导入失败的半成品，可在作品回收站恢复）；`agent_run` 新列用 `PRAGMA table_info` 探测后 `ALTER TABLE ADD COLUMN`，可重复执行。
- 启动时新增三步对账（均幂等、不调用模型、不删除文件）：
  1. `doc_write::recover`：账本 `prepared` 行按磁盘 hash 补记 `committed` 或标 `aborted`；
  2. `agent_run::reconcile_on_boot`：遗留 `running` → `interrupted`（附原因与已落产物数；绝不自动重跑）；生成中的产物 → `interrupted`；
  3. `chapter_commit::repair_orphans`：「正文待审」中有文件但无队列记录的稿件补登为 `pending`。
- `input_fingerprint` 不再包含整张技能表（技能改由冻结计划保证一致）。指纹只在单个任务的内存中「生成前冻结 / 落盘前比较」，不持久化；升级重启时在飞任务本就终止，因此算法变化不影响任何已落盘数据。

## 6. 生产迁移（待办，未执行）

1. **备份**：停写窗口内完整备份 `data/`（含 `writerx.db*` 的 WAL/SHM）、`books/`、版本目录与当前静态树；记录 SHA-256。
2. **影子演练**：把备份复制到隔离目录，用新二进制启动（`BIND=127.0.0.1`、`MOLAN_WEB_DIR` 指向新 `dist`），检查启动日志中的对账计数；抽查 3 本书：打开/保存/冲突、选区替换、细纲保存→确认、待审定稿、旧会话的建书卡与保存卡。
3. **真实模型冒烟**（需维护者批准费用）：在影子环境用真实渠道各跑一次 聊天 / 细纲 / 正文（指定章）/ 选区修改 / 审稿，记录 usage 与产物状态；验证工具调用模型与「不支持工具 → 直接生成」降级。
4. **发布**：沿用现有 build/deploy manifest 流程发布二进制与 `dist`（`/assets/*` 带内容哈希，可长缓存；`index.html` no-store）。
5. **观察**：前 24 小时关注 `doc_write_log` 中 `failed/conflict` 比例、`agent_run` 中 `failed/interrupted`、待审孤儿修复计数。
6. **回退预案**：按 §4 先回退界面；若需回退二进制，直接替换即可（schema 兼容）。
