# Molan Paper Studio 重构文档

| 文档 | 内容 |
|---|---|
| [01-baseline.md](01-baseline.md) | 重构前基线：工程事实、实测调用链、22 个已确认问题（附 file:line）、功能与链路迁移矩阵 |
| [02-architecture.md](02-architecture.md) | 新架构：分层职责、调用链收敛前后、Agent 权限、技能计划、上下文、写入语义、状态机、有意的行为变化、兼容与回退 |
| [03-contracts.md](03-contracts.md) | 契约：新增 IPC、Task / SkillPlan / WritePlan / WriteReceipt / Artifact / RunState / NDJSON 事件、旧数据适配 |
| [04-verification.md](04-verification.md) | 验证报告（通过 / 部分 / 未验证 / 阻塞）、基线问题处置、迁移矩阵交付状态 |
| [05-design.md](05-design.md) | 设计令牌、布局与断点、可访问性、截图索引 |
| [06-operations.md](06-operations.md) | 构建运行、验证命令、回退、数据库兼容、生产迁移待办（未执行） |
| [screenshots/](screenshots/) | 浏览器端到端自动生成的截图（mock 渠道） |

规范契约样例在仓库根的 `contracts/fixtures/`，由 Rust 测试生成、前端测试校验。

**范围声明**：模型相关验证全部使用 `mock://` 确定性渠道（协议 / 流程验证）；真实模型端到端与中文输入法手动测试未验证。本次未修改生产数据、未部署、未提交或推送。
