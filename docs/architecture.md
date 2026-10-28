# 架构与设计约束

[CONTRIBUTING.md](../CONTRIBUTING.md) 是工程总入口，定义设计原则 D1–D6、变更流程和完成标准。本文件记录这些原则在当前实现中的细则、模块边界和设计决策。用户能力与配置见 [README](../README.md)，维护要求见 [maintenance](maintenance.md)，部署操作见 [deployment](deployment.md)。验收结果和基准测量属于带版本依据的 [validation](validation.md)，不能替代当前规范。

## Module boundaries

当前实现是一个 Rust 服务进程，每个实例配置一个邮箱并独占一份本地操作数据库；多个邮箱使用独立实例、数据目录和 OIDC 资源地址。生产路径使用 Streamable HTTP、IMAP4rev1 和 SMTP，认证服务独立部署。Python 和 Node 不参与服务运行时。

| 模块 | 当前职责 | 依赖与边界 |
| --- | --- | --- |
| `main` | 读取配置、组装服务、中间件、发现路由、日志和停机流程 | 负责接线；不得绕过认证、请求预算或输出保护 |
| `config` | 环境变量、默认值、配置有效性 | 供认证和邮件适配器读取；凭据不进入日志 |
| `auth` | OIDC 发现、JWT/JWKS 校验、请求主体和 HTTP 认证错误 | 依赖配置与身份服务，不执行邮件操作 |
| `server` | MCP 工具目录、权限模式、输入校验、响应封装与预算 | 将已认证主体传给 `mail`；协议适配不直接操作邮箱或数据库 |
| `mail` | IMAP 会话、邮件操作编排、发送意图与结果记录 | 调用 `imap_text`、`mime`、`smtp`、`journal`，持有执行槽位 |
| `mail/attachments`、`mail/insights` | 附件交付及有界读取扩展 | 由 `mail` 编排，共用 MIME、协议、槽位与预算 |
| `imap_text` | rev1 邮箱名、非 ASCII 搜索与严格统计 STATUS | 处理编码、literal 和命令终结；不承担 MCP 授权 |
| `mime`、`html` | 有界 MIME 解析、正文选择、附件分类、HTML 文本提取 | `mime` 调用 `html`；不执行网络请求、不发送邮件 |
| `smtp` | 经验证 TLS/AUTH 的提交事务及独立清理 | 返回事务结果；由 `mail` 调用 `journal` 持久化，不在适配器中自动重试 |
| `journal` | SQLite 操作键、参数摘要、状态持久化及重启恢复 | 不调用邮件后端；数据库事务不跨 SMTP/IMAP 网络请求 |
| `error` | 脱敏的错误结构与基础错误构造 | 供上层使用；不得把底层协议错误原文透传给用户或日志 |

维护时保留上述依赖方向：协议、解析和存储模块不得反向依赖 `server` 或 HTTP 认证上下文。跨边界新增能力应先说明职责、兼容性和验证方法；当前文档不要求把现有模块重构为多 crate 或引入额外抽象。

## Stable contracts

工具名、参数与必填项、输出字段、错误码和重试语义、权限模式、邮件引用、幂等范围、环境变量及持久化格式都是变更时需要审阅的契约。实现入口分别在 [`server.rs`](../src/server.rs)、[`error.rs`](../src/error.rs)、[`config.rs`](../src/config.rs) 和 [`journal.rs`](../src/journal.rs)；变更与发布规则遵循 CONTRIBUTING.md。

| 契约 | 当前约定 |
| --- | --- |
| 权限模式 | `read` 只开放读取/状态工具；`draft` 再开放 `save_draft`；`send` 再开放发送、回复和转发；`full` 再开放标记、移动、删除及文件夹写入。默认 `read`。 |
| 邮件引用 | 文件夹名、UID、UIDVALIDITY 共同定位邮件；不得在 UIDVALIDITY 变化后沿用旧引用。列表/搜索使用 `next_before_uid`，空页也可能有后续游标。 |
| 幂等范围 | 同一服务实例/操作数据库内，以 **OIDC subject（principal）与 operation_key** 为键；不是同一邮箱所有用户共享去重。不同 subject 的相同键相互独立；多实例数据库之间不共享去重。 |
| 重试与状态 | 相同键和相同参数返回已有状态；参数变化返回 `IDEMPOTENCY_CONFLICT`。`sending` 为进行中，`accepted/rejected/unknown` 为结果；`unknown` 必须人工核实，不自动重发。 |
| 结果与错误 | 工具执行结果使用结构化内容；常规成功含 `data`、`request_id`，工具错误含 `error`、`request_id`，错误具有 `code/message/retryable`。协议层错误仍使用 MCP 错误；不能把协议错误、业务失败与发送结果混为一类。超限降级结果保留已知发送状态及禁止自动重试的提示。 |
| 用户授权 | 客户端负责展示收件人与内容并取得明确发送授权；破坏性操作先确认。服务端执行权限模式和相应 `confirm` 校验；工具提示或 annotations 不能替代服务端检查，也不是用户授权的证明。 |

一次工具响应的 **完整序列化 JSON-RPC** 预算为 256 KiB，计入实际 `id`、`request_id`、`structuredContent` 及文本客户端使用的转义副本，不计 HTTP 头和 SSE 帧。JSON-RPC id 的序列化 JSON 最多 1024 字节，HTTP 中间件必须在 rmcp/工具执行前以 413 拒绝超限 id。超限错误本身也必须有界；已知发送结果不能因输出超限而丢失。客户端使用原 `operation_key` 调用 `get_operation` 后再决定后续操作。其他发现/握手响应按固定结构生成，不应混称为已经覆盖工具响应预算。

**当前已知偏差：**JSON Schema 的 `minLength/maxLength` 表示字符数，现有 `server::validate` 使用 Rust `str::len()` 计算 UTF-8 字节数，底层文件夹名也有字节限制。文档整理未修复此实现；不能声称 Unicode 输入已与 Schema 完全一致。后续应把标准字符约束与额外资源字节预算分开定义并验证，见 [待闭环事项](../CONTRIBUTING.md#待闭环事项)。

## MCP and mail

`get_email` 的正文先受 MIME 解码和正文长度选项约束，再由 `server::bounded_email_result` 按实际 JSON-RPC 响应二分选择可容纳的最长 UTF-8 前缀。计量包含真实 id、request_id、全部元数据及两份 JSON 内容；需要额外截断时设置 `body_truncated=true`，保留已有截断标记。`body_byte_limit` 仍表示正文选项与解析硬上限的交集，实际前缀可能更短。非正文元数据不裁剪，空正文仍超限时保留有界 `LIMIT_EXCEEDED`。其他工具和发送结果继续使用原有预算及状态降级路径。

服务使用官方 Rust MCP SDK rmcp 3.8.0 与 Streamable HTTP。IMAP 使用 async-imap 和证书校验的 TLS；SMTP 使用 lettre 的隐式 TLS 或必须成功的 STARTTLS。每个需要邮箱后端的操作获取两个执行槽位之一，连接归单次操作所有，在完成、超时或解析失败后关闭。只读的本地账户/发送状态查询不需要邮箱连接。

读取必须使用 `EXAMINE` 和 `BODY.PEEK`。状态 FETCH 只接受所请求 UID 的结果，忽略其他 UID 或无 UID 的非请求更新；目标不存在返回 `NOT_FOUND`，目标存在但 `FLAGS ()` 为空应成功，后出现的同 UID flags 覆盖先前值。不得以关闭会话为由引入 `CLOSE/EXPUNGE` 等额外副作用。

连接、原始响应和单封邮件都必须有预算；`BudgetStream` 对连接累计读取字节计量；`Client::with_buffer_limit` 在真实 parser 的 `decode_needs`、扩容及剩余数据重置前执行同一个应用字节上限（`max_message + 2 MiB`，另保留上游 512 MiB 硬上限）。已用字节与 parser 所需追加字节采用 checked arithmetic；初始化、分块取整和替换缓冲区同样不超过该上限。移除独立的行尾 literal 猜测扫描，合法响应文本中的花括号不会被当作 literal。预算约束每个 parser 缓冲块，非进程 RSS；增长时新旧块、已交付响应及协议对象可能同时存活。列表和搜索最多扫描 500 个 UID、返回 50 条摘要，批量读取最多 10 封并保留逐项结果。具体字节、结构及输出限额见 README；调整数值仍须满足 D3 并评估客户端兼容性。

IMAP 文本模式固定为 rev1：所有邮箱参数编码为 modified UTF-7，LIST 名称解码为 Unicode 后验证规范往返。不得直接把 UTF-8 字节放入 quoted search；非 ASCII 查询使用 `CHARSET UTF-8` 与同步 literal，等待 continuation 和对应命令的 tagged OK。`BADCHARSET` 返回 `UNSUPPORTED_CHARSET`，不降级为含义不同的查询。当前未协商 `UTF8=ACCEPT` 或 IMAP4rev2。

async-imap 0.12 的 APPEND flags 原样序列化，必须传入 `(\Draft)` 这样的带括号列表。合成 duplex 回归覆盖 literal continuation 及 OK/NO/BAD。vendored completion-status 补丁只允许匹配 tag 的 OK 成功；NO/BAD、缺失终结响应和错误 tag 均失败。来源、退出条件与审计方式见 [PATCHES.md](../vendor/async-imap/PATCHES.md)。

`mail/attachments.rs` 使用现有有界整封邮件与 MIME 解析发现附件，统一 `get_email`、列表、内联 Base64 和二进制下载的索引与附件子树边界。`download_attachment` 保持原契约；新增 `prepare_attachment_download` 保存最多 16 个主体绑定、300 秒有效的元数据句柄。GET 再核对 UIDVALIDITY 并重新读取整封邮件；不引入 BODYSTRUCTURE 或单部分 FETCH。两份二进制传输许可持有至响应消费或丢弃，邮箱连接仍共用原槽位与预算。

`mail/insights.rs` 提供有界标识符图会话、精确 Message-ID 定位、严格 STATUS 统计、脱敏诊断与正文展示。头部缓存最多 128 项/2 MiB、30 秒，只保存不可变头部并以新 UIDVALIDITY 失效；正文共用 `mime`/`html` 选择器。统计用 `imap_text::status_counts` 验证完整字段和命令完成。批量修改使用明确 UID、总体期限和逐项结果，失败项保守为 unknown、后续 unprocessed；没有持久化修改日志，重试前必须检查邮箱。范围与限制见 [能力说明](capabilities.md)。

## MIME and HTML

邮件正文、主题、头部和附件均按不可信输入处理。构建 MIME 结构前检查共享的节点、深度和头部预算；transfer/charset 解码前预留预算。不得恢复对不可信输入调用 `mailparse::parse_mail`；回复和草稿发送只需要有界头部。boundary 按完整分隔符匹配，接受允许的尾部空白，不能把相同前缀当作分隔符。

正文选择排除显式 attachment 或带文件名的部件及其整个子树；`multipart/alternative` 选一个受支持表示并优先纯文本，`multipart/related` 只选主部件。读取和转发共用选择逻辑，附件列表和下载共用分类规则，multipart 附件可整体下载。真实空正文与“不支持或无法完整解释的正文”必须区分。

依据 [RFC 2046 §5.1.4](https://www.rfc-editor.org/rfc/rfc2046#section-5.1.4)，alternative 每个候选表示同一份信息，选择必须覆盖候选的完整内联正文。纯文本探测遇到非空 HTML 时判定该候选不完整，回滚其中已收集的所有片段（如 plain 签名），继续寻找完整纯文本表示；不存在时才允许 HTML 转换。此规则沿 mixed、嵌套 alternative 和 related 主部件递归传播，附件仍排除；真正空的 HTML 不引入待转换内容，真正空的 plain 仍是有效完整表示。探测不解码、不分配正文，最终选中叶子继续共用既有解码预算。

HTML 回退只用有界 tokenizer sink，不引入 DOM/layout 引擎、不执行脚本或加载资源。tokenizer 前检查输入和属性复杂度，处理时继续限制 token 与输出；属性复杂度必须覆盖无空白的相邻 quoted attributes 和 slash recovery。tokenizer-only sink 无法解释 SVG/MathML 外来内容及其 CDATA，出现时即使已有可见文字也拒绝整段正文。其他非空但无法完整解释的内联内容同样显式失败，转发必须在发送 claim 前拒绝不完整原文。所有新构建纯文本邮件显式声明 UTF-8，保持转发中的 Unicode 内容。

## Send lifecycle

发送先提交 operation key、参数摘要与 Message-ID，再进入 SMTP。参数摘要包含工具名与参数；并发请求通过同一去重键竞争执行权，重复调用读取已存状态。SMTP 最终响应丢失、超时或取消后不能猜测已失败，更不能自动重试。进程重启将未完成的 `sending` 记录改为 `unknown`。操作键默认持续保留；保留和迁移规则见维护文档。

SMTP 使用 lettre 的低层 connection/command/message API，保留 TLS、AUTH、SMTPUTF8 和 8BITMIME 策略。连接及事务共用发送期限；最终 DATA 250 确认后先记录 `accepted`，明确拒绝先记录 `rejected`，再执行独立的最多一秒 QUIT 清理。`unknown` 或取消的事务直接关闭，不 QUIT、不进连接池。执行槽位持有到清理结束，避免积累清理任务。HTTP/MCP 调用者断开不能把发送生命周期变成不受记录的重试。

已知 SMTP 接受结果不得因后续日志/数据库更新或清理失败而改为失败。操作记录更新失败时，本次响应保留 `accepted` 并附 `journal_update_failed`；这不等于数据库已经成功保存该结果，后续必须检查操作状态及服务商记录。

草稿发送使用同一个有界规范化头部缓冲区做收件人解释和隐私过滤，在 claim/SMTP 前验证字段名、续行及头/体分隔符，并明确拒绝不支持的 `Resent-*` 语义。BCC 参与信封投递但不进入发送头部；可用于 DATA 的正文原始字节（包括 Base64/quoted-printable 编码附件）保持不变，源草稿保留并返回 `draft_retained`。在 `prepare_draft` 和共同 `claim_send` 边界验证 CRLF、无 NUL、每行最多 998 原始字节（dot-stuffing 新增点不计）；不合规返回固定 `UNSUPPORTED_DATA_FORMAT`，不 claim、不连接，不盲目规范化二进制/MIME 正文。末行无 CRLF 仍允许，由现有 lettre DATA codec 补终结。`SmtpSender::submit` 同样在连接前做防御检查，不将确定未提交的非法格式标为 unknown。该限制不代表协商 BINARYMIME/BDAT，8bit 数据仍需能力协商。`unsupported_data_is_rejected_before_journal_claim`、`canonical_data_preserves_content_and_codec_final_crlf` 和 `data_line_validation_uses_source_octets_and_allows_final_line` 对应 D3/D4/D5 的发送前验证边界。

## Authentication

Rauthy 0.37.2 是选用的身份服务，使用嵌入式 Hiqlite/SQLite，负责 authorization code/PKCE、用户登录、refresh rotation 和会话持久化。邮件服务对每次进入 MCP 的 HTTP 请求校验 RS256 签名、issuer、resource audience、expiry、nbf、scope 与 subject allowlist；不能把已有 MCP 会话当作免验凭据。公开发现信息和健康检查保持各自有限职责。

JWKS 的硬缓存期限为 300 秒，不因刷新失败延长。刷新 I/O 使用独立协调器，仍有效的已缓存 key 不等待其他 kid 的网络刷新；成功和失败刷新结果共享 10 秒冷却。JWKS 暂不可用返回 HTTP 503 与 `Retry-After`，无效令牌返回 401。JWT 选择 AWS-LC 后端时的动机是避开当时锁文件中存在已知风险的 RustCrypto 传递依赖；持续安全状态以当前依赖审计为准。

静态 OAuth 客户端明确回调 allowlist，生产与测试分离用户和客户端。Rauthy 允许提前 refresh，保留轮换和五秒宽限。单节点 Hiqlite 使用 immediate durable Raft log sync、一个读连接、一个 HTTP worker 和一次并发 Argon2id 哈希（32 MiB、三次迭代、两通道）。多会话互不干扰、提前刷新、重启恢复及资源观测是认证/部署变更的验收内容。

部署时保留 WebAuthn origin 的显式 `:443`，ChatGPT callback 精确复制客户端当前设置界面值。Rauthy 0.37.2 的 discovery 遗漏其支持的 public-client `none`，按 [deployment](deployment.md) 修正；升级或变更域名、端点、scope 时重新导出核对。登录限流不覆盖静态 asset/theme；验收全部登录页脚本/样式及突发加载，HTML 200 本身不代表登录页可用。

## Local operation store

SQLite 仅保存主体、发送键、参数摘要、Message-ID、时间戳和结果，不保存邮件正文或收件人列表。WAL 允许读写并行，FULL 同步使发送意图在网络提交前落盘；事务保持短小且不跨网络调用。数据库位于本地文件系统，只由一个服务进程拥有，避免另一个进程在启动恢复时改写正在发送的记录。

备份使用 SQLite backup API 或停止服务后复制完整数据目录；恢复、数据库格式变更及回滚遵循 [maintenance](maintenance.md) 与 [deployment](deployment.md)。压测工作负载、环境和测量结果统一保存在 [validation](validation.md)，一次测量不构成性能承诺。

## Logs and failure handling

JSON 日志使用固定事件名和受控字段，记录服务端生成的 request ID、白名单工具名、耗时、HTTP 状态、稳定错误码、发送结果及服务版本。凭据、token、authorization code、邮件主题、地址、正文、附件和底层库错误原文不得进入日志。库协议日志使用固定过滤器关闭，panic hook 只发固定事件，不格式化 panic payload。部署示例的容器日志按 5 MiB、两份轮换。

工具错误使用结构化内容，批量读取保留逐项结果；发送通过持久化状态和 `get_operation` 恢复判断。`/healthz` 只说明进程/配置健康，不能充当邮箱连通性或真实投递成功证明。邮箱探测使用隔离身份和只读操作，真实写入测试另行取得明确授权。

## Invariants and verification

下表把总纲原则映射到当前实现和已有验证入口。它不宣称每条约束都有自动化覆盖；涉及真实部署的检查需按变更范围补充验收记录，测试名称变化时同步更新映射。

| 原则 | 实现入口 | 现有验证与审阅入口 |
| --- | --- | --- |
| D1 身份与授权 | `auth`、`server::allowed/call_tool`、`main` 接线 | `jwt_trust_boundaries`、`failed_refresh_is_shared_and_expired_keys_fail_closed`、`cached_key_does_not_wait_for_rotation_and_refresh_updates_snapshot`、`schemas_match_policy`；另做隔离 OAuth/登录资产验收 |
| D2 读取无副作用与 UIDVALIDITY | `mail`、`imap_text`、vendored completion boundary | `status_uses_only_target_uid_and_distinguishes_missing_from_empty_flags`、`rev1_mailbox_names_round_trip_through_tools`、`statistics_status_requires_complete_matching_snapshot_and_tag`、`refused_imap_tools_return_backend_error_and_stop_delete`、`tests/imap_completion.rs`；只读邮箱验收核对读前后 flags |
| D3 资源有界 | `BudgetStream`、vendored `ImapStream::Buffer`、`mime`、`html`、`server` | `bounded_parser_rejects_incomplete_response_at_application_limit`、`bounded_parser_accepts_fragmented_text_and_normal_literal`、`bounded_parser_handles_exact_text_budget_and_unknown_need`、vendored `application_limit_rejects_growth_before_allocation`、`headers_and_decode_are_bounded_before_allocation`、`attributes_are_bounded_before_tokenization_without_whitespace`、`final_wire_budget_covers_duplicates_escaping_unicode_and_attachments`、`email_body_prefix_fits_real_wire_budget_and_preserves_metadata`、`long_parsed_email_returns_a_wire_bounded_readable_prefix`、`oversized_rpc_id_is_rejected_before_execution` |
| D4 发送结果与幂等 | `journal`、`mail::send`、`smtp` | `durable_dedup_and_principal_isolation`、`concurrent_claim_has_single_winner`、`smtp_lost_reply_does_not_resend`、`smtp_accepted_is_persisted_before_unresponsive_quit`、`smtp_accepted_survives_quit_timeout_and_journal_failure` |
| D5 隐私与不可信内容 | `mail`、`mime`、`html`、`main` 日志 | `discovery_matches_message_indices_and_multipart_attachment_trees`、`http_download_authorization_binary_headers_and_transfer_capacity`、`draft_envelope_and_privacy_filter_share_canonical_fields`、`ambiguous_draft_headers_are_rejected`、`text_attachments_stay_downloadable_but_leave_body_and_forward`、`html_read_and_forward_preserve_original_content`、`alternative_read_and_forward_require_complete_representations`、`plain_probes_require_all_inline_parts_and_prune_only_attachments`；日志字段和 panic 脱敏仍需代码审阅 |
| D6 简单可维护与明确范围 | 模块边界、工具链、补丁来源、部署模板 | CI 的 fmt/test/clippy/build；`scripts/audit-dependencies.sh`；`scripts/test-docker-context.sh` 和生产 Dockerfile 构建；升级/恢复证据见维护与部署指南 |

vendored 构建依赖必须由 `.dockerignore` 明确允许并复制进 Docker builder；允许范围不能扩大到 vendor 同级目录、秘密或部署数据。Docker context 检查使用合成哨兵与真实构建器。普通 `cargo audit` 会跳过 path package，因此本地与 CI 统一运行专用审计脚本，同时独立审阅本地补丁代码；升级时同步更新来源、脚本和回归测试。

## Decision records

以下为当前选择的简短追溯记录，状态均为“采用”。新决策记录背景、决定、代价及重审条件；被替代的决策保留并指向后继记录，不以部署状态覆盖设计历史。

| 编号 | 背景与决定 | 代价与重审条件 |
| --- | --- | --- |
| ADR-001 | 为控制常驻资源和运行时数量，邮件服务采用 Rust、官方 rmcp、async-imap 与 lettre；运行结构见上文 | 需维护协议和解析边界；新增 runtime、transport 或连接复用前评估资源、取消与副作用 |
| ADR-002 | 旧 FastMCP-to-Dex 代理共享一个上游 Dex client，新测试登录影响已有 refresh 记录；改用独立 Rauthy 与嵌入式存储，分离测试/生产会话 | Rauthy 版本特定的 discovery、origin 和刷新行为需升级验收。选型时 Keycloak 的受支持数据库不含 SQLite，因常驻内存目标未采用；若部署/存储目标改变可重审 |
| ADR-003 | 为避免重复 SMTP 提交，用本地 SQLite WAL/FULL 在发送前记录意图，不引入常驻独立数据库 | 单进程/本地磁盘约束；不保证 SMTP exactly-once 或跨实例去重。跨实例、存储迁移及保留期变更须重新设计恢复与兼容策略 |
| ADR-004 | async-imap 0.12.0 吞掉流式命令的失败终结状态，采用可追溯的本地补丁和合成协议回归 | 需要额外的上游版本审计及本地代码审阅；只有官方版本通过相关回归后才能移除补丁，详见 PATCHES.md |

旧服务镜像、配置和数据的保留期限是维护决策；架构文档不声明某个生产环境当前仍保留哪些回滚资源。

## Sources checked 2026-10-28

以下为选型和协议依据，版本升级时核对仍适用的部分；链接本身不构成当前版本验收证据。

- [Official Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk/tree/rmcp-v3.8.0)
- [OpenAI MCP authentication](https://developers.openai.com/plugins/build/auth)
- [Keycloak OIDC endpoints](https://www.keycloak.org/securing-apps/oidc-layers)
- [Keycloak supported databases](https://www.keycloak.org/server/db)
- [Keycloak container sizing and hardening](https://www.keycloak.org/server/containers)
- [SQLite WAL and concurrency](https://www.sqlite.org/wal.html)
- [Rauthy refresh configuration](https://sebadob.github.io/rauthy/config/config.html)
- [lettre BCC handling](https://docs.rs/lettre/latest/lettre/message/struct.MessageBuilder.html#method.keep_bcc)
- [async-imap session operations](https://docs.rs/async-imap/latest/async_imap/struct.Session.html)
