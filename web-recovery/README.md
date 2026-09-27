# 建书保存前端修复记录

部署版本：20260927-095248。后端二进制未改动。

## 已确认原因

- 单项保存失败被 store 捕获后隐式返回，组件无条件标记“已存”。
- list_messages 缺少 bookId，被后端归属校验拒绝；前端原来静默显示空会话。
- 被修改的同名资产仍使用 immutable 一年缓存，旧客户端可能持续加载旧逻辑。
- isTauri 缺失的早期诊断是错误的；线上 glue 明确设置了它。

## 修复顺序

本目录保留原 bundle.js（SHA256 f53fa2048e89a0a195880fc9e95aa8276c705955d1a23b5f27a7429dd324737e）与 bundle.css，以及可审计脚本：

1. node booksetup-patch.cjs
2. node post-review.cjs
3. node --check final-bundle.js

注意：子代理基础补丁不是最终版本，必须执行 post-review.cjs；它移除模拟保存、严格检查后端确认、修正缓存/归属及忙状态。

最终已部署静态资产完整保存在项目 web-reviewed 目录（编译前端，不是 React 原始源码）。发布时将它作为 web.new，通过现有 build/deploy manifest 校验流程发布。release-web.py 是本次从原始静态树生成新资源名的记录脚本，默认输入是当时 recovery-review/isolated/web；不可对已经版本化的 web-reviewed 再运行。

所有 JS/CSS 分块同时换新地址，动态导入引用同步更新，避免新旧模块混用。未来每次修改资产内容必须重新版本化，不能同名覆盖 immutable 缓存资源。

## 实测证据

- 本机127.0.0.1:17482独立空数据库：真实Chrome点击单保存、全部保存，2个文件磁盘内容逐字一致，书名与消息saved持久化，刷新仍显示已创建。
- 线上只读浏览器：新主脚本index-molan-6af43e72544b.js，已有会话恢复12条assistant消息、4张建书卡，零pageerror。
- 线上静态资产+隔离IPC拒绝注入：保存失败不显示已存，零pageerror。
- 后端边界：缺确认、非空批量files、错误/跨书message被拒；标题不变；同名覆盖被拒且原文不变。
- 桌面卡片宽度等于消息body；390px窄屏无横向溢出，按钮纵向排列。

完整测试脚本和截图位于工作区 molan-work/recovery-review。测试是确定性夹具，不包含真实模型生成端到端测试。未代替用户点击真实书籍保存。

## 发布指纹

binary: ab62407e10b9da21d70d8a1906c7050e09c8788d780f6383c16db1a864738593

web tree: 1eb04fd357d1ddfeb872f09b47ec14a84579667cd61e1cb30dd590914791382b

数据库备份位于NAS /vol2/molan-deploy/db-backup-20260927-095248；上一版本20260927-020056保留。

## 排查意外与范围

一个只读审查子代理误点生产新书共创入口，两次真实生成新增四条消息并消耗额度。已向用户披露，未删除消息；Lead核对四个ID前缀为f3dd21f2、8c539670、8dcd6dc8、a0875b78。后续测试使用本机独立数据库或拦截所有生产写请求。

GitHub上传已按用户选择暂缓。本目录和web-reviewed均尚未推送到任何仓库。
