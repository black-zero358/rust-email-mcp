# 2026-10-29 SELECT/EXAMINE 与 STATUS 完成边界（issue #28）

- 日期与环境：2026-10-29，Asia/Shanghai；Windows 隔离工作树、WSL Ubuntu、固定 Rust 1.99.1。全程为本地 duplex 与合成数据，没有真实邮箱、发送、部署或压力实验。
- 修复代码：`331da70cc37c34ab547a8c90ee015ad21e512c8d`，原基线 `36177076aae667cd72ba3ca81556b94575ab0fc6`；随后合入已完成的 main issue #25，合并提交 `f998b576c2e34164a836879baaa1e9118bfb4729`。

## 根因与修复前证据

`parse_mailbox`、`parse_status` 原来直接遍历原始 stream；EOF 后无条件返回 `Ok(Mailbox)`。matching-tag NO/BAD 会拒绝，但未匹配 tag 的完成响应被当作非目标更新，缺失完成状态也没有错误。应用 list/search 在 exists 为零时返回空页，因此未完成 EXAMINE 被解释成空邮箱。

在未修复的基线上，新增 `empty_mailbox_pages_require_completed_examine` 失败，错误断言获得 `Ok({emails: [], next_before_uid: null, uid_validity: null})`，确认问题仍然存在。没有将这一合成输出推广为所有邮箱操作的行为。

修复复用既有 `checked_responses`，每个命令保留一个实例；未修改共享 helper。SELECT、SELECT (CONDSTORE)、EXAMINE 和 STATUS 只在对应 tagged OK 后成功，EOF、部分响应 EOF、错误 tag、NO/BAD 通过既有错误链路返回。字段提取和非请求更新分发保持原行为。

## 已执行验证

- 原始修复候选 `cargo fmt` 后，`cargo test --locked` 通过：79 单元、2 集成；`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --locked --release` 通过。专属 target `/home/oeic_fe/rust-email-mcp-fix28-target`，没有跨工作树共享测试产物。
- 新增 `mailbox_commands_require_matching_tagged_completion`：四个命令入口，各验证空响应/部分数据与 OK、NO、BAD、EOF、错误 tag 的组合；成功时检查 0 EXISTS、UIDVALIDITY 和 UIDNEXT。
- 新增 `empty_mailbox_pages_require_completed_examine`：应用 list/search 在缺少或拒绝完成时返回稳定 `MAIL_BACKEND`，真正成功空邮箱返回原有空页和元数据，同时断言没有后续 SEARCH/FETCH 或写命令。
- 随后合入 main issue #25，运行时代码无冲突；但共享 WSL 新会话出现 `WSL/Service/WSAETIMEDOUT`，后续本地重测、专用依赖审计和 Docker 检查尚未完成，不能标为通过。未重启共享 WSL；最终候选必须以 PR 的 `check`、`audit`、`container` 全部成功为合并门禁。
- WSL 恢复后，合入 main `717a302e3ca955a084c3a0cca78d38dcb0fc141a` 的提交 `3363aa00378ed5c7346e30eebe08af57e9c8967a` 重新完成 fmt、84 单元+2 集成、Clippy 和 release，均通过；专用依赖审计（cargo-audit 0.22.2，307 依赖）及 Docker context allowlist 通过，生产 Dockerfile 由最终 PR #41 的 container 门禁验证。
- vendored 上游版本、checksum、许可证和 Cargo.lock 不变；PATCHES.md 与审计脚本的本地补丁审阅范围同步更新。上游已有 `unsafe_pinned` 弃用警告保留，未引入新警告。

## 兼容性与边界

修复错误成功结果，保留稳定错误码、工具参数/结果、权限、配置和操作数据库格式；无迁移或新依赖。0 EXISTS + matching tagged OK 仍成功。STATUS 解析的相邻遗漏一并修复；当前应用统计使用独立严格 STATUS 实现，本项不宣称该统计路径原来有同样缺陷。

未验证真实 IMAP 兼容性、生产部署、自然 OAuth 刷新、持续负载及 musl 发布产物。默认 Linux GNU release 构建不能替代 musl 交付验证。

依据：[RFC 3501 §2.2.2](https://www.rfc-editor.org/rfc/rfc3501#section-2.2.2) 与 [EXAMINE §6.3.2](https://www.rfc-editor.org/rfc/rfc3501#section-6.3.2)。