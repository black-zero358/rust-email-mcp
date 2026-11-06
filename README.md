# Rust Email MCP

[![CI](https://github.com/black-zero358/rust-email-mcp/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/black-zero358/rust-email-mcp/actions/workflows/ci.yml)
[![许可证 MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**让 ChatGPT 等 MCP 客户端连接你已有的 IMAP/SMTP 邮箱。** 在自己的服务器上运行 Rust Email MCP，即可搜索邮件、阅读正文和跨文件夹会话、下载附件。服务默认只读，按需启用草稿、发送和邮件整理。

[快速开始](#快速开始) · [能力说明](docs/capabilities.md) · [部署指南](docs/deployment.md) · [发布下载](https://github.com/black-zero358/rust-email-mcp/releases) · [贡献指南](CONTRIBUTING.md)

> “在收件箱里找主题含‘会议’的未读邮件，按发件人和日期列出，并提取邮件里的参会链接。”

## 快速开始

仓库提供 **Linux amd64 单节点**部署模板。准备一个已开通 IMAP 的邮箱及应用专用密码或授权码、Docker Compose，以及可从客户端访问的 HTTPS 域名。参考部署使用 Nginx 提供 HTTPS、Rauthy 负责 OAuth，邮件服务通过 Streamable HTTP 提供 MCP。

1. **配置邮箱。** 克隆仓库，按部署指南复制 [.env.example](.env.example) 到 `secrets/mail.env`。填写邮箱凭据、IMAP/SMTP 端点和 OAuth 参数，保留 `MAIL_MODE=read`。单邮箱环境配置在只读模式下也要求填写 `SMTP_HOST`。
2. **启动服务。** 按[单节点部署](docs/deployment.md)配置 Rauthy、Nginx 与发现文档，先启动认证服务并登记 OAuth 客户端，再在仓库根目录运行以下命令。将 `mail.example.com` 替换为实际域名。

   ```sh
   docker compose -f deploy/compose.yaml up -d --build mail
   curl --fail --silent --show-error https://mail.example.com/email/healthz
   ```

   健康检查成功会返回 `"status":"ok"`，表示进程和配置检查通过。

3. **连接并验证。** 将 `MCP_RESOURCE` 的完整 HTTPS 地址填入客户端，完成 OAuth 登录。先调用 `list_accounts` 和 `list_mailboxes`，确认能看到目标邮箱及文件夹，再做一次[只读查询](#第一次查询)。如需单独检查 IMAP 的连接、TLS 和登录阶段，调用 `diagnose_connectivity`。

### 认证与 ChatGPT 接入

部署模板的公开 MCP 地址示例为 `https://mail.example.com/email/mcp`。将 ChatGPT 显示的回调 URL 精确填入 Rauthy 的 Redirect URI。公共 OAuth 客户端使用 Authorization Code、PKCE S256 和 Refresh Token。发现文档的准备步骤见[认证配置](docs/deployment.md#配置认证)。

| 客户端设置 | 模板值 |
| --- | --- |
| MCP 服务器 URL | `MCP_RESOURCE` 的完整 HTTPS 地址 |
| 认证与注册 | OAuth，预先登记的自定义 OAuth 客户端 |
| OAuth 客户端 ID | `email-mcp`，与 Rauthy 一致 |
| Token 端点认证 | `none`，客户端密钥与注册 URL 留空 |
| 请求 scope | `email:mcp`，基础 scope 为 `openid` |

只读调用成功后，检查访问令牌到期后的自然刷新。登录页、发现文档和刷新问题见[接入排障](docs/troubleshooting.md)。

### 第一次查询

在 MCP 客户端调用 `search_emails`，例如按主题筛选未读邮件。以下是工具参数，可替换主题或加入 `since` 和 `before` 日期条件。

```json
{"mailbox":"INBOX","subject":"会议","seen":false,"limit":10}
```

用 `data.emails` 中某封邮件的 `uid` 和 `data.uid_validity` 调用 `get_email`。下方数值仅作示例。选择 `html_display:"readable"` 可在纯文本正文中保留受支持的链接地址和列表结构。

```json
{"mailbox":"INBOX","uid":7,"uid_validity":9,"html_display":"readable"}
```

多账户调用可带上 `list_accounts` 返回的 `account_id`。搜索有下一页时，把 `next_before_uid` 作为 `before_uid` 传入，并沿用返回的 `uid_validity` 和原筛选条件。空页也可能有下一页，直到 `next_before_uid` 为 `null` 时结束。

## 能做什么

| 任务 | 常用工具与行为 |
| --- | --- |
| 找邮件和读正文 | `list_emails`、`search_emails` 支持组合筛选、分页和可选预览。用 `get_email` 或 `get_emails` 读取单封或批量正文。 |
| 追踪往来 | `read_conversation` 按邮件关联头串起收件箱、已发送和配置的归档目录，可分页追查并保留副本来源。用 `locate_message` 按精确 Message-ID 定位。 |
| 下载附件 | `list_attachments` 查附件，`download_attachment` 返回小文件的 Base64，`prepare_attachment_download` 生成短期认证下载地址。 |
| 访问多个邮箱 | `list_accounts` 和 `list_mailboxes` 列出当前主体可用的账户和文件夹。实例最多配置 16 个邮箱，追加账户固定只读。 |
| 读取离线副本 | 管理员可按账户和文件夹启用 full 缓存，客户端显式选择本地读取，结果带观察时间和覆盖范围。用 `get_cache_status` 查看缓存及同步进度。 |
| 处理草稿、发送与整理 | `save_draft` 保存草稿，`send_email`、`send_draft`、`reply_email`、`forward_email` 提交发送。full 模式提供标记、移动、删除及文件夹管理。 |

附件的 HTTP GET 需要**同一 OAuth 主体**的有效 Bearer 令牌，文件提供 SHA-256 校验。客户端取得文件后交给文件解析工具。[标准适配器](scripts/attachment-client-adapter/README.md)支持下载、校验和落盘。当前附件下载与内容读取使用合成文件离线验收，ChatGPT 文件接收待实测。

持久缓存默认关闭，启用后仍默认在线读取。支持离线的工具可传 `read_source:"cache"`，搜索和 Message-ID 定位使用 `search_source:"cache"`。后台同步需在 full 缓存中单独启用。[缓存配置与同步](docs/deployment.md#持久缓存配置)

离线全文搜索返回正文索引覆盖；`search_index.rebuild_pending` 为 true 时，初始搜索会继续从本地正文重建旧索引。升级后请重新开始先前的搜索分页。[离线查询契约](docs/capabilities.md#诊断统计和缓存)

## 权限与使用限制

| `MAIL_MODE` | 默认邮箱可用操作 |
| --- | --- |
| `read`（默认） | 查询、读取、附件、统计和诊断 |
| `draft` | 只读能力，以及保存草稿 |
| `send` | 草稿模式能力，以及发送、回复和转发 |
| `full` | 发送模式能力，以及标记、移动、删除和文件夹管理 |

- **身份与引用。** 服务逐请求验证 OAuth 令牌，按主体和账户授权。读取使用 IMAP `EXAMINE` 和 `BODY.PEEK`，保持原有邮件标记。邮件引用包含文件夹、UID 和 UIDVALIDITY，多账户调用同时保留账户标识。HTML 在本地转换为纯文本，不执行脚本或请求外部资源。
- **发送。** 客户端先展示收件人与内容并取得用户确认。发送工具要求 `operation_key`，服务在联系 SMTP 前记录发送意图。同一主体、同一操作数据库内复用原键查询同一意图。遇到断连或 `unknown`，用 `get_operation` 和原键查询，结合邮箱及服务商记录人工核对，避免重复投递。参见[发送状态与去重](docs/architecture.md#send-lifecycle)。
- **整理。** 写工具使用原 `default` 邮箱，并检查权限。永久删除和批量删除要求明确确认。批量操作逐项返回结果，中断后先核对已完成、未知和未处理项。参见[批量整理](docs/capabilities.md#批量整理)。

默认单次整封读取上限为 **32 MiB**，按部件下载的解码附件上限为 **32 MiB**，完整 JSON-RPC 工具响应上限为 **2 MiB**。小附件内联 Base64 对应的解码上限为 **512 KiB**，同时受完整响应预算限制。正文可能按响应预算截断，列表和会话可能分页返回。实际账户能力与限制可通过 `list_accounts` 查询。参见[工具参数与响应](docs/capabilities.md)。

## 构建与版本

本页对应 `main` 的 0.3.0 源码。从已发布版本安装时，使用该版本的 Linux amd64 二进制、`SHA256SUMS` 和发行说明。[下载 v0.2.2](https://github.com/black-zero358/rust-email-mcp/releases/tag/v0.2.2)

从 0.2.2 升级到 0.3.0 前，核对整理结果中的 `accepted`、离线正文索引重建及停止宽限。[0.3.0 升级与回退](docs/maintenance.md#030-升级要点)

从源码构建时使用 [rust-toolchain.toml](rust-toolchain.toml) 固定的 Rust 1.99.1 和仓库锁文件：

```sh
cargo build --release --locked
cargo test --locked
```

## 文档与贡献

| 我想了解 | 从这里开始 |
| --- | --- |
| 配置、部署、多账户、备份与恢复 | [单节点部署](docs/deployment.md) · [环境变量样例](.env.example) |
| 工具参数、分页、附件与离线读取 | [能力说明](docs/capabilities.md) |
| OAuth、登录和连接问题 | [排障手册](docs/troubleshooting.md) |
| 设计、兼容性、发布与依赖审计 | [架构](docs/architecture.md) · [维护规程](docs/maintenance.md) |
| 开发规范、测试和 PR 流程 | [贡献指南](CONTRIBUTING.md) |

反馈问题时，请提供版本、`request_id`、错误码及使用合成数据的复现步骤。漏洞请按[安全策略](SECURITY.md)私下报告。项目采用 [MIT 许可证](LICENSE)。
