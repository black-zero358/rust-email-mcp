# Rust Email MCP

使用 Rust 和官方 rmcp SDK，通过 MCP 管理 IMAP/SMTP 邮箱。提供邮件查询、正文读取、草稿、发送、标记、移动和文件夹管理，适合部署在自己的服务器上供 ChatGPT 等 MCP 客户端使用。

运行结构：

```text
MCP 客户端 ── HTTPS ── Nginx ── Rust Email MCP ── IMAP / SMTP
                        │             │
                        │             └── SQLite 操作记录
                        └── Rauthy ── Hiqlite / SQLite
```

邮件服务使用 rmcp、async-imap、lettre、rustls、rusqlite 和 tracing。认证交给 Rauthy，登录、刷新和邮件工具共享清晰的权限边界。运行时只需两个服务容器。

开发与维护从 [工程纲领与贡献指南](CONTRIBUTING.md) 开始；模块与设计约束见 [架构](docs/architecture.md)，版本、发布和依赖复核见 [维护规程](docs/maintenance.md)。

## 功能

| 类别 | 工具 |
| --- | --- |
| 账户与文件夹 | `list_accounts`、`list_mailboxes` |
| 邮件查询 | `list_emails`、`search_emails`、`get_email`、`get_emails`、`get_email_status` |
| 附件 | `list_attachments`、`download_attachment`、`prepare_attachment_download` |
| 会话与定位 | `read_conversation`、`locate_message` |
| 诊断与统计 | `diagnose_connectivity`、`get_mailbox_statistics` |
| 草稿与发送 | `save_draft`、`send_draft`、`send_email`、`reply_email`、`forward_email` |
| 发送状态 | `get_operation` |
| 邮件整理 | `mark_email`、`move_email`、`delete_email`、`mark_emails`、`move_emails`、`delete_emails` |
| 文件夹管理 | `create_mailbox`、`rename_mailbox`、`delete_mailbox` |

读取使用 IMAP `EXAMINE` 和 `BODY.PEEK`，保持邮件标记。每个邮件引用由文件夹、UID 和 UIDVALIDITY 组成，避免文件夹重建后误操作旧 UID。

列表和搜索每次扫描最多 500 个 UID，返回最多 50 条摘要且不超过请求 limit。同 UID 的 FETCH 属性合并成一条摘要，异步 FLAGS 更新不会生成空摘要；后到的显式空 FLAGS 可以清空标记，缺失 HEADER 的 UID 不生成摘要。继续查询时传入 `next_before_uid`；空页也可能带有下一页游标。批量读取最多 10 封，结果包含逐项状态；完整获取后的本地 MIME 错误仅影响该项，继续读取后续 UID。原始获取失败（包括协议、连接或字节预算失败）仍停止，后续项标为 `unprocessed`，不自动重试。`completed` 计已尝试项数，包含单项失败，不表示成功数量。

正文返回纯文本，并标记截断状态。单封 MIME 邮件上限 32 MiB。单次工具响应的序列化 JSON 上限 2 MiB，包括 JSON-RPC 外层及实际 id、request_id、structuredContent 和供文本客户端读取的转义副本；不包括 HTTP 头及 SSE 帧。超限返回小型 `LIMIT_EXCEEDED` 工具错误。小附件以 Base64 返回，解码后最多 512 KiB，仍受完整响应预算限制；内联解码上限按总输出预算的四分之一计算，为 Base64 与两份结果封装保留空间。新邮件支持文本；现有草稿发送保留原 MIME 附件。转发工具发送原邮件的纯文本内容。

JSON-RPC id 的序列化 JSON（包括引号、转义和 UTF-8 字节）最多 1024 字节，超过后 HTTP 返回 413，并在执行工具前拒绝请求。此限制确保超限错误也能携带原 id。若写操作执行后结果超限，响应保留已知发送状态并禁止自动重试；使用原 `operation_key` 调用 `get_operation` 查询。此输出预算约束工具响应，其他协议发现/握手响应另按固定结构生成。

MIME 在构建结构时限制为 1024 个节点、32 层嵌套，头部每段最多 128 KiB，累计最多 1 MiB、8192 行（含折行）。正文解码前按编码字节数的四倍预留共享的 32 MiB 预算，覆盖字符集转为 UTF-8 的放大；附件解码也先限制编码输入。超限返回 `LIMIT_EXCEEDED`，因此部分原始大小合规的复杂邮件或长正文仍会被拒绝。回复和草稿发送只解析有界头部，保留草稿正文原始字节。边界分隔行按完整 boundary 匹配，接受尾部空格和制表符；不再把仅具有相同前缀的行当作分隔符。

文件夹名称在工具参数、LIST 返回值和 DRAFTS_MAILBOX/TRASH_MAILBOX 配置中使用 Unicode；返回的 name 可直接传给读取、移动或重命名工具。连接采用 IMAP4rev1，线路上的名称使用 modified UTF-7，包含 & 的名称也会正确转换。非规范或无法安全往返的服务端名称返回 MAIL_BACKEND，避免误引用其他文件夹。ASCII 搜索继续使用 US-ASCII；中文等非 ASCII 查询使用 CHARSET UTF-8 和同步 literal。服务器返回 BADCHARSET 时报告 UNSUPPORTED_CHARSET，不降级为含义不同的查询；未协商 UTF8=ACCEPT 或 IMAP4rev2。
搜索支持日期、发件人、收件人、主题、已读、星标、已回复、附件和大小组合。会话根据关联头组织；跨文件夹定位使用精确 Message-ID。批量整理最多 50 个明确 UID，保留逐项结果，批量删除必须确认。

正文支持完整或去引用模式、长度设置及省略提示。`get_email` 在正文长度上限内，按完整 JSON-RPC 响应预算返回可容纳的最长 UTF-8 正文前缀；额外截断设置 `body_truncated=true`。`body_byte_limit` 表示正文配置上限，实际正文可能因 JSON 转义、元数据和调用 id 而更短；元数据本身超限仍返回 `LIMIT_EXCEEDED`。单封 MIME 邮件上限 32 MiB，单次工具输出上限 2 MiB。附件发现、准备下载及二进制 GET 均通过现有有界整封 MIME 读取，保持统一索引和整体 multipart 附件语义；每封原始邮件和解码后的附件分别受 32 MiB 限制。新邮件支持文本；现有草稿发送保留原 MIME 附件。转发工具发送原邮件的纯文本内容。

`download_attachment` 保留原有内联 Base64 返回格式及输出预算。新增 `prepare_attachment_download` 返回短期下载 URL；客户端需携带同一用户的 OAuth Bearer 令牌。普通链接点击和 ChatGPT 自动文件解析尚未验收。参数与范围限制见 [邮件能力扩展](docs/capabilities.md)。

## 发送行为

发送已有草稿时，收件人解释与隐私头清理共用规范化后的字段名，支持大小写、折行与旧格式字段名尾部空白。缺少冒号、非法字段名、控制字符、孤立续行或缺少头部结束空行的草稿会在发送意图记录和 SMTP 前拒绝。`Resent-*` 块的投递语义暂不支持，返回 `UNSUPPORTED_DRAFT_HEADERS`；请先保存为普通草稿。MIME 正文和二进制附件保留原始字节。

正文选择会排除标为 attachment 的部件及其整个子树；显式但不支持的 Content-Disposition 类型也按附件处理，缺省或空类型保持原有内联语义。带 `filename` 或 Content-Type `name` 的部件同样视为附件。`multipart/alternative` 优先选最后一个完整纯文本表示（包括真正为空的纯文本），没有时再选一个允许 HTML 转换的表示；候选中的 HTML 主体不能被跳过后只留下 plain 签名。`multipart/related` 只选 `start` 指定的主部件，缺省选首部件；mixed 内联内容按顺序读取。附件列表与下载共用识别规则，multipart 附件作为整体提供下载。读取正文和转发使用同一选择逻辑，转发不包含原附件内容。

HTML 在本地提取纯文本，保留实体、Unicode、段落换行和图片 alt 文本，忽略文档元数据、脚本、样式、模板及内嵌框架；不执行脚本、不加载资源，也不保留 CSS 可见性和表格排版。转换前限制解码后的 HTML 为 1 MiB、ASCII 空白、单/双引号、斜杠和等号合计最多 65536 个，以保守限制标记属性复杂度；流式处理最多 32768 个 token、1 MiB 输出，并受共享 MIME 解码预算约束。超限返回 `LIMIT_EXCEEDED`；SVG/MathML 外来命名空间与非空但无法完整解释的内联正文（包括未知或替换型字符集）整段返回 `UNSUPPORTED_BODY`，转发在记录发送意图与 SMTP 前拒绝不完整原文。真正为空的正文仍可读取和转发。所有新构建的纯文本邮件（包括转发）显式声明 `text/plain; charset=utf-8`。

Content-Transfer-Encoding 缺省为 7bit，叶子解码支持 7bit、8bit、binary、Base64 和 quoted-printable，名称不区分大小写。未知或显式空编码的非空内联实体返回 `UNSUPPORTED_BODY`，alternative 可改选其他完整受支持表示；未知编码的 multipart 保持不透明，不按原始字节解释子树。显式附件仍可发现，但未知编码无法按已解码附件下载：内联下载拒绝，二进制 GET 沿既有后端失败路径返回 HTTP 502；不提供未标明的原始编码数据。

发送前由客户端向用户展示收件人和内容，并获得明确授权。所有发送工具必须提供 `operation_key`，同一发送意图重试时复用该值。

SMTPUTF8 需求同时检查信封地址和保留的原始邮件头（含 MIME 子头和未转码封装的邮件头），不将 RFC 2047 编码词解码后的 Unicode 或普通 8 位正文误判为国际化头。只需 8BITMIME 的邮件仍可使用不提供 SMTPUTF8 的服务器；需要 SMTPUTF8 但服务端不支持时，在 MAIL 前拒绝，操作记录为 `rejected`，不提交 DATA。头部检查沿用有界 MIME 结构/头部预算，不解码或渲染正文；非 ASCII 邮件无法在预算内确定头部需求时，连接前确定拒绝。原始草稿正文保持不变。

服务先将操作键、参数摘要和 Message-ID 写入 SQLite，再提交 SMTP。结果有三种：

| 状态 | 含义与后续操作 |
| --- | --- |
| `accepted` | SMTP 服务器已接受邮件。后续投递由邮件服务商处理。 |
| `rejected` | SMTP 服务器明确拒绝。检查配置或收件人，确认后使用新的操作键。 |
| `unknown` | 连接中断、超时或进程重启使最终结果无法确认。先检查邮箱和服务商记录。 |

进行中的操作显示 `sending`。同一服务实例的操作数据库内，相同 OIDC subject 下的同一个 `operation_key` 只会获得一次发送执行权；参数变化会返回 `IDEMPOTENCY_CONFLICT`。不同 subject 的操作键和状态彼此隔离，即使它们使用同一个邮箱。操作记录默认持续保留，便于查询历史结果与阻止重复提交；删除记录、更换主体或恢复旧备份后的去重边界见 [维护规程](docs/maintenance.md)。

SMTP 已接受后，操作记录更新失败会返回 `journal_update_failed`，同时保留本次 `accepted` 结果。草稿发送保留原草稿，并返回 `draft_retained`。BCC 参与 SMTP 信封投递，发送内容中的 BCC 头会被移除。

SMTP 连接与事务共用发送期限。DATA 最终返回 250 后先记录 `accepted`，再单独执行最多 1 秒的 QUIT 清理；清理超时或断连不会改变已知结果。明确的事务拒绝先记录为 `rejected`。最终答复丢失、事务超时或响应无法确认时保持 `unknown`，直接关闭该连接且不复用。现有四个执行槽位持有至清理结束，限制并发连接与清理任务。

## 编译和测试

需要 Rust 1.99.1。仓库包含工具链版本与 Cargo.lock。当前承诺使用这一固定工具链构建；其他 Rust 版本需另行验证。完整验证要求见 [贡献指南](CONTRIBUTING.md#验证与完成标准)。

```sh
cargo build --release --locked
cargo test --locked
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
```

容器交付先运行 `bash scripts/test-docker-context.sh`，用合成哨兵和实际 Docker 构建器验证上下文仅包含 Cargo 清单、源码和所需的 vendored 依赖，再用 `docker build --tag rust-email-mcp:local .` 验证实际 Dockerfile。CI 同步执行上下文检查和镜像构建，避免意外放行 vendor 同级目录或遗漏 path 依赖。

测试覆盖发送状态持久化、并发去重、草稿 BCC 与 MIME 保留、日志写入后置失败、实际字节与 literal 声明限额、参数校验，以及 JWT 信任边界。测试邮件使用本地合成数据。

本地 SQLite 压测使用独立的新数据库：

```sh
cargo run --release --example benchmark_journal -- /tmp/email-mcp-benchmark.sqlite
```

输出包含四个线程、1,000 次操作的中位数、p95、最大耗时及 SQLite 版本。每次操作包含提交发送意图、提交结果和查询结果。

## 配置

JWKS 公钥成功加载后缓存 300 秒，超过期限后必须刷新成功才可继续使用。网络刷新使用独立协调锁；有效缓存中的公钥不等待其他 kid 的刷新。成功或失败的刷新结果共享 10 秒冷却，限制未知 kid 或依赖故障造成的请求放大。冷却从请求完成时开始；取消中的刷新从尝试开始时计时。过期公钥不因失败延长信任期限。

JWKS 暂不可用时认证返回 HTTP 503 和 `Retry-After: 10`，客户端可稍后重试原请求。无效令牌或成功刷新后仍不存在的 kid 返回 HTTP 401。公钥在有效缓存期内仍可继续验证令牌，因此 IdP 删除公钥的生效延迟最多为缓存期限。

复制 [.env.example](.env.example)，填写自己的邮箱和 OIDC 参数。由进程管理器或容器注入环境变量。

| 变量 | 用途 |
| --- | --- |
| `EMAIL_ADDRESS`、`EMAIL_PASSWORD` | 邮箱地址和服务商应用专用密码或授权码 |
| `IMAP_HOST`、`IMAP_PORT` | IMAP TLS 地址，端口默认 993 |
| `SMTP_HOST`、`SMTP_PORT` | SMTP 地址，端口默认 465 |
| `SMTP_STARTTLS` | `false` 使用隐式 TLS；`true` 使用必须成功的 STARTTLS |
| `DRAFTS_MAILBOX`、`TRASH_MAILBOX` | 邮箱服务商的草稿和废纸篓名称 |
| `MAIL_MODE` | `read`、`draft`、`send`、`full`，默认 `read` |
| `MCP_RESOURCE` | 公网 MCP 完整 HTTPS 地址 |
| `OIDC_ISSUER` | 身份服务发现文档中的精确 issuer，包含其尾部斜线 |
| `OIDC_AUDIENCE` | 访问令牌目标资源，一般与 `MCP_RESOURCE` 相同 |
| `OIDC_SUBJECTS` | 获准使用此邮箱的用户 subject ID，逗号分隔 |
| `OIDC_SCOPE` | 必需权限，默认 `email:mcp` |
| `BIND`、`DATABASE_PATH` | 监听地址与 SQLite 文件位置 |

QQ 邮箱通常使用 `imap.qq.com:993` 和 `smtp.qq.com:465`，`SMTP_STARTTLS=false`。iCloud 使用 `imap.mail.me.com:993` 和 `smtp.mail.me.com:587`，`SMTP_STARTTLS=true`。凭据使用服务商签发的应用专用密码或授权码。

每个实例配置一个邮箱。需要多个邮箱时，使用独立实例、数据目录和 OIDC 资源地址，分别设置 subject 白名单。

## 认证与 ChatGPT 接入

Rauthy 使用嵌入式 Hiqlite/SQLite，省去独立数据库容器。建议配置：

- 仅启用 Authorization Code、PKCE S256 和 Refresh Token。
- 明确填写客户端回调地址、资源 allowlist 和 `email:mcp` scope。
- `disable_refresh_token_nbf=true`，允许客户端提前刷新；保留刷新轮换和短暂重用宽限期。
- 单节点设置 `log_sync='immediate'`，单 HTTP worker、单密码哈希线程。
- Argon2id 至少采用 32 MiB 内存、3 次迭代、2 个并行通道。
- 测试使用独立用户和客户端。

在 ChatGPT 创建自定义 MCP 连接，使用以下设置。先按[部署指南](docs/deployment.md)完成 Rauthy 客户端与发现文档配置。

| ChatGPT 设置 | 本项目示例值 |
| --- | --- |
| MCP 服务器 URL | `MCP_RESOURCE` 的完整 HTTPS 地址 |
| 认证 / 客户端注册方法 | OAuth / 自定义 OAuth 客户端 |
| OAuth 客户端 ID | `email-mcp`，与 Rauthy 中的客户端一致 |
| OAuth 客户端密钥 | 留空，使用公共 PKCE 客户端 |
| Token 端点身份验证方法 | `none` |
| 默认请求的权限范围 | `email:mcp` |
| 基础授权范围 | `openid` |
| 注册 URL | 留空，此配置使用预注册客户端 |
| 可选 OIDC 邮箱身份功能 | 当前最小配置关闭；启用前配置相应身份 scope |

将 ChatGPT 页面显示的回调 URL 精确复制到 Rauthy 的 Redirect URI。授权地址、Token URL、issuer 和资源地址由发现文档提供；检查其域名和路径与实际部署一致。

完成登录后，先调用 `list_accounts` 和 `list_mailboxes`，再确认访问令牌到期后的自然刷新。遇到设置验证失败、登录页 `500` 或 `ERR_CONNECTION_CLOSED`，参阅[接入排障](docs/troubleshooting.md)。

从旧 FastMCP/Dex 服务迁移时，重新创建或重新授权连接，使客户端获取新的认证发现信息。旧令牌属于旧身份服务。

服务逐请求验证签名算法、issuer、audience、有效期、scope 与 subject 白名单。`/.well-known/oauth-protected-resource` 提供资源发现信息；无效访问令牌返回带资源元数据地址的 `401`。

## 部署与运维

完整步骤、Compose、Nginx 和 Rauthy 配置见 [单节点部署](docs/deployment.md)。

升级前先按 [维护规程](docs/maintenance.md)核对版本、产物来源、数据兼容和发送记录连续性。

使用 Dockerfile 构建邮件服务，在反向代理后监听内网地址。数据目录由容器用户独占，根文件系统设为只读，移除 Linux capabilities，设置 `no-new-privileges`、内存限额和容器重启策略。

日志输出 JSON，字段包括 request ID、工具名称、耗时、HTTP 状态、错误码和发送结果。工具参数、邮件正文、主题、地址、附件与令牌不进入运行日志。关闭依赖库的协议调试日志。Docker 日志建议 `max-size=5m`、`max-file=2`。

`/healthz` 检查进程与配置。`diagnose_connectivity` 在认证后检查 IMAP 连接、TLS、登录和能力阶段，返回脱敏结果；不探测 SMTP。`get_mailbox_statistics` 使用 STATUS 获取数量，并可按有限日期及 UID 范围统计分布。

备份时同时保留 Rauthy 配置、加密密钥、完整 Hiqlite 数据目录和邮件操作数据库。SQLite 在线备份使用 backup API；停机备份时复制数据库及其 WAL/SHM 文件。恢复先在隔离端口验证，再切换反向代理。

升级前保存旧镜像、配置与数据备份。新版本在独立端口完成 OAuth 登录与刷新、重启恢复、工具发现和邮件只读检查后，再切换流量。回滚恢复旧镜像和对应身份服务入口，并重新确认客户端认证状态。

## 安全与贡献

邮件正文、主题和附件均按不可信内容处理。工具 schema 与服务端校验同时约束参数，禁止服务器文件路径附件。永久删除要求 `confirm=true`，客户端负责取得用户明确确认。服务只使用目标 UID 的删除命令，文件夹移动要求 IMAP MOVE 能力。

请勿在 issue 中提交邮件原文、授权码、访问令牌或生产配置。问题反馈可提供版本、request ID、错误码和使用合成数据复现的步骤。安全问题处理方式见 [SECURITY.md](SECURITY.md)。

架构、选型依据与官方资料见 [docs/architecture.md](docs/architecture.md)。许可证为 [MIT](LICENSE)。感谢官方 Rust MCP SDK、async-imap、lettre、mailparse、SQLite 和 Rauthy 社区。

## 依赖补丁与审计

async-imap 0.12.0 的流式命令终结状态补丁位于 vendor/async-imap，来源、许可证和维护说明见 [PATCHES.md](vendor/async-imap/PATCHES.md)。仅匹配命令 tag 的 OK 表示成功，NO/BAD 或缺失终结响应返回错误。

本地和 CI 统一运行 bash scripts/audit-dependencies.sh。普通 cargo audit 会跳过 path 依赖；脚本在临时锁文件中恢复上游版本的注册表身份，使审计覆盖本地补丁对应的上游版本及全部锁定依赖。实际 Cargo.lock 不变，补丁代码仍需独立审核。升级依赖时必须同步核对脚本和回归测试。

## 0.2.0 的 SMTP DATA 格式约束

源码 0.2.0 收紧此前被接受的非规范发送输入；源码版本不代表已发布产物或生产已部署。发送前、记录操作意图前校验 DATA：行分隔必须为 CRLF、每行最多 998 原始字节（不计透明传输新增的点），不接受 NUL。不符合时返回 `UNSUPPORTED_DATA_FORMAT`，`retryable=false`，不创建操作记录或建立 SMTP 连接。末行可不含 CRLF，由现有 SMTP codec 补终结。

规范 CRLF 草稿、7bit/8bit 内容及 Base64/quoted-printable 附件保持原字节；8bit 与 SMTPUTF8 仍须现有能力协商。不会盲目转换 MIME 正文。旧草稿若包含裸 LF/CR、NUL 或超长行，应由邮件客户端按 MIME 语义修复或编码后再发送。头部原有 LF 规范化仍支持，正文不自动重写。无数据库迁移；已有 accepted/unknown 去重记录继续保留，未知发送不能自动重试。
## 0.2.1 宽松容量与读取容错

默认单封32 MiB、每连接累计128 MiB、完整工具响应2 MiB；4个邮箱执行槽，最多20个进入执行/等待的工具请求，等待槽位最多10秒，操作超时60秒。会话完整头与MIME单头块统一为128 KiB，缓存256项/16 MiB/60秒；正文长度参数最高512 KiB，批读每封最多64 KiB，会话累计正文最多256 KiB。HTTP请求体仍为256 KiB。

会话扫描在完整FETCH后跳过超大或解析失败的候选，通过 `skipped_candidates` 返回UID及脱敏错误；已关联邮件的本地内容错误放入 `message_errors`，同时保留已读取邮件与 `omitted_uids`。种子头或传输/协议错误明确失败。`get_emails` 和会话正文按完整JSON-RPC预算截断UTF-8前缀，保留元数据和逐项结果；元数据本身超限仍返回有界错误。

默认完整头上限扩大后，18 KiB级长邮件头可直接读取。配置无需迁移，SQLite结构与操作键不变。部署模板的mail内存额度提高为1 GiB。SMTP行格式、未知发送去重、UIDVALIDITY、JWT与TLS校验保持原规则。
