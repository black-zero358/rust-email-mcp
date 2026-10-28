# Issue #31 — 批量读取本地错误隔离

- 日期：2026-10-29，Asia/Shanghai。[Issue #31](https://github.com/black-zero358/rust-email-mcp/issues/31)。
- 修复代码：`324aecf`；整合 main `9bb9721e86e14ca24bd471fb82bc94aa27a1c888` 的提交 `7725f4fc429219c1544b496eaa8b2a42177132cd`；最终工具说明/契约提交 `a07ba54635e101936ba611fb18f511b4d5e90a6f`。后续文档提交不改变运行代码。
- 环境：Windows 独立工作树、WSL Ubuntu、固定 Rust/Cargo 1.99.1。历史 `issue-status-target` 在本轮专属使用，先清理本项目与 vendored async-imap 构建产物，确认重编当前工作树。只使用合成小邮件与本地 duplex；没有真实邮箱或部署。

## 核实及回归

原实现运行新增 `batch_read_continues_after_local_content_errors` 失败：请求两封，第一封已完整获取但本地正文不支持，`completed=1`，后续正常 UID 未读取。

修复按阶段分流而不是判断错误码：只有 `raw()` 成功后才调用本地 MIME 解析，解析错误记录单项失败并继续。获取失败保留既有保守停止边界，后续 UID 为 `unprocessed`，不重试。`completed` 保持已尝试项数，包含失败项。

- `batch_read_continues_after_local_content_errors`：`UNSUPPORTED_BODY`、头部预算 `LIMIT_EXCEEDED`、本地 malformed header 的 `MAIL_BACKEND` 三类后均读取正常下一封，返回两项、`requested=completed=2`。其中本地 `MAIL_BACKEND` 证明不能仅按错误码判断会话是否可用。
- `batch_read_stops_after_protocol_failure_or_transport_exhaustion`：第一封 FETCH 后 NO/BAD/EOF/错误 tag 或累计传输预算耗尽时，只尝试第一封；后续 UID 保留 `unprocessed`，不发出额外 UID 命令。
- 合成后端检查 EXAMINE、UID SIZE/BODY.PEEK 命令及 tagged completion；保持 UIDVALIDITY、原始字节预算和 MIME 解码/结构预算。

依据：[RFC 3501 §2.2.2](https://www.rfc-editor.org/rfc/rfc3501#section-2.2.2) 的命令响应终结语义。判断能否继续依赖成功消费 FETCH 阶段，不以本地正文失败替代协议状态。

## 验证结果与边界

固定最终源码 `a07ba54635e101936ba611fb18f511b4d5e90a6f` 后，`cargo fmt --check`、`cargo test --locked`（86 单元 + 1 IMAP 集成）、`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --locked --release`、`git diff --check` 全部通过。专用 `bash scripts/audit-dependencies.sh`（cargo-audit 0.22.2，307 依赖）通过，依赖、Cargo.lock 和 vendored 代码不变。

曾有一次 release 构建与整合主线重叠，读到旧 vendor 产物而失败；该次作废，清理两个 crate 产物后对固定源码完整重跑，上述通过结果仅指重新运行。vendored 既有 deprecated 宏警告保留。最终 PR 的 `check`、`audit`、`container` 必须按最终 head 核对，本地历史结果不替代 CI。

兼容修复：工具参数/结果字段/错误码、权限、配置和数据库不变，无迁移；明确工具说明与 README 的逐项语义。没有真实读取/发送/修改、生产部署、持续负载或 musl 产物验收；本地 release 为 GNU 默认目标。
