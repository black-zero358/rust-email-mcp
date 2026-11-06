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

服务先使用 UID SEARCH 过滤日期、文本、地址、大小和标记。`has_attachment` 在有限候选中核对 BODYSTRUCTURE 和必要 MIME 头，按已验证清单判断附件；需要完整 MIME 时，每页最多一次有界整封回退。回退后可提前返回短页，`next_before_uid` 保留最后一个已检查 UID；新连接复查 UIDVALIDITY 和大小，两条连接共用原传输预算和期限。持续传入游标及返回的 `uid_validity`，直到游标为空，空页同样可以带后续游标。

`list_emails` 和 `search_emails` 增加可选 `preview`，默认 false。启用后先取得整页摘要，再按同一 MIME 候选规则读取正文预览；每封最多 200 个 Unicode 码点、8 KiB 编码取样，每页共用 128 KiB，探针与失败请求也计入申请额度。空白跨部件合并，HTML 使用可读文本策略。截断前缀仅解码完整编码组和字符，未结束的 HTML token 留待完整读取；预览不触发整封回退。附件筛选已成功获取的整封字节可按同一取样预算生成本地预览，用后释放，避免重复结构请求。

预览字段仅在请求时返回：`preview_status` 为 `ok`、`empty`、`budget_skipped`、`error`、`unprocessed` 或 `wire_budget_skipped`。成功或完整空正文附 `preview` 和 `preview_truncated`；错误附脱敏 `preview_error`。UTF-7 和其他不支持的取样字符集返回逐项 `UNSUPPORTED_BODY`，可通过完整正文读取工具读取 UTF-7 邮件。单项本地错误后继续；协议失败或预览子期限结束后，保留摘要并将后续预览标为 unprocessed。预览读取期限设为 5 秒与剩余操作时间一半中的较小值；网络等待按期限取消，同步 MIME 取样受字节和结构预算约束，完成后再次核对期限。完整 JSON-RPC 超限时从页尾移除可选预览，标 wire_budget_skipped；若逐项状态仍放不下，则省略整页的预览字段，保留 UID、摘要、分类和游标。客户端在请求预览后收到无预览字段的摘要时，应按响应预算省略处理；仅基础元数据超限返回 `LIMIT_EXCEEDED`。

列表摘要始终提供 `classification`、`classification_signals`、`classification_rule_version:headers-v1`、`classification_status` 和 `unsubscribe_available`。合法 List-Id、List-Unsubscribe、List-Unsubscribe-Post 或 `Precedence:list` 产生 newsletter；非 no 的合法 Auto-Submitted，或 `Precedence:bulk|junk|auto_reply` 产生 automated。两类可并存，无信号时为 unclassified。signals 仅含固定字段名与类别，按固定顺序去重。摘要头最多 1 MiB。在线和离线摘要均返回 `header_complete`。值为 false 时，分类和主题、发件人、日期只使用已完整观察到的字段，null 表示未知。值为 true 时，null 表示完整头部中缺少该字段。

`unsubscribe_available` 表示 List-Unsubscribe 中存在受支持的 http/https/mailto URI；最多 2048 字节，拒绝认证信息及控制字符。服务只检查字段，不访问地址、不退订、不修改邮件。完整正文继续使用 `get_email` 或 `get_emails`。

中文等非 ASCII 筛选词使用 IMAP4rev1 的 `CHARSET UTF-8` 与同步 literal，发送 UTF-8 字节前等待 continuation，命令结束必须收到对应 tag 的 OK。`BADCHARSET` 返回 `UNSUPPORTED_CHARSET`；不协商 `UTF8=ACCEPT` 或依赖 `LITERAL+`。组合文本字段分别在同一 500 UID 窗口搜索并取交集；服务器的字符集与搜索匹配规则仍影响结果。

## 附件与兼容性

全部 16 个读取工具声明 `outputSchema`，覆盖实际 `{data, request_id}`、`{error, request_id}`、逐项失败和完整响应预算降级。写工具沿用原契约。常用顺序为 `list_accounts → list_mailboxes → search_emails → get_emails`，再按需调用 `read_conversation` 或 `list_attachments → prepare_attachment_download`。附件当前仅做本地离线测试，核对完整下载和文件内容读取；ChatGPT 实际连接测试后移。

`list_accounts` 的 `default_account_id` 与账户的 `default` 指示默认项；`capabilities` 和 `limits` 给出当前实现及配置值。`list_mailboxes` 保留 `attributes`，增加 `attribute_flags`、`delimiter`、`selectable`、`special_use`、`role_source` 和 `role_candidates`。服务器用途、管理员配置和 INBOX 协议角色分别标记 `server`、`configuration`、`protocol`；只有名称候选时 `special_use` 为空、`role_source` 为 null。

`download_attachment` 保持原有 `content_type`、`size`、`base64` 返回字段和工具输出预算，新增 SHA-256 校验值。`list_attachments` 提供与 `get_email` 一致的原遍历索引（0–1023）及稳定的 `attachment_id`；`prepare_attachment_download` 和 `download_attachment` 接受 id 或索引，至少提供一个，同时提供时必须指向同一部件。这三个工具与其余账户读取工具共用可选 `account_id`，省略时选择配置默认读取账户并检查当前主体权限。稳定 id 绑定账户身份、文件夹、UIDVALIDITY、UID、遍历位置及 MIME 引用版本；下载授权由请求主体及短期句柄决定。

附件描述保留原字段，增加 `safe_filename`、`declared_content_type`、`detected_content_type`、`decoded_size`、`size_accuracy`、`disposition`、`content_id`、`downloadable`、`unavailable_reason`、`delivery` 和 `checksum`。类型检测及准备阶段摘要当前为 null；identity 编码大小为 exact，Base64 为 estimate，其他为 unknown。仅 exact 解码大小用于提前判定解码超限，估算值保留为提示。未知传输编码列为不可下载，编码部件本身达到连接累计预算时标记 `connection_bytes`；完整解码后才生成摘要。整封大小超过 raw reader 上限时不列出 `inline_base64`，符合部件预算的附件仍可使用认证 HTTP 下载。`delivery` 指出可用方式、认证、过期及下一动作，`client_delivery_required` 提醒客户端把下载字节交给文件解析工具。

`list_attachments` 和 `prepare_attachment_download` 先确认邮件大小，再读取 BODYSTRUCTURE，并逐节点核对完整 MIME 头，建立与原 MIME 遍历一致的附件清单。结构超限、服务器明确拒绝结构读取或映射需要完整 MIME 时，仅在已确认整封大小不超过 32 MiB 的情况下，关闭原连接并新建连接回退一次整封读取。SIZE、必要头和 raw 命令的协议故障直接返回错误。所有步骤共用原操作期限。GET 重新核对 UIDVALIDITY 和清单，分块读取所选附件；内联下载继续整封读取。

两项工具可返回 `read_diagnostics`：`source` 为 `bodystructure` 或 `raw`；`fallback_reason` 为 null 或 `structure_limit`、`structure_rejected`、`mapping_unverified`、`metadata_conflict`、`whole_multipart_attachment`、`header_budget`；`limit_kind` 为 null 或 `mime_structure`。原因是固定类别，保留成功回退的依据。必要 MIME 头每命令最多 16 个 section、最多 64 个命令，单头最多 128 KiB、总证据最多 1 MiB。

既有附件分类、RFC2231 文件名、原索引及整体 multipart 附件语义共用 PartManifest；附加 `.eml` 作为整体附件。整封 raw 读取及解码附件分别受 32 MiB 限制；按部件下载可读取更大邮件中的小附件，每块最多 64 KiB，连接累计最多 128 MiB。`part_fetch:true` 表示附件下载和正文读取均支持按部件获取。元数据和筛选共用连接响应预算，超过预算明确失败。

返回的 `download_url` 需要同一用户的有效 OAuth Bearer 令牌。句柄有效期 600 秒，每实例最多保存 64 个描述符，不缓存附件字节；重启后失效。它不能作为公共分享链接。最多四个二进制下载并发，响应被消费或丢弃后释放传输名额；IMAP 读取还使用共享邮箱槽位。附件列表使用 `next_attachment_offset` 分页。

公开路由为 `MCP_RESOURCE/attachments/{token}`，需转发至内部 `/mcp/attachments/{token}` 并关闭包含句柄的访问日志。响应使用 `application/octet-stream`、`no-store`、`nosniff` 和安全文件名的 attachment disposition，`X-Content-SHA256` 与 `Content-Digest` 校验最终交付字节。客户端负责安全解析文件内容；[标准适配器](../scripts/attachment-client-adapter/README.md)使用独立授权会话完成下载、校验和落盘。[ChatGPT 适配设计](attachment-client-adapter-design.md)记录宿主文件接口及待验收步骤，分析请求由用户发起。

附件 GET 的失败响应为带 `request_id`、固定 `phase` 和 `action` 的小型 JSON：401 重新认证；认证服务暂不可用时返回 503；未知、过期或其他主体的句柄统一 404 并提示重新准备；UIDVALIDITY 变化返回 409；消息或部件删除返回 404；未知编码返回 422；资源超限返回 413 及限制类别；下载满额返回 429 和 `Retry-After`；后端失败/坏编码返回 502，超时返回 504。`connection_bytes` 指连接累计读取预算，`part_fetch_bytes` 指单次部件 FETCH 的编码 literal 预算，`decoded_attachment_bytes` 指最终解码内容预算。确认 IMAP tagged OK 后才判定消息缺失；缺必需属性、NO 或断连按后端失败处理。

私密暂存目录不可用或文件操作失败时，准备/下载返回 `ATTACHMENT_STORAGE_UNAVAILABLE`，GET 使用 HTTP 503、`phase:staging`、`action:contact_operator`；管理员按[部署说明](deployment.md#准备目录与配置)检查目录和存储。文件传输开始后若读取失败，响应流结束，客户端校验长度和摘要后重新发起下载。暂存文件最多四份、每份 32 MiB，响应消费或取消后删除；运行中的文件操作完成前继续占用相应名额。


## 批量整理

`mark_emails`、`move_emails`、`delete_emails` 接受同一文件夹下明确的 UID 列表，最多 50 个，不接受范围字符串或重复 UID。必须提供 UIDVALIDITY。批量删除无论移入废纸篓还是永久删除，都要求用户明确确认并传入 `confirm:true`。

单封和批量整理在写入前核对源文件夹 UIDVALIDITY 和 UID 存在性。预检缺失时单封返回 `NOT_FOUND`，批量项返回 `not_found`，重复删除沿用这一结果。`updated` 表示服务器确认请求的标记状态或通过 COPYUID 确认目标邮件移动。`accepted` 表示服务器返回 OK，逐 UID 结果尚待核对，例如没有 COPYUID 的移动或永久删除。客户端应处理新增的 `accepted` 状态，并在 `retry_allowed` 为 `false` 时先核对邮箱状态。

操作逐项返回结果。中断后已完成结果保留，当前项可能为 `unknown`，其余为 `unprocessed`。不要自动重试整个批次；先检查邮箱的当前状态。移动要求 MOVE，永久删除要求 UIDPLUS，并仅对指定 UID 执行 UID EXPUNGE。

## 会话、定位和正文

`get_email`、`get_emails` 和 `read_conversation` 先核对邮件结构及 MIME 头，再分块读取完整的所选正文；大附件邮件中的小正文可独立读取。正文与原整封解析共用 alternative/related 选择规则，所有所选部件共用 32 MiB 解码预留和 HTML 展示预算，字符集状态跨块保留。`max_body_length:0` 和显示截断仍先完成正文校验。

需要完整 MIME、UTF-7 解码，或已成功完成的正文末尾探测返回额外 1 字节时，在已确认的整封大小上限内，每 UID 最多回退一次新连接 raw 读取，重新核对 UIDVALIDITY 并沿用原期限。正文结果增加 `read_diagnostics`，字段与附件诊断一致，额外回退原因 `charset_utf7` 表示 UTF-7，`body_eof_mismatch` 表示正文部件的长度与末尾探测结果不一致。批量中已完成命令后的局部正文错误只影响当前项；协议、目标数据或连接失败停止后续读取，后续项标记 `unprocessed`。重复 UID 按输入顺序保留逐项结果。

`read_conversation` 未传 `mailboxes`、`scope` 或 `cursor` 时沿用单文件夹调用：从指定 `uid` 和 `uid_validity` 出发，按 Message-ID、References 和 In-Reply-To 的精确标识符关联同一文件夹的邮件，不使用相同主题作为关联依据。默认检查 100 个候选头，最多 200 个；默认返回 10 封，最多 20 封，累计正文最多 256 KiB。扫描范围、候选上限、遗漏邮件和后续 UID 游标均随结果返回。`next_before_uid` 仅推进候选扫描；`omitted_uids` 列出本次已关联但因数量、响应预算、正文错误或连接终止未返回的 UID，具体失败保存在 `message_errors`。可根据逐项错误处理后用 `get_emails` 分批读取。跨窗口关联由客户端继续查询并结合已有结果。

`locate_message` 接受 `message_id` 和显式 `mailboxes` 列表，最多 20 个文件夹。先以服务端 HEADER 搜索缩小候选，再验证完整 Message-ID，避免子串误匹配。每个文件夹分别返回搜索范围及下一页游标；继续某个文件夹时应单独传入该文件夹和它自己的游标。

`get_email`、`get_emails` 和 `read_conversation` 支持 `body_mode:full|trimmed` 和 `max_body_length`，长度以 UTF-8 字节计，截断保持字符完整。`trimmed` 按常见引用分隔符及标准签名分隔符进行启发式处理；结果分别报告引用、签名省略及长度截断，可用 `full` 重新读取。HTML-only 邮件仍沿用现有纯文本读取行为，不新增 HTML 渲染器。

这三个工具另支持 `html_display:legacy|readable`，默认 `legacy`。`readable` 在纯文本中保留链接地址、列表编号与层次、表格行列边界、引用前缀和预格式空白；图片保留 alt。链接仅显示 http/https/mailto 地址，外部资源请求数为零。`html_display_metadata` 返回格式版本、HTML 部件数和固定原因的省略计数；旧转发固定使用 legacy 格式。

例如读取会议邮件并保留参会链接：

```json
{"mailbox":"INBOX","uid":7,"uid_validity":9,"body_mode":"full","html_display":"readable"}
```

可读格式最多 16 层缩进、256 个链接、每地址 2048 UTF-8 字节；多个所选 HTML 部件共用链接、32768 token 与 1 MiB 输出预算。相对地址、危险协议、控制字符、过长链接和超额层次通过 `omissions` 报告；无法支持的外来内容继续返回 `UNSUPPORTED_BODY`。MIME 候选选择及纯文本优先规则保持一致。

## 跨文件夹会话

`read_conversation` 首次传入 `mailboxes` 或 `scope:"standard"` 进入跨文件夹模式，两者互斥。指定种子 `mailbox`、`uid`、`uid_validity`，省略 mailbox 使用 INBOX；种子目录须包含在查询范围内，仅选择 Sent 等目录时应同时指定种子 mailbox。显式列表最多 20 个文件夹；standard 使用协议 INBOX、服务器明确标记的 Sent 和管理员 `ARCHIVE_MAILBOX` / `folders.archive`，排除 All、Junk、Trash、Drafts 和不可选择目录。普通名称候选不加入 standard，显式列表可选择需要检查的其他目录。

```json
{"account_id":"work","mailbox":"INBOX","uid":7,"uid_validity":9,"mailboxes":["INBOX","Sent","Archive"],"limit":20,"candidate_limit":200,"html_display":"readable"}
```

后续只传入返回的 cursor 和原账户；种子、范围、正文选项及分页设置保存在游标中。非默认账户每次均须传入 `account_id`，省略账户会按当前默认账户检查。`before_uid` 仅用于旧单文件夹模式。

```json
{"account_id":"work","cursor":"返回的不透明游标"}
```

首次读取固定实际目录、各自 UIDVALIDITY 和 UIDNEXT−1 水位。新到邮件留给下一次新查询；续页复核已冻结目录的 UIDVALIDITY，变化返回 `STALE_UID` 并使游标失效。起始不可访问的目录保持独立错误，健康目录继续；收到重启动作后新建查询，以重新确定范围。Message-ID、References、In-Reply-To 通过有界 HEADER 搜索和精确头解析双向展开，相同主题不参与关联。

每页全范围共享以下预算，`list_accounts` 的 `data.accounts[].limits` 同时公开当前值：

| 项目 | 上限 |
| --- | --- |
| 候选头读取 | 默认及最多 200 次，种子头单独读取 |
| 返回结果位置 | 默认及最多 20 个；副本来源更新也计入 |
| 展示正文 | 每个变体最多 64 KiB，每页共 256 KiB |
| 搜索 | 256 个窗口，每窗口最多 200 个 UID 数值；单命令响应及解析分配各 128 KiB |
| 身份指纹原始邮件读取 | 每页累计 256 KiB，同时遵守单封配置上限 |
| 普通与故障恢复连接 | 8 个；另有每个结果位置至多一次正文 raw 回退 |
| 总操作期限 | 进入邮箱执行槽后采用配置超时与 60 秒中的较小值，排队另限 10 秒；连接共用原传输预算 |

响应 `data.mode` 为 `cross_mailbox`。`messages` 按实体和变体组织；`entity_id` / `variant_id` 供跨页合并，`new_entity` / `new_variant` 标记首次出现，`locations` 保留所有新增来源，每个引用包含 `account_id`、`mailbox`、`uid_validity`、`uid`。正文位于变体的 `message`，`representative` 标明本页已核对的来源。后续页返回新实体、新变体或新增位置，客户端累积这些增量。

只有关键头原始字节和完整根 MIME 正文字节的 SHA-256 指纹均一致时，多个位置才折叠为同一变体。关键头包含发送/收件、日期、主题、关联头和 MIME 头，按固定字段顺序比较原始值，同名字段保留出现次序；不同折行或编码保留为独立证据。相同 Message-ID 的关键头或内容冲突标 `collision`，完整核对一致标 `verified`，尚缺内容指纹标 `identity_unverified`；合法 ID 缺失或重复时按邮件引用独立保留。正文仍可按部件读取，超出指纹额度的邮件保留未核验身份。

`folders` 报各目录水位、完成/剩余窗口、待处理候选及错误；`coverage` 区分关联图、待交付位置、请求/已返回正文。`complete:true` 要求冻结范围内关联扫描结束、无未解决错误且位置增量已交付。正文因预算或内容错误省略时列入 `omitted_refs`，对应变体可含 `body_error`，客户端可用完整引用单独读取。`errors` 最多列 20 项，`errors_omitted` 报剩余数量。

完整 JSON-RPC 预算计入真实 id、结构化内容及转义文本副本。预算不足先省略正文，再减少本页位置增量；未交付位置留在游标，`response_budget_omitted` 标记首次交付的响应裁减。基础元数据仍超限时返回 `LIMIT_EXCEEDED`，原游标保留待交付结果。客户端重试最近一次已使用的 token 可重放该页的位置、覆盖、错误快照和原裁减标记。重放仍复核冻结目录；原已裁减页保持精简结果，其他页重新读取正文，新增正文省略通过 `omitted_refs` 与正文覆盖计数报告。目录暂不可访问时返回可重试错误并保留 token。并发使用同一个 token 返回 `CURSOR_UNAVAILABLE`，应等待正在执行的调用完成。

游标绑定请求主体和账户世代，全实例共享最多 64 项、8 MiB，租出执行中的状态也计入。成功推进刷新 10 分钟空闲 TTL，过期后用种子重新查询。`cursor_storage:memory` 在重启后失效；`persistent` 保存原到期时间和最近重放页，重启后仍在线复核全部冻结目录。配额不足返回部分结果及 `cursor_action`：`resume_or_restart_with_narrower_scope` 可继续或缩小范围，`restart_later_or_with_narrower_scope` 要求稍后或缩小范围重启。初始目录不可用且其余工作已结束时也会提示重启。`cursor:null` 且 `complete:false` 时按动作重启；已有错误和正文遗漏也应分别处理。

## 诊断、统计和缓存

上述部分结果及 cursor_action 对应内存状态容量不足。持久 SQLite 提交因磁盘额度拒绝时返回 `LIMIT_EXCEEDED`，details.limit_kind 为 cache_quota，原已发布 token 和未交付位置保留，扩容恢复后可继续。

`diagnose_connectivity` 分阶段报告当前认证调用、TCP、TLS、IMAP greeting、登录和能力检查。只返回固定阶段、状态及允许展示的能力，不返回服务器 banner、邮箱身份、主机名或原始错误。缺失或无效令牌由 HTTP 401 表达，JWKS 暂不可用由 HTTP 503 与 Retry-After 表达，工具只有在认证通过后才执行；SMTP 不在诊断范围内。

`get_mailbox_statistics` 以 STATUS 返回文件夹邮件数量、未读数量等。统计必须收到匹配文件夹的全部请求字段和匹配 tag 的 OK；断连、缺失字段及拒绝均返回 MAIL_BACKEND，不把缺失响应解释为零。可选 `date_start` 和 `date_end` 统计最多 31 天的 INTERNALDATE 日分布，结束日期不包含在内。分布只覆盖本次 500 UID 窗口，结果附带范围和游标；总量与分布不是原子快照，不能混为同一时刻的全量统计。

默认内存头缓存保存完整邮件头，全实例所有账户合计最多 256 项、16 MiB，TTL 为 60 秒。键包含内部账户身份与世代、文件夹、UIDVALIDITY 和 UID；每次会话先获取当前 UIDVALIDITY，只失效相应账户和文件夹的旧缓存。正文、附件、可变标记和统计结果不进入缓存。

持久缓存由管理员通过 `CACHE_CONFIG_PATH` 选择账户和目录，模式为 off、metadata 或 full。在线读取完成后，有界后台队列记录邮件头、已核对部件清单与索引；部分头保留完整性标记。full 另保存完整正文、原始邮件及附件。`get_cache_status({"account_id":"work"})` 先授权，再查询本地状态，返回 off、ready 或 unavailable，以及该账户的磁盘额度、当前目录世代、检查时间、dirty、邮件数、完整头数、索引头数及已核对清单数。full 还提供 content_pending 和各目录 raw/full_body/attachments 层的物化、可派生、缺失、省略及不支持计数。

full 显式启用 `sync.enabled:true` 后后台回填选定范围的全部历史，并周期发现新增及核对标记/删除；旧配置省略 sync 保持暂停。`get_cache_status` 的 sync 返回策略、运行状态和 folders；每目录的 progress 包含独立 history/tail/reconciliation、冻结水位与历史/当前覆盖，runtime 给出固定最后失败类别及配额暂停标记。查询状态本身失败时 error 为 quota/storage，账户仍为 ready、目录失效证据待持久化时为 verification_pending，这些情况下 progress 为 null；跨 UIDVALIDITY 的旧账户快照先返回 unavailable，sync.folders 为 null。各层 complete 结合该范围的发现、存在性核对及对应内容证据；可从 raw 派生的内容同时保留物化缺口。参数、时间字段及管理步骤见[缓存状态](cache-design.md#查询状态)和[后台同步管理](deployment.md#后台同步管理)。

可选 `sync.protocol:"auto"` 优先启用 QRESYNC 标记/删除同步，其次使用 CONDSTORE 标记加速，默认 `rev1`；周期存在性核对继续运行。`sync.protocol` 返回配置，目录 `runtime.protocol`/`protocol_fallback` 显示实际协议和固定降级类别；`condstore` 与 `qresync` 分别返回独立持久化水位，MODSEQ 使用十进制字符串。每页事实与位置共同提交，完整一轮结束后更新 committed；大范围已确认删除先暂停该范围离线读取，新的完整核对完成后恢复。能力缺失、协商失败、临时拒绝和明确 NOMODSEQ 分别处理，详细字段见[缓存状态](cache-design.md#查询状态)。

`write_health` 记录当前进程的失败证据任务数 `failed_writes`、最后固定原因 `last_error`（quota、queue_full、storage 或 null）及粘滞额度标记 `quota_blocked`。一批证据写入计一个任务；重启后重新评估。额度不足时仍可读取已保存的覆盖和额度信息，普通在线邮件读取继续返回其实际结果。

跨文件夹会话的冻结范围全部被缓存配置选中时，`cursor_storage` 为 persistent，保存检查点和最近输入的重放快照；关闭缓存或包含未选目录时为 memory。持久范围遇存储故障返回固定游标错误，修复后可重试原已提交 token；已确认的世代变化使旧 token 失效。后台元数据写入失败保留已完成的普通在线读取结果。配置与恢复见[缓存设计](cache-design.md)。

full 支持以下显式来源参数，默认值沿用在线行为：

| 工具 | 离线参数 |
| --- | --- |
| list_mailboxes、list_emails、get_email、get_emails、read_conversation | `read_source:"cache"` |
| list_attachments、download_attachment、prepare_attachment_download | `read_source:"cache"` |
| search_emails、locate_message | `search_source:"cache"` |

cache 请求使用当前账户授权和本地证据，零 IMAP；在线退避期间仍可读取。结果的 cache 对象标明 stale、remote_generation_unverified、最后观察时间和覆盖范围。列表的 FLAGS、INTERNALDATE、SIZE 保存各自观察时间，缺失字段按未知值报告；筛选跳过缺少所需证据的邮件并计入覆盖缺口。`get_email_status`、`get_mailbox_statistics` 和 `diagnose_connectivity` 继续执行在线命令，`get_cache_status` 查询本地状态。

正文支持原有 body_mode、html_display 和完整响应预算。附件保留原 MIME index、字节长度和 SHA-256；离线 download_url 继续要求同主体 Bearer，GET 复核当前账户权限、过期、目录状态和文件完整性。缺少完整副本返回 CACHE_MISS；未配置 full 或请求超出范围返回 CACHE_UNAVAILABLE；恢复、异常退出或写操作影响的范围返回 CACHE_NEEDS_VERIFICATION。网络错误后可另行选择 cache。

全文搜索使用完整头和完整正文索引，支持中文长词与短词，并返回匹配层及索引覆盖。`search_index.version` 为 2，`indexed_body` 只计入新版完整正文索引，`rebuild_pending` 表示还有旧索引或尚未完成的替换。每次初始搜索可从本地正文重建至多四封，后台同步暂停时仍可推进。存在 next_cache_cursor 时，使用 cache_cursor 和相同查询、范围、筛选条件继续；索引版本或数据修订变化后，游标返回 `CURSOR_UNAVAILABLE`，需重新搜索。离线会话游标绑定来源和本地范围修订，正常清洁重启保留原到期时间。各工具的完整矩阵见[内容缓存设计](cache-content-design.md#显式离线契约)。

## 协议依据

- [RFC 3501：IMAP4rev1 SEARCH、FETCH、STATUS 和 UIDVALIDITY](https://www.rfc-editor.org/rfc/rfc3501#section-6.3.10)
- [async-imap 官方 API 说明](https://github.com/chatmail/async-imap/blob/main/_autodocs/api-reference/session.md)
- [RFC 3501：字符串与同步 literal](https://www.rfc-editor.org/rfc/rfc3501#section-4.3)

协议文档用于核对行为；实际编译以 Cargo.lock 锁定的 async-imap 0.12 API 为准。
