# 2026-10-29 APPEND continuation 通知（issue #30）

- 日期：2026-10-29，Asia/Shanghai。修复代码 `d7d3a452ddc84ec39cbfc1634907a7967e02cfff`；基于 main `717a302e3ca955a084c3a0cca78d38dcb0fc141a`，独立分支 `codex/fix-30-append-continuation`。
- 环境：Windows 隔离工作树、WSL Ubuntu、固定 Rust 1.99.1，独立 `CARGO_TARGET_DIR=/home/oeic_fe/rust-email-mcp-fix30-target`。没有跨工作树复用测试产物；只使用本地 duplex 和小型合成草稿，没有真实邮箱、SMTP、部署或压力实验。

## 核实与实现

原 APPEND 只读取一次响应，要求其为 Continue；合法 untagged 通知导致 Error::Append。新增 `save_draft_waits_for_continuation_after_notifications` 在未修复基线上实际失败，正常通知路径返回 MAIL_BACKEND。

修复在 APPEND 等待同步 literal continuation 时循环读取，把合法通知交给原有有界（100 项、满后丢弃）unsolicited 队列；只有实际 Continue 才发送 literal。此前任何 tagged 结果、BYE、EOF 都保持 Error::Append，不发送正文；排队的后续 continuation 不能恢复已拒绝命令。读取错误照常传播，正文后的匹配 tag 完成校验保持原调用。应用连接字节预算和操作 timeout 保持原有边界。

## 验证

- `cargo fmt` 后，`cargo test --locked`（84 单元、1 集成）、`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --locked --release` 通过；修改的 vendor 文件另用 Rust 2024 rustfmt 格式化，`git diff --check` 通过。
- 新矩阵通过生产 `save_draft` 路径验证零、一、三条通知，通知可交付、literal 内容及 CRLF、正确 `(\Draft)` flags 和 tagged OK。NO/BAD/BYE/EOF/错误 tag/提前 OK 前置拒绝后，服务器确认没有任何 literal 字节；非 EOF 拒绝后另排队 Continue，仍不发送。原 post-literal NO/BAD 与 flags 回归保留。
- 专用 `bash scripts/audit-dependencies.sh`（cargo-audit 0.22.2，307 依赖）和 `bash scripts/test-docker-context.sh` 通过。上游版本/checksum、Cargo.lock、许可证及依赖不变；本地状态机补丁需另行代码审阅，审计不能证明协议行为。
- 生产 Dockerfile 由最终 PR `container` 检查补足；最终 `check`、`audit`、`container` 均须成功后才可合并。没有把其他提交的 CI 作为最终候选验证。

## 兼容性与边界

修复此前被误拒绝的合法通知，工具参数/结果/错误码、权限、配置、数据库、MIME 和草稿 flags 不变，无迁移或新增依赖。保留前置拒绝 Error::Append，不扩大 literal 提交条件。

真实 IMAP、真实草稿写入、生产部署、持续负载和 musl 发布产物未验收。本地 release 为 Linux GNU 默认目标。

依据：[RFC 3501 §2.2.1](https://www.rfc-editor.org/rfc/rfc3501#section-2.2.1)。