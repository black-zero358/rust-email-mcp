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

## 功能

| 类别 | 工具 |
| --- | --- |
| 账户与文件夹 | `list_accounts`、`list_mailboxes` |
| 邮件查询 | `list_emails`、`search_emails`、`get_email`、`get_emails`、`get_email_status` |
| 附件 | `download_attachment` |
| 草稿与发送 | `save_draft`、`send_draft`、`send_email`、`reply_email`、`forward_email` |
| 发送状态 | `get_operation` |
| 邮件整理 | `mark_email`、`move_email`、`delete_email` |
| 文件夹管理 | `create_mailbox`、`rename_mailbox`、`delete_mailbox` |

读取使用 IMAP `EXAMINE` 和 `BODY.PEEK`，保持邮件标记。每个邮件引用由文件夹、UID 和 UIDVALIDITY 组成，避免文件夹重建后误操作旧 UID。

列表和搜索每次扫描最多 500 个 UID，返回最多 50 条摘要。继续查询时传入 `next_before_uid`；空页也可能带有下一页游标。批量读取最多 10 封，结果包含逐项状态。

正文返回纯文本，并标记截断状态。单封 MIME 邮件上限 8 MiB，单次工具输出上限 256 KiB，小附件以 Base64 返回，解码后上限 128 KiB。新邮件支持文本；现有草稿发送保留原 MIME 附件。转发工具发送原邮件的纯文本内容。

MIME 在构建结构时限制为 256 个节点、20 层嵌套，头部每段最多 32 KiB，累计最多 256 KiB、2048 行（含折行）。正文解码前按编码字节数的四倍预留共享的 8 MiB 预算，覆盖字符集转为 UTF-8 的放大；附件解码也先限制编码输入。超限返回 `LIMIT_EXCEEDED`，因此部分原始大小合规的复杂邮件或长正文仍会被拒绝。回复和草稿发送只解析有界头部，保留草稿正文原始字节。边界分隔行按完整 boundary 匹配，接受尾部空格和制表符；不再把仅具有相同前缀的行当作分隔符。

## 发送行为

发送前由客户端向用户展示收件人和内容，并获得明确授权。所有发送工具必须提供 `operation_key`，同一发送意图重试时复用该值。

服务先将操作键、参数摘要和 Message-ID 写入 SQLite，再提交 SMTP。结果有三种：

| 状态 | 含义与后续操作 |
| --- | --- |
| `accepted` | SMTP 服务器已接受邮件。后续投递由邮件服务商处理。 |
| `rejected` | SMTP 服务器明确拒绝。检查配置或收件人，确认后使用新的操作键。 |
| `unknown` | 连接中断、超时或进程重启使最终结果无法确认。先检查邮箱和服务商记录。 |

进行中的操作显示 `sending`。相同账户下的同一个操作键只会获得一次发送执行权；参数变化会返回 `IDEMPOTENCY_CONFLICT`。操作记录默认持续保留，便于查询历史结果与阻止重复提交。

SMTP 已接受后，操作记录更新失败会返回 `journal_update_failed`，同时保留本次 `accepted` 结果。草稿发送保留原草稿，并返回 `draft_retained`。BCC 参与 SMTP 信封投递，发送内容中的 BCC 头会被移除。

## 编译和测试

需要 Rust 1.99.1。仓库包含工具链版本与 Cargo.lock。

```sh
cargo build --release --locked
cargo test --locked
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
```

测试覆盖发送状态持久化、并发去重、草稿 BCC 与 MIME 保留、日志写入后置失败、实际字节与 literal 声明限额、参数校验，以及 JWT 信任边界。测试邮件使用本地合成数据。

本地 SQLite 压测使用独立的新数据库：

```sh
cargo run --release --example benchmark_journal -- /tmp/email-mcp-benchmark.sqlite
```

输出包含四个线程、1,000 次操作的中位数、p95、最大耗时及 SQLite 版本。每次操作包含提交发送意图、提交结果和查询结果。

## 配置

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

在 ChatGPT 创建自定义 MCP 连接，填入 `MCP_RESOURCE`，认证选择 OAuth，并填写预注册客户端 ID。公共 PKCE 客户端无需客户端密钥。完成登录后，先调用 `list_accounts` 和 `list_mailboxes`。

从旧 FastMCP/Dex 服务迁移时，重新创建或重新授权连接，使客户端获取新的认证发现信息。旧令牌属于旧身份服务。

服务逐请求验证签名算法、issuer、audience、有效期、scope 与 subject 白名单。`/.well-known/oauth-protected-resource` 提供资源发现信息；无效访问令牌返回带资源元数据地址的 `401`。

## 部署与运维

完整步骤、Compose、Nginx 和 Rauthy 配置见 [单节点部署](docs/deployment.md)。

使用 Dockerfile 构建邮件服务，在反向代理后监听内网地址。数据目录由容器用户独占，根文件系统设为只读，移除 Linux capabilities，设置 `no-new-privileges`、内存限额和容器重启策略。

日志输出 JSON，字段包括 request ID、工具名称、耗时、HTTP 状态、错误码和发送结果。工具参数、邮件正文、主题、地址、附件与令牌不进入运行日志。关闭依赖库的协议调试日志。Docker 日志建议 `max-size=5m`、`max-file=2`。

`/healthz` 检查进程与配置。邮箱连通性通过单独的只读验收检查 IMAP；SMTP 登录探测应与真实发送分开进行。

备份时同时保留 Rauthy 配置、加密密钥、完整 Hiqlite 数据目录和邮件操作数据库。SQLite 在线备份使用 backup API；停机备份时复制数据库及其 WAL/SHM 文件。恢复先在隔离端口验证，再切换反向代理。

升级前保存旧镜像、配置与数据备份。新版本在独立端口完成 OAuth 登录与刷新、重启恢复、工具发现和邮件只读检查后，再切换流量。回滚恢复旧镜像和对应身份服务入口，并重新确认客户端认证状态。

## 安全与贡献

邮件正文、主题和附件均按不可信内容处理。工具 schema 与服务端校验同时约束参数，禁止服务器文件路径附件。永久删除要求 `confirm=true`，客户端负责取得用户明确确认。服务只使用目标 UID 的删除命令，文件夹移动要求 IMAP MOVE 能力。

请勿在 issue 中提交邮件原文、授权码、访问令牌或生产配置。问题反馈可提供版本、request ID、错误码和使用合成数据复现的步骤。安全问题处理方式见 [SECURITY.md](SECURITY.md)。

架构、选型依据与官方资料见 [docs/architecture.md](docs/architecture.md)。许可证为 [MIT](LICENSE)。感谢官方 Rust MCP SDK、async-imap、lettre、mailparse、SQLite 和 Rauthy 社区。

## 依赖补丁与审计

async-imap 0.12.0 的流式命令终结状态补丁位于 vendor/async-imap，来源、许可证和维护说明见 [PATCHES.md](vendor/async-imap/PATCHES.md)。仅匹配命令 tag 的 OK 表示成功，NO/BAD 或缺失终结响应返回错误。

本地和 CI 统一运行 bash scripts/audit-dependencies.sh。普通 cargo audit 会跳过 path 依赖；脚本在临时锁文件中恢复上游版本的注册表身份，使审计覆盖本地补丁对应的上游版本及全部锁定依赖。实际 Cargo.lock 不变，补丁代码仍需独立审核。升级依赖时必须同步核对脚本和回归测试。
