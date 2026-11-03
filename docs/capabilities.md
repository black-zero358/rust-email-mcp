# 邮件能力扩展

当前验证证据见 [验证记录](validation.md)，待闭环验收见 [贡献指南](../CONTRIBUTING.md#待闭环事项)。所有测试使用合成邮件或本地组件；生产部署、真实邮件修改、ChatGPT 客户端附件接收另行验收。

## 筛选与分页

读取文件夹名和 `query/from/to/subject` 最多 512 个 Unicode 码点、2048 UTF-8 字节；文件夹 modified UTF-7 编码后最多 8192 字节。Schema 的 `maxLength` 使用码点，额外字节预算由 `x-maxUtf8Bytes` 和 `x-maxModifiedUtf7Bytes` 公布。例如 200 个汉字可直接作为查询词，组合字符按各自码点计数。旧写工具保留原字节规则。

`INVALID_INPUT` 的可选 `details` 给出字段路径、稳定 `reason`、单位和 `action`。首项直接置于 details，其余放入 `additional_violations`，总计最多八项；未知字段指向所属对象。错误仅返回类型和长度等信息。正文等响应仍按完整 JSON-RPC 预算处理，错误详情在预算紧张时省略。

`search_emails` 支持组合 `query`、`from`、`to`、`subject`、`since`、`before`、`seen`、`flagged`、`answered`、`has_attachment`、`min_size`、`max_size`。日期采用 `YYYY-MM-DD`，`since` 包含当日、`before` 不包含当日，按 IMAP INTERNALDATE 的日历日匹配。大小为整封邮件编码后的字节数，最小值和最大值均包含边界。

例如查询本周未读邮件：

```json
{"mailbox":"INBOX","since":"2026-10-26","before":"2026-11-02","seen":false,"limit":20}
```

未回复筛选使用 `answered:false`，含义是服务器未设置 `\\Answered` 标记；不能据此断定所有客户端都没有回复过。收件人 `to` 对应 To 头，不包含隐式 BCC 投递关系。

服务先使用 UID SEARCH 过滤日期、文本、地址、大小和标记。IMAP 没有通用的附件搜索条件，`has_attachment` 在有限候选中使用有界整封邮件读取与 MIME 解析检查附件。每页只覆盖有限 UID 窗口；必须持续传入 `next_before_uid` 和返回的 `uid_validity`，直到游标为空。空页不等于没有更早的匹配。

中文等非 ASCII 筛选词使用 IMAP4rev1 的 `CHARSET UTF-8` 与同步 literal，发送 UTF-8 字节前等待 continuation，命令结束必须收到对应 tag 的 OK。`BADCHARSET` 返回 `UNSUPPORTED_CHARSET`；不协商 `UTF8=ACCEPT` 或依赖 `LITERAL+`。组合文本字段分别在同一 500 UID 窗口搜索并取交集；服务器的字符集与搜索匹配规则仍影响结果。

## 附件与兼容性

全部 15 个读取工具声明 `outputSchema`，覆盖实际 `{data, request_id}`、`{error, request_id}`、逐项失败和完整响应预算降级。写工具沿用原契约。常用顺序为 `list_accounts → list_mailboxes → search_emails → get_emails`，再按需调用 `read_conversation` 或 `list_attachments → prepare_attachment_download`。附件当前仅做本地离线测试，核对完整下载和文件内容读取；ChatGPT 实际连接测试后移。

`list_accounts` 的 `default_account_id` 与账户的 `default` 指示默认项；`capabilities` 和 `limits` 给出当前实现及配置值。`list_mailboxes` 保留 `attributes`，增加 `attribute_flags`、`delimiter`、`selectable`、`special_use`、`role_source` 和 `role_candidates`。服务器用途、管理员配置和 INBOX 协议角色分别标记 `server`、`configuration`、`protocol`；只有名称候选时 `special_use` 为空、`role_source` 为 null。

`download_attachment` 保持原有 `content_type`、`size`、`base64` 返回字段和工具输出预算，新增 SHA-256 校验值。`list_attachments` 提供与 `get_email` 一致的原遍历索引（0–1023）及稳定的 `attachment_id`；`prepare_attachment_download` 和 `download_attachment` 接受 id 或索引，至少提供一个，同时提供时必须指向同一部件。这三个工具与其余账户读取工具共用可选 `account_id`，省略时选择配置默认读取账户并检查当前主体权限。稳定 id 绑定账户身份、文件夹、UIDVALIDITY、UID、遍历位置及 MIME 引用版本；下载授权由请求主体及短期句柄决定。

附件描述保留原字段，增加 `safe_filename`、`declared_content_type`、`detected_content_type`、`decoded_size`、`size_accuracy`、`disposition`、`content_id`、`downloadable`、`unavailable_reason`、`delivery` 和 `checksum`。类型检测及准备阶段摘要当前为 null；identity 编码大小为 exact，Base64 为 estimate，其他为 unknown。仅 exact 解码大小用于提前判定解码超限，估算值保留为提示。未知传输编码列为不可下载，编码部件本身达到连接累计预算时标记 `connection_bytes`；完整解码后才生成摘要。整封大小超过 raw reader 上限时不列出 `inline_base64`，符合部件预算的附件仍可使用认证 HTTP 下载。`delivery` 指出可用方式、认证、过期及下一动作，`client_delivery_required` 提醒客户端把下载字节交给文件解析工具。

`list_attachments` 和 `prepare_attachment_download` 先确认邮件大小，再读取 BODYSTRUCTURE，并逐节点核对完整 MIME 头，建立与原 MIME 遍历一致的附件清单。结构超限、服务器明确拒绝结构读取或映射需要完整 MIME 时，仅在已确认整封大小不超过 32 MiB 的情况下，关闭原连接并新建连接回退一次整封读取。SIZE、必要头和 raw 命令的协议故障直接返回错误。所有步骤共用原操作期限。GET 重新核对 UIDVALIDITY 和清单，分块读取所选附件；内联下载继续整封读取。

两项工具可返回 `read_diagnostics`：`source` 为 `bodystructure` 或 `raw`；`fallback_reason` 为 null 或 `structure_limit`、`structure_rejected`、`mapping_unverified`、`metadata_conflict`、`whole_multipart_attachment`、`header_budget`；`limit_kind` 为 null 或 `mime_structure`。原因是固定类别，保留成功回退的依据。必要 MIME 头每命令最多 16 个 section、最多 64 个命令，单头最多 128 KiB、总证据最多 1 MiB。

既有附件分类、RFC2231 文件名、原索引及整体 multipart 附件语义共用 PartManifest；附加 `.eml` 作为整体附件。整封 raw 读取及解码附件分别受 32 MiB 限制；按部件下载可读取更大邮件中的小附件，每块最多 64 KiB，连接累计最多 128 MiB。当前 R07 第一批已接入附件部件读取，局部正文在第二批交付，`part_fetch` 在两批完成后更新。元数据和筛选共用连接响应预算，超过预算明确失败。

返回的 `download_url` 需要同一用户的有效 OAuth Bearer 令牌。句柄有效期 600 秒，每实例最多保存 64 个描述符，不缓存附件字节；重启后失效。它不能作为公共分享链接。最多四个二进制下载并发，响应被消费或丢弃后释放传输名额；IMAP 读取还使用共享邮箱槽位。附件列表使用 `next_attachment_offset` 分页。

公开路由为 `MCP_RESOURCE/attachments/{token}`，需转发至内部 `/mcp/attachments/{token}` 并关闭包含句柄的访问日志。响应使用 `application/octet-stream`、`no-store`、`nosniff` 和安全文件名的 attachment disposition，`X-Content-SHA256` 与 `Content-Digest` 校验最终交付字节。客户端负责安全解析文件内容；[标准适配器](../scripts/attachment-client-adapter/README.md)使用独立授权会话完成下载、校验和落盘。[ChatGPT 适配设计](attachment-client-adapter-design.md)记录宿主文件接口及待验收步骤，分析请求由用户发起。

附件 GET 的失败响应为带 `request_id`、固定 `phase` 和 `action` 的小型 JSON：401 重新认证；认证服务暂不可用时返回 503；未知、过期或其他主体的句柄统一 404 并提示重新准备；UIDVALIDITY 变化返回 409；消息或部件删除返回 404；未知编码返回 422；资源超限返回 413 及限制类别；下载满额返回 429 和 `Retry-After`；后端失败/坏编码返回 502，超时返回 504。`connection_bytes` 指连接累计读取预算，`part_fetch_bytes` 指单次部件 FETCH 的编码 literal 预算，`decoded_attachment_bytes` 指最终解码内容预算。确认 IMAP tagged OK 后才判定消息缺失；缺必需属性、NO 或断连按后端失败处理。

私密暂存目录不可用或文件操作失败时，准备/下载返回 `ATTACHMENT_STORAGE_UNAVAILABLE`，GET 使用 HTTP 503、`phase:staging`、`action:contact_operator`；管理员按[部署说明](deployment.md#准备目录与配置)检查目录和存储。文件传输开始后若读取失败，响应流结束，客户端校验长度和摘要后重新发起下载。暂存文件最多四份、每份 32 MiB，响应消费或取消后删除；运行中的文件操作完成前继续占用相应名额。


## 批量整理

`mark_emails`、`move_emails`、`delete_emails` 接受同一文件夹下明确的 UID 列表，最多 50 个，不接受范围字符串或重复 UID。必须提供 UIDVALIDITY。批量删除无论移入废纸篓还是永久删除，都要求用户明确确认并传入 `confirm:true`。

操作逐项返回结果。中断后已完成结果保留，当前项可能为 `unknown`，其余为 `unprocessed`。不要自动重试整个批次；先检查邮箱的当前状态。移动要求 MOVE，永久删除要求 UIDPLUS，并仅对指定 UID 执行 UID EXPUNGE。

## 会话、定位和正文

`read_conversation` 从指定 `uid` 和 `uid_validity` 出发，按 Message-ID、References 和 In-Reply-To 的精确标识符关联同一文件夹的邮件，不使用相同主题作为关联依据。默认检查 100 个候选头，最多 200 个；默认返回 10 封，最多 20 封，累计正文最多 256 KiB。扫描范围、候选上限、遗漏邮件和后续 UID 游标均随结果返回。`next_before_uid` 仅推进候选扫描；`omitted_uids` 列出本次已关联但因数量或响应预算未返回的 UID，可用 `get_emails` 分批读取。跨窗口关联需要客户端继续查询并结合已有结果；不保证单次返回全邮箱内的所有往来。

`locate_message` 接受 `message_id` 和显式 `mailboxes` 列表，最多 20 个文件夹。先以服务端 HEADER 搜索缩小候选，再验证完整 Message-ID，避免子串误匹配。每个文件夹分别返回搜索范围及下一页游标；继续某个文件夹时应单独传入该文件夹和它自己的游标。

`get_email`、`get_emails` 和 `read_conversation` 支持 `body_mode:full|trimmed` 和 `max_body_length`，长度以 UTF-8 字节计，截断保持字符完整。`trimmed` 按常见引用分隔符及标准签名分隔符进行启发式处理；结果分别报告引用、签名省略及长度截断，可用 `full` 重新读取。HTML-only 邮件仍沿用现有纯文本读取行为，不新增 HTML 渲染器。

这三个工具另支持 `html_display:legacy|readable`，默认 `legacy`。`readable` 在纯文本中保留链接地址、列表编号与层次、表格行列边界、引用前缀和预格式空白；图片保留 alt。链接仅显示 http/https/mailto 地址，外部资源请求数为零。`html_display_metadata` 返回格式版本、HTML 部件数和固定原因的省略计数；旧转发固定使用 legacy 格式。

例如读取会议邮件并保留参会链接：

```json
{"mailbox":"INBOX","uid":7,"uid_validity":9,"body_mode":"full","html_display":"readable"}
```

可读格式最多 16 层缩进、256 个链接、每地址 2048 UTF-8 字节；多个所选 HTML 部件共用链接、32768 token 与 1 MiB 输出预算。相对地址、危险协议、控制字符、过长链接和超额层次通过 `omissions` 报告；无法支持的外来内容继续返回 `UNSUPPORTED_BODY`。MIME 候选选择及纯文本优先规则保持一致。

## 诊断、统计和缓存

`diagnose_connectivity` 分阶段报告当前认证调用、TCP、TLS、IMAP greeting、登录和能力检查。只返回固定阶段、状态及允许展示的能力，不返回服务器 banner、邮箱身份、主机名或原始错误。缺失或无效令牌由 HTTP 401 表达，JWKS 暂不可用由 HTTP 503 与 Retry-After 表达，工具只有在认证通过后才执行；SMTP 不在诊断范围内。

`get_mailbox_statistics` 以 STATUS 返回文件夹邮件数量、未读数量等。统计必须收到匹配文件夹的全部请求字段和匹配 tag 的 OK；断连、缺失字段及拒绝均返回 MAIL_BACKEND，不把缺失响应解释为零。可选 `date_start` 和 `date_end` 统计最多 31 天的 INTERNALDATE 日分布，结束日期不包含在内。分布只覆盖本次 500 UID 窗口，结果附带范围和游标；总量与分布不是原子快照，不能混为同一时刻的全量统计。

元数据缓存仅保存完整邮件头，全实例所有账户合计最多 256 项、16 MiB，TTL 为 60 秒。键包含内部账户身份与世代、文件夹、UIDVALIDITY 和 UID；每次会话先获取当前 UIDVALIDITY，只失效相应账户和文件夹的旧缓存。正文、附件、可变标记和统计结果不进入缓存。

## 协议依据

- [RFC 3501：IMAP4rev1 SEARCH、FETCH、STATUS 和 UIDVALIDITY](https://www.rfc-editor.org/rfc/rfc3501#section-6.3.10)
- [async-imap 官方 API 说明](https://github.com/chatmail/async-imap/blob/main/_autodocs/api-reference/session.md)
- [RFC 3501：字符串与同步 literal](https://www.rfc-editor.org/rfc/rfc3501#section-4.3)

协议文档用于核对行为；实际编译以 Cargo.lock 锁定的 async-imap 0.12 API 为准。
