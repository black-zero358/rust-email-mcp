# 邮件能力扩展

当前验证证据见 [验证记录](validation.md)，待闭环验收见 [贡献指南](../CONTRIBUTING.md#待闭环事项)。所有测试使用合成邮件或本地组件；生产部署、真实邮件修改、ChatGPT 客户端附件接收另行验收。

## 筛选与分页

`search_emails` 支持组合 `query`、`from`、`to`、`subject`、`since`、`before`、`seen`、`flagged`、`answered`、`has_attachment`、`min_size`、`max_size`。日期采用 `YYYY-MM-DD`，`since` 包含当日、`before` 不包含当日，按 IMAP INTERNALDATE 的日历日匹配。大小为整封邮件编码后的字节数，最小值和最大值均包含边界。

例如查询本周未读邮件：

```json
{"mailbox":"INBOX","since":"2026-10-26","before":"2026-11-02","seen":false,"limit":20}
```

未回复筛选使用 `answered:false`，含义是服务器未设置 `\\Answered` 标记；不能据此断定所有客户端都没有回复过。收件人 `to` 对应 To 头，不包含隐式 BCC 投递关系。

服务先使用 UID SEARCH 过滤日期、文本、地址、大小和标记。IMAP 没有通用的附件搜索条件，`has_attachment` 在有限候选中使用有界整封邮件读取与 MIME 解析检查附件。每页只覆盖有限 UID 窗口；必须持续传入 `next_before_uid` 和返回的 `uid_validity`，直到游标为空。空页不等于没有更早的匹配。

中文等非 ASCII 筛选词使用 IMAP4rev1 的 `CHARSET UTF-8` 与同步 literal，发送 UTF-8 字节前等待 continuation，命令结束必须收到对应 tag 的 OK。`BADCHARSET` 返回 `UNSUPPORTED_CHARSET`；不协商 `UTF8=ACCEPT` 或依赖 `LITERAL+`。组合文本字段分别在同一 500 UID 窗口搜索并取交集；服务器的字符集与搜索匹配规则仍影响结果。

## 附件与兼容性

`download_attachment` 保持原有 `content_type`、`size`、`base64` 返回字段和工具输出预算，旧客户端无需迁移。新增 `list_attachments` 提供与 `get_email` 一致的索引，新增 `prepare_attachment_download` 接受文件夹、UID、UIDVALIDITY 和 `attachment_index`，返回认证二进制 GET URL。

附件发现、URL 准备和实际 GET 均使用已有有界整封读取及 MIME 解析；GET 会重新核对 UIDVALIDITY 并读取同一消息。当前不会发起 BODYSTRUCTURE 或单部分 FETCH，不能发现或下载超出整封邮件上限的内容。这样保持既有附件分类、RFC2231 文件名意图和整体 multipart 附件语义；附加 `.eml` 作为整体附件，不枚举其内部附件。每封原始邮件及解码附件均受 8 MiB 限制；元数据和筛选还共用单连接响应预算，超过预算明确失败。

返回的 `download_url` 需要同一用户的有效 OAuth Bearer 令牌。句柄有效期 300 秒，每实例最多保存 16 个描述符，不缓存附件字节；重启后失效。它不能作为公共分享链接。最多两个二进制下载并发，响应被消费或丢弃后释放传输名额；IMAP 读取还使用共享邮箱槽位。附件列表使用 `next_attachment_offset` 分页。

普通浏览器点击链接不会自动携带 MCP 令牌，客户端必须支持认证下载；ChatGPT 自动读取 PDF/Excel 尚未验收。公开路由为 `MCP_RESOURCE/attachments/{token}`，需转发至内部 `/mcp/attachments/{token}` 并关闭包含句柄的访问日志。响应使用 `no-store`、`nosniff` 和 attachment disposition；客户端负责安全解析文件内容。


## 批量整理

`mark_emails`、`move_emails`、`delete_emails` 接受同一文件夹下明确的 UID 列表，最多 50 个，不接受范围字符串或重复 UID。必须提供 UIDVALIDITY。批量删除无论移入废纸篓还是永久删除，都要求用户明确确认并传入 `confirm:true`。

操作逐项返回结果。中断后已完成结果保留，当前项可能为 `unknown`，其余为 `unprocessed`。不要自动重试整个批次；先检查邮箱的当前状态。移动要求 MOVE，永久删除要求 UIDPLUS，并仅对指定 UID 执行 UID EXPUNGE。

## 会话、定位和正文

`read_conversation` 从指定 `uid` 和 `uid_validity` 出发，按 Message-ID、References 和 In-Reply-To 的精确标识符关联同一文件夹的邮件，不使用相同主题作为关联依据。默认检查 100 个候选头，最多 200 个；默认返回 10 封，最多 20 封，累计正文最多 64 KiB。扫描范围、候选上限、遗漏邮件和后续 UID 游标均随结果返回。`next_before_uid` 仅推进候选扫描；`omitted_uids` 列出本次已关联但因数量或响应预算未返回的 UID，可用 `get_emails` 分批读取。跨窗口关联需要客户端继续查询并结合已有结果；不保证单次返回全邮箱内的所有往来。

`locate_message` 接受 `message_id` 和显式 `mailboxes` 列表，最多 20 个文件夹。先以服务端 HEADER 搜索缩小候选，再验证完整 Message-ID，避免子串误匹配。每个文件夹分别返回搜索范围及下一页游标；继续某个文件夹时应单独传入该文件夹和它自己的游标。

`get_email`、`get_emails` 和 `read_conversation` 支持 `body_mode:full|trimmed` 和 `max_body_length`，长度以 UTF-8 字节计，截断保持字符完整。`trimmed` 按常见引用分隔符及标准签名分隔符进行启发式处理；结果分别报告引用、签名省略及长度截断，可用 `full` 重新读取。HTML-only 邮件仍沿用现有纯文本读取行为，不新增 HTML 渲染器。

## 诊断、统计和缓存

`diagnose_connectivity` 分阶段报告当前认证调用、TCP、TLS、IMAP greeting、登录和能力检查。只返回固定阶段、状态及允许展示的能力，不返回服务器 banner、邮箱身份、主机名或原始错误。缺失或无效令牌由 HTTP 401 表达，JWKS 暂不可用由 HTTP 503 与 Retry-After 表达，工具只有在认证通过后才执行；SMTP 不在诊断范围内。

`get_mailbox_statistics` 以 STATUS 返回文件夹邮件数量、未读数量等。统计必须收到匹配文件夹的全部请求字段和匹配 tag 的 OK；断连、缺失字段及拒绝均返回 MAIL_BACKEND，不把缺失响应解释为零。可选 `date_start` 和 `date_end` 统计最多 31 天的 INTERNALDATE 日分布，结束日期不包含在内。分布只覆盖本次 500 UID 窗口，结果附带范围和游标；总量与分布不是原子快照，不能混为同一时刻的全量统计。

元数据缓存仅保存完整邮件头，每实例最多 128 项、2 MiB，TTL 为 30 秒。键包含文件夹、UIDVALIDITY 和 UID，每次会话先获取当前 UIDVALIDITY，失效时移除旧缓存。正文、附件、可变标记和统计结果不进入缓存，避免旧已读状态误导后续操作。

## 协议依据

- [RFC 3501：IMAP4rev1 SEARCH、FETCH、STATUS 和 UIDVALIDITY](https://www.rfc-editor.org/rfc/rfc3501#section-6.3.10)
- [async-imap 官方 API 说明](https://github.com/chatmail/async-imap/blob/main/_autodocs/api-reference/session.md)
- [RFC 3501：字符串与同步 literal](https://www.rfc-editor.org/rfc/rfc3501#section-4.3)

协议文档用于核对行为；实际编译以 Cargo.lock 锁定的 async-imap 0.12 API 为准。
