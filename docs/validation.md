# 验证记录

本文件保存带版本与环境边界的证据。每次新增记录注明提交/tag、日期与时区、环境、命令、结果、CI 链接及未覆盖项；保留历史结果，不把其测试数量或部署状态当作当前版本的证明。验证要求见 [贡献指南](../CONTRIBUTING.md#验证与完成标准)。

## 2026-10-28 源码 CI 快照

- 提交：[`c775e922e29ff04a0dfafe3f0e501a8b926b290a`](https://github.com/black-zero358/rust-email-mcp/commit/c775e922e29ff04a0dfafe3f0e501a8b926b290a)。
- 来源：[CI run 40678288110](https://github.com/black-zero358/rust-email-mcp/actions/runs/40678288110)，push 触发，完成时间 2026-10-28 19:16:50（Asia/Shanghai）。本记录来自已完成运行日志的核验。
- 环境：GitHub Actions `ubuntu-latest`，固定 Rust 1.99.1；具体 runner 与构建环境以该运行日志为准。
- `check`：`cargo fmt --check`、`cargo test --locked`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --locked --release` 均通过；测试为 57 项单元测试、1 项集成测试。
- `audit`：cargo-audit 0.22.2，`bash scripts/audit-dependencies.sh` 通过，包含 vendored async-imap 对应上游版本的审计视图。
- `container`：Docker 上下文哨兵检查和生产 Dockerfile 构建通过。
- 边界：这次 CI 不证明真实邮箱写入、生产 OAuth 刷新、持续负载、恢复演练或 musl 发布资产通过验收；本地补丁的代码审阅也不能由依赖审计替代。

## 2026-10-28 初始部署验收历史

以下为原始验收记录。记录当时的 Rust 1.99.1、rmcp 3.8.0、Rauthy 0.37.2、SQLite 3.54.1；未保存确切提交、产物校验值或时区，不能据此推定覆盖后续修复。[v0.1.0 Release](https://github.com/black-zero358/rust-email-mcp/releases/tag/v0.1.0) 也记载了 16 项测试，但不据此反推这些现场测量对应的源码提交。

### 自动化检查

- `cargo test --locked`：16 项通过。
- `cargo fmt --check`、`cargo clippy --locked --all-targets -- -D warnings`：通过。
- Linux amd64 musl 静态 release 构建：通过。
- `cargo audit`：0 项漏洞、0 项警告；JWT 使用 AWS-LC 后端，审计包含测试依赖。
- 本地模拟 SMTP 验证接受后审计失败、响应丢失、重复操作键；合成草稿验证 BCC 和 MIME 附件保留。
- JWT 测试覆盖签名算法、issuer、audience、subject、scope、有效期与 nbf。

历史 `cargo audit` 结果仅保留为当时记录。当前审计入口统一为 `bash scripts/audit-dependencies.sh`，普通 `cargo audit` 会跳过 path 包，不能用于宣称完整覆盖当前 vendored 依赖。

### 隔离部署验收

使用独立测试身份与 OAuth 客户端，完成同用户两次登录、两份刷新令牌独立使用、提前刷新和服务重启后的刷新恢复。QQ 邮箱验证包括工具发现、账户、文件夹、单条列表、正文读取和读前后标记一致。工具数量为 20。

SMTP 故障测试使用本地模拟服务器。真实邮箱发送、移动、删除和文件夹写入由专用测试邮箱另行验收。

### 内存与存储

Ubuntu Docker 单次空闲内存快照，在新服务完成登录和只读调用后采集：

| 服务 | Docker 显示内存 |
| --- | ---: |
| Rust 邮件服务 | 1.445 MiB |
| Rauthy | 33.02 MiB |
| 原 FastMCP gateway | 246.4 MiB |
| 原 Node 邮件服务 | 73.86 MiB |
| 原 Dex | 10.55 MiB |
| 原 PostgreSQL | 18.93 MiB |

新组合合计 34.465 MiB，原组合合计 349.74 MiB。原容器已连续运行约 46–47 小时，新容器刚完成验收。登录哈希、并发和大附件的峰值需持续采样。

正式切换并将邮箱授权收敛到 owner 后，邮件容器重建；空闲快照为 mail 1.273 MiB、Rauthy 26.24 MiB，合计 27.513 MiB。原四个容器已停止，数据保留供回滚。

SQLite 使用本地新数据库、WAL、synchronous=FULL；4 个线程执行共 1,000 次 claim → finish → get 循环。总耗时 4.068 秒，中位数 3.719 ms、p95 7.236 ms、最大 2.107 秒。该结果记录当前磁盘和锁竞争条件下的操作记录成本。选用 SQLite 的主要收益是省去常驻数据库服务。

### 当时尚未完成的验收

原开发记录还列有用户侧 ChatGPT 重新连接/自然刷新、专用测试邮箱的授权写入，以及持续负载和大附件内存观察。这里保留其历史未覆盖状态；后续完成时应追加对应版本和证据，不能从源码 CI 成功推定完成。原记录称验收后已移除测试邮箱授权，这不构成任何后续真实邮箱操作的授权。
