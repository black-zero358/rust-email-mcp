# 2026-10-29 SMTP DATA 发送前格式校验（issue #33）

- 日期：2026-10-29，Asia/Shanghai。代码提交 `400eb962c685125d82b2e138c662ab57d4603c25`；整合最新 main `c80e7b5` 后的固定代码提交 `49b76e55370d976d7835fe694c4b79913f02ca31`。后续本文件提交不改变运行代码。
- 环境：Windows 独立 worktree、WSL Ubuntu、固定 Rust 1.99.1，专属 `CARGO_TARGET_DIR=/home/oeic_fe/rust-email-mcp-fix33-target`。源码变更/整合与验证串行；只使用普通小型合成内容、localhost SMTP 与本地数据库，没有真实邮箱操作、生产部署或压力实验。

## 核实与方案

在未修复实现，新增 `draft_data_format_rejects_noncanonical_body_without_rewriting` 实际失败：只有普通 ASCII 多行文本的非规范草稿仍被 prepare_draft 接受。未构造传输终止/后续命令样例，未测试真实服务商。

核对固定依赖 lettre 0.11.23 源码：DATA ClientCodec 实现透明传输，不把裸 LF 转换为 CRLF；message_iter 在内容后补最终 CRLF。因此只验证实际 DATA 原始字节，不盲目重写 MIME 正文，仍允许末行不含 CRLF。

新增无分配线性 validate_data，在 prepare_draft 输出及共同 claim_send 边界执行：CRLF、无 NUL、每行至多 998 原始字节，透明传输新增点不计。服务所有新发送意图在实际 journal.claim 前验证，非法格式返回固定 UNSUPPORTED_DATA_FORMAT、retryable=false，不记录意图；直接 SmtpSender::submit 也在连接前防御拒绝，不误标 unknown。既有 accepted/unknown 去重记录与恢复语义保持。

## 验证

固定代码 `49b76e55370d976d7835fe694c4b79913f02ca31` 的 `cargo fmt --check`、`cargo test --locked`（99 单元、1 集成）、`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --locked --release` 全部通过。`bash scripts/audit-dependencies.sh`（cargo-audit 0.22.2，307 依赖）、`bash scripts/test-docker-context.sh` 与 `git diff --check` 通过。

- 非规范草稿：普通裸 LF、裸 CR、NUL、999 字节行均明确拒绝；源码测试没有传输边界攻击序列。
- claim_send 实际共用路径：每次拒绝后 journal.get 为空，随后同一键的合法输入可以 claim；没有提前保留该键。
- SmtpSender 直接入口：非法格式返回确定 rejected，本地 listener 观察无连接。
- 允许 998 原始字节、合法 8bit、CRLF 与末行无换行；额外透明传输点不参与 998 限制。
- 原有 LF 头部规范化及 BCC/身份过滤保持；规范 CRLF 正文和编码附件在 prepare_draft 中逐字节保留，Base64 MIME 解码后附件二进制也保持。
- 真实 lettre DATA codec + localhost SMTP 验证普通点开头行、CRLF 正文与 Base64 multipart 的完整字节内容及终结 CRLF，最终 accepted 和 QUIT 清理正常。没有真实投递。

生产 Dockerfile 由最终 PR 的 container 门禁验证，最终 check/audit/container 必须按最终 head 全部成功后才可合并。本地 GNU release 不等同 musl 发布验收；vendored 既有 deprecated 宏警告保留。

## 兼容性与版本

这是此前接受输入集的有意收紧，按维护政策提升源码至 0.2.0，Cargo.toml/Cargo.lock 一致，仅根包版本变化，无新增/升级运行依赖或数据库迁移。源码版本不代表发布或部署。已有草稿应由客户端按 MIME 语义修复或编码后重试；不盲目改写正文，也不自动重试已存在的 unknown 发送。合法输入及原有幂等状态保持。

0.2.0 配套的原始头 SMTPUTF8/有界检测收紧见独立 #32 修复和维护说明；该项有自己的连接前确定拒绝边界，不能与本项 claim 前输入拒绝混淆。最终发布应核验两项，不以 Schema 字段未变宣称完全兼容。

依据：[RFC 5321 §2.3.8](https://www.rfc-editor.org/rfc/rfc5321#section-2.3.8)、[§4.5.2](https://www.rfc-editor.org/rfc/rfc5321#section-4.5.2)、[§4.5.3.1.6](https://www.rfc-editor.org/rfc/rfc5321#section-4.5.3.1.6)、[RFC 6152](https://www.rfc-editor.org/rfc/rfc6152)，以及固定依赖的 client/mod.rs 与 async_connection.rs。未将协议检查推广为所有 MIME 或服务商行为验证。