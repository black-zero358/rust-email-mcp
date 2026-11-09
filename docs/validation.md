# 验证记录

本页保存发布验收与现行兼容性复验入口。每条验收记录注明提交/tag、日期与时区、环境、命令、结果和 CI 链接；历史实验按月份归档。验证要求见 [贡献指南](../CONTRIBUTING.md#验证与完成标准)。

## 2026-11-06 v0.3.0 发布验收

注解标签 `v0.3.0` 的 Git 对象为 `57f01440b1446a685be0c29c67e0ad6f3183114a`，于 2026-11-06 16:54:47（Asia/Shanghai）创建，指向源码提交 `3e3ebc5b280813df6cb65cf63d6a1c6a762e614c`。该提交的 `scope`、`check`、`audit`、`container` 均通过。[源码提交](https://github.com/black-zero358/rust-email-mcp/commit/3e3ebc5b280813df6cb65cf63d6a1c6a762e614c) · [主干 CI](https://github.com/black-zero358/rust-email-mcp/actions/runs/42188935189)

Release 工作流在 Ubuntu 24.04 runner 使用 Rust 1.99.1 和 `musl-gcc` 从标签提交构建 `x86_64-unknown-linux-musl`。`package` 和 `publish` 均通过，工作流于 2026-11-06 16:58:16（Asia/Shanghai）发布以下附件。[发布工作流](https://github.com/black-zero358/rust-email-mcp/actions/runs/42191094237) · [发行页](https://github.com/black-zero358/rust-email-mcp/releases/tag/v0.3.0)

| 文件 | 字节数 | SHA-256 |
| --- | ---: | --- |
| `rust-email-mcp-0.3.0-x86_64-unknown-linux-musl.tar.gz` | 9,243,240 | `869fd933efc24538640dd96fab5f0de3ae3f19d5d405b48a0eb11675980a5055` |
| `SHA256SUMS` | 120 | `b9ba7bd1595b15103f53c157fcbb1b6de3657f4e3a944612f34ba57418c11bf1` |
| 解包后的 `rust-email-mcp` | 21,824,560 | `1bde603ad4b20bef30b2140721eaa487095bb490ad276423cb9e37954b76bd18` |

独立复验从发行页下载两个附件，核对 GitHub 资产摘要与下载字节，并核对 `SHA256SUMS` 记录的归档摘要。解包与程序核验使用 WSL Ubuntu amd64。归档含程序、`SOURCE.txt`、README 和 LICENSE 四个普通文件。README、LICENSE 与标签内容一致，`SOURCE.txt` 的版本、目标、工具链、提交及二进制摘要均已核对。使用 `file` 和 `readelf` 确认二进制为 ELF64、little-endian、x86-64、已剥离符号的静态 PIE，程序头无 `INTERP`，动态表无 `NEEDED`。

使用标签内的 `scripts/check-cache-recovery.py`，通过 uv 离线运行 Python 3.12.3，对实际下载的程序执行 `--schema-version 6` 恢复烟测。独立用户和网络命名空间内验证了恢复代次 1、2、3，缓存关闭时失败启动仍推进运行见证，损坏见证在打开 Journal 前被拒绝，操作库哨兵保持原字节，缓存目录和文件分别为 0700 和 0600。该版本的客户端兼容事项见[升级与回退](maintenance.md#030-升级要点)。

## 0.3.0 兼容性验证入口

下表用于在最终候选提交上复验升级契约。测试使用合成邮件、隔离缓存及测试身份，运行 `cargo test --locked <测试名>` 可单独复验。验收结果按[发布检查](maintenance.md#发布检查)记录到对应提交和 CI 运行。

| 场景 | 回归入口及检查点 |
| --- | --- |
| 单封和批量整理 | `single_writes_distinguish_missing_targets_and_confirmed_updates`、`message_write_outcomes_validate_through_signed_mcp`。验证缺失或重复删除、逐 UID 证据、命令已接受但结果待核对、错误完成，以及签名 MCP 的逐项结果和连接失败 Schema。 |
| 正文与旧索引 | `structured_alternative_retries_render_failure_but_keeps_command_failure`、`offline_index_rebuild_rotates_past_storage_failures_and_retries`、`offline_body_search_publishes_projection_coverage_through_its_schema`。验证 MIME 候选回退、旧索引有界轮转和 v2 搜索覆盖。 |
| 摘要与定位 | `listing_reports_header_coverage_without_exposing_a_clipped_field`、`offline_search_matches_both_internal_date_formats_and_legacy_flags`、`cold_locate_accepts_complete_header_only_literals_within_budget`。验证在线及离线的 `header_complete`、截断字段、纯头邮件和协议完成。 |
| 原发送参数与附件引用 | `signed_legacy_writes_accept_crlf_in_body_only`、`legacy_inbox_attachment_ids_still_bind_all_reference_dimensions`、`signed_target_attachment_get_rechecks_subject_and_returns_exact_binary`。验证旧操作键的指纹语义及附件 ID 的在线、离线、大小写和作用域兼容性。 |
| 正常停止 | Linux 上运行 `sigterm_drains_admitted_drafts`，向测试服务发送 SIGTERM，验证空闲时快速退出、排空已接纳写入，以及 SMTP 确认超时后持久保留 `unknown`。部署时按[停止宽限](deployment.md#验收与回滚)核对实际编排配置。 |

候选提交同时运行 `cargo fmt --check`、`cargo test --locked`、`cargo clippy --locked --all-targets -- -D warnings`，并按[维护规程](maintenance.md#离线持续验收)复核测试规模。发行二进制的静态链接和隔离恢复结果记录在对应发布验收中。

## 历史记录

| 记录 | 内容 |
| --- | --- |
| [2026 年 11 月](archive/2026-11/validation.md) | 测试精简、R00–R13、缓存恢复与同步、离线附件实验 |
| [2026 年 10 月](archive/2026-10/validation.md) | 初始部署、协议与 MIME 修复、早期资源测量 |
| [Issue 修复台账](archive/issue-campaigns.md) | 两轮修复的 PR、提交、审阅与 CI |

现行回归对应[架构验证表](architecture.md#invariants-and-verification)。新发布验收记录保留在本页；阶段实验归入对应月份，日常变更证据写入 PR。
