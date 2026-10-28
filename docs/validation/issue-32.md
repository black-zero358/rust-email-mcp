# 2026-10-29 SMTPUTF8 原始头需求（issue #32）

- 环境/日期：Windows 独立工作树、WSL Ubuntu、固定 Rust/Cargo 1.99.1；2026-10-29，Asia/Shanghai。独立 target `/home/oeic_fe/issue-smtputf8-target`，复制依赖缓存后清除项目及 vendored async-imap 产物并重编，新增线路回归命名执行 1 项。
- 代码：`12856ccc4bf9ced347cb9cc57bb4c16cbb556490`；整合 `aca17fc390cfe7cd284b2e43d5846f119e4cfa94` 合入 main `ca9eeac23f9dfe3c3d0f3a95680ee896a7829ec1`。唯一冲突为 MIME 测试插入位置，保留 SMTPUTF8 与 disposition 两项测试；运行时代码自动合并。后续文档提交不改变已测源码。

## 证据与行为

修复前 `smtp_utf8_covers_preserved_draft_headers_but_not_body_bytes` 通过真实 prepare_draft → SmtpSender::submit → 本地 SMTP 线路重现：ASCII 信封与原始 UTF-8 Subject 在 MAIL 中未添加 SMTPUTF8，断言失败。测试未联系真实邮箱或真实收件人。

`mime::requires_smtputf8` 检查原始头字节，不使用已解码的 encoded-word。所有 MIME 部件头包含附件子树；identity 编码 message/rfc822、message/global 用同一结构/头部预算递归检查。Base64 等编码封装保持不透明；叶子正文不按邮件头解释，不解码、选择或渲染正文。ASCII 原始消息无需创建 MIME 树。

需要扩展而服务端未提供时，在 MAIL 前以私有确定拒绝类型返回 rejected；连接前头部预算/解析失败同样确定拒绝。已有持久化路径先记录结果再进行有界 QUIT 清理。网络中断、最终 DATA 结果不确定仍为 unknown；accepted 和已知结果后的审计/清理边界不变。

## 已执行检查

- 冻结整合源码后 `cargo fmt --check`、`cargo test --locked`（94 单元 + 1 IMAP 完成状态集成）、`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --locked --release` 全部通过。
- 专用 `bash scripts/audit-dependencies.sh`（cargo-audit 0.22.2）覆盖 307 项依赖并通过；`git diff --check` 通过。依赖、锁文件和 vendored 补丁不变，保留既有 deprecated 宏警告。
- `smtp_utf8_covers_preserved_draft_headers_but_not_body_bytes`：ASCII 信封下原始 Subject、显示名、附件 MIME 子头、嵌套邮件头各与有/无 SMTPUTF8 能力交叉验证；RFC 2047 ASCII 头 + 8 位正文、ASCII MIME 子头 + 8 位正文、嵌套 ASCII 头 + 8 位正文均可在仅 8BITMIME 服务端成功。检查实际 MAIL 参数、RCPT、DATA、内容及拒绝时只允许清理而不发送 MAIL。
- `smtp_utf8_inspection_uses_raw_headers_and_shared_budgets`：编码词、伪头正文、二进制附件内容、Base64 封装 message/global 的对照，以及嵌套/头部预算拒绝。
- `smtp_utf8_refusal_is_recorded_as_rejected_without_mail`：真实 prepare_draft/submit/record_send，确认 rejected 已保存，复用原键仍为 rejected，线路只出现 EHLO 与清理 QUIT。
- 线路内容断言保留 lettre 0.11.23 已有的 DATA `CRLF . CRLF` framing；它在原始消息已有 CRLF 后仍追加 leading CRLF。本修复不改传给 transport 的草稿正文；DATA 格式校验由独立 issue #33 处理。

## 兼容性与未覆盖项

修复 SMTPUTF8 漏协商与确定拒绝被误记为 unknown 的行为；工具参数/结果结构、权限、配置、操作数据库与依赖不变，无迁移。8BITMIME 的既有原始非 ASCII 检查保持，普通国际化正文不被误要求 SMTPUTF8。无法在有界结构/头部检查中确定需求的非 ASCII 草稿会确定拒绝，不作为不完整推断后的发送。

仅合成数据与本地模拟 SMTP，没有真实邮箱操作、生产部署、持续负载或 musl 产物验收。最终 PR 的 check/audit/container 必须绑定最终 head 全部成功后合并；后续整合验证用 COMMENT 记录。

依据：[RFC 6531 §3.2](https://www.rfc-editor.org/rfc/rfc6531#section-3.2)、[§3.6](https://www.rfc-editor.org/rfc/rfc6531#section-3.6)、[RFC 6532 §3.5](https://www.rfc-editor.org/rfc/rfc6532#section-3.5)，以及锁定 mailparse/lettre 的原始头访问与 DATA 实现。
