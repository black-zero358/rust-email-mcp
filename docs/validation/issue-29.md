# Issue #29 — FETCH 属性归并验证

- 日期：2026-10-29，Asia/Shanghai。问题：[issue #29](https://github.com/black-zero358/rust-email-mcp/issues/29)。
- 修复代码：`223f263`；整合 main `717a302e3ca955a084c3a0cca78d38dcb0fc141a` 后代码提交：`c86654b4dda067565aa4cd6f953b93e443704a3f`。后续文档提交不改变运行代码。
- 环境：Windows 独立工作树、WSL Ubuntu、固定 Rust/Cargo 1.99.1。独立 `CARGO_TARGET_DIR=/home/oeic_fe/issue-29-target`，复制缓存后清除本项目及 vendored async-imap 产物并实际重编。只使用小型合成协议响应及 duplex 后端；没有真实邮箱操作或部署。

## 复现与回归

新增两项回归先在原实现运行失败：`raw_fetch_ignores_interleaved_flags_and_requires_target_attributes` 返回 `MAIL_BACKEND`；`list_fetch_merges_uid_attributes_and_only_actual_flag_updates` 在 `limit=1` 时返回 5 条同 UID 记录。修复后通过。

四项新回归经过真实 async-imap parser、Session 和应用 raw/list 路径，覆盖：

- SIZE/BODY 前后交错的同 UID FLAGS，以及其他 UID/无 UID 响应；缺失所需属性仍失败。
- 按请求 UID 合并 HEADER、SIZE、最后显式 FLAGS；空 FLAGS 清空，后到 UID/SIZE/HEADER 缺省 FLAGS 不清空已有标记。
- FLAGS-only 不创建摘要，真实空 HEADER 可创建摘要；分拆属性保持唯一 UID、降序及请求 limit，其他 UID 和无 UID 的 HEADER 不进入结果。
- SIZE、BODY 和列表的匹配 tagged OK；已有数据后 NO/BAD/EOF/错误 tag 均失败。

依据：[RFC 3501 §5.2](https://www.rfc-editor.org/rfc/rfc3501#section-5.2) 允许非请求更新；[§7.4.2](https://www.rfc-editor.org/rfc/rfc3501#section-7.4.2) 定义 FETCH 属性。vendored `Fetch::has_flags()` 只暴露属性存在性，解析及既有 iterator 行为不变，来源与范围记录于 [PATCHES.md](../../vendor/async-imap/PATCHES.md)。

## 检查与边界

整合代码 `c86654b4dda067565aa4cd6f953b93e443704a3f` 的 `cargo fmt --check`、`cargo test --locked`（87 单元 + 1 IMAP 完成状态集成）、`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --locked --release` 通过。修改的 vendor 文件另以 `rustfmt --edition 2024 --check` 检查。`git diff --check` 通过。

专用 `bash scripts/audit-dependencies.sh`（cargo-audit 0.22.2，307 项依赖）和 `bash scripts/test-docker-context.sh` 通过。运行依赖、Cargo.lock、上游版本/checksum 和容器 allowlist 不变。生产 Dockerfile 本地构建已启动，尚未取得完成结果；最终 PR `check`、`audit`、`container` 必须成功后才可合并。vendored async-imap 既有 deprecated 宏警告保留。

兼容修复：不改变工具参数、结果字段类型、错误码、权限、配置或数据库，无迁移。列表不再把 FLAGS-only 更新当成空邮件；SIZE/BODY 保持原来的字节预算与缺失属性错误，连接仍等待完整命令终结。未验收真实 IMAP、真实发送/修改、持续负载、生产部署或 musl 发布产物；本地 release 为 GNU 默认目标。
