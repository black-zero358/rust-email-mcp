# 架构与设计约束

[CONTRIBUTING.md](../CONTRIBUTING.md) 是工程总入口，定义设计原则 D1–D6、变更流程和完成标准。本文件记录这些原则在当前实现中的细则、模块边界和设计决策。用户能力与配置见 [README](../README.md)，维护要求见 [maintenance](maintenance.md)，部署操作见 [deployment](deployment.md)。验收结果和基准测量属于带版本依据的 [validation](validation.md)，不能替代当前规范。

## Module boundaries

当前实现是一个 Rust 服务进程，独占一份本地操作数据库与账户注册库。原默认邮箱保留既有写工具；可显式配置追加只读邮箱，每实例最多 16 个账户。账户共享全局资源额度，按每次请求的主体分别授权。生产路径使用 Streamable HTTP、IMAP4rev1 和 SMTP，认证服务独立部署。Python 和 Node 不参与服务运行时。

| 模块 | 当前职责 | 依赖与边界 |
| --- | --- | --- |
| `main` | 读取配置、组装服务、中间件、发现路由、日志和停机流程 | 负责接线；不得绕过认证、请求预算或输出保护 |
| `config` | 环境变量、严格账户 JSON、秘密引用及配置有效性 | 先完整校验配置，凭据不进入日志或注册库 |
| `accounts` | 持久 UUID、世代、账户 ACL、配置快照和注册库迁移 | 原 default 身份与操作库绑定；在 Journal 恢复前核验，不迁移发送记录 |
| `auth` | OIDC 发现、JWT/JWKS 校验、请求主体和 HTTP 认证错误 | 依赖配置与身份服务，不执行邮件操作 |
| `server` | MCP 工具目录、权限模式、输入校验、响应封装与预算 | 将已认证主体传给 `mail`；协议适配不直接操作邮箱或数据库 |
| `server/input`、`input_limits` | 读取字段错误、字符与字节契约、旧写用途限制 | 输入常量供 Schema 与 IMAP 编码共用；协议层不依赖 MCP 请求对象 |
| `mail` | IMAP 会话、邮件操作编排、发送意图与结果记录 | 调用 `imap_text`、`mime`、`smtp`、`journal`，持有执行槽位 |
| `mail/conversation` | 跨文件夹会话的读取编排、身份、关联图、分页状态和游标 | 编排复用 `mail/parts`；graph/identity/state/store 不依赖 MCP、HTTP 或网络，完整输出预算由 server 提供判定闭包 |
| `mail/runtime`、`mail/scheduler` | 按主体和账户路由、全局准入及公平调度 | 授权先于队列、缓存和连接；固定原写账户，保持发送生命周期 |
| `mail/attachments`、`mail/insights` | 附件交付及有界读取扩展 | 由 `mail` 编排，共用 MIME、协议、槽位与预算 |
| `mail/listing`、`mail/classification` | 摘要分页、附件筛选、可选预览及固定邮件头分类 | 保留最后已检查 UID 游标；分类只使用完整观察到的头字段，预览失败保留摘要 |
| `mail/parts` | BODYSTRUCTURE、必要 MIME 头、统一部件清单及正文分块读取 | 核对原 DFS 索引后发布引用；正文与 raw MIME 共用候选规则，读回退有独立大小证据、次数及总期限边界 |
| `imap_text` | rev1 邮箱名、非 ASCII 搜索与严格统计 STATUS | 处理编码、literal 和命令终结；不承担 MCP 授权 |
| `mime`、`html` | 有界 MIME 解析、正文选择、附件分类、HTML 文本提取 | `mime` 调用 `html`；不执行网络请求、不发送邮件 |
| `smtp` | 经验证 TLS/AUTH 的提交事务及独立清理 | 返回事务结果；由 `mail` 调用 `journal` 持久化，不在适配器中自动重试 |
| `journal` | SQLite 操作键、参数摘要、状态持久化及重启恢复 | 不调用邮件后端；数据库事务不跨 SMTP/IMAP 网络请求 |
| `cache` | 私有账户分区、物理额度、版本化元数据/索引和持久游标字节流 | 使用内部 AccountKey 和类型化证据，不依赖 MCP/邮箱凭据；短事务和流式 BLOB 接口 |
| `mail/persistence` | 管理员范围、在线观察、即时世代隔离、有界写队列及状态屏障 | 复用当前账户授权；缓存故障保留普通在线读取结果，持久提交使用明确错误 |
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

一次工具响应的 **完整序列化 JSON-RPC** 预算为 2 MiB，计入实际 `id`、`request_id`、`structuredContent` 及文本客户端使用的转义副本，不计 HTTP 头和 SSE 帧。JSON-RPC id 的序列化 JSON 最多 1024 字节，HTTP 中间件必须在 rmcp/工具执行前以 413 拒绝超限 id。超限错误本身也必须有界；已知发送结果不能因输出超限而丢失。客户端使用原 `operation_key` 调用 `get_operation` 后再决定后续操作。其他发现/握手响应按固定结构生成，不应混称为已经覆盖工具响应预算。

读取输入由 `server/input` 按 Unicode 码点执行 Schema 的 `minLength/maxLength`；`input_limits` 共享读取与旧写入的用途限制。读取文件夹及查询字段最多 512 码点、2048 UTF-8 字节，文件夹另有 8192 modified UTF-7 线路字节上限。Schema 的 `x-maxUtf8Bytes` / `x-maxModifiedUtf7Bytes` 公开额外预算。组合字符逐码点计数。旧写工具继续使用原字节校验；写操作中的读取原邮件及配置目录也显式选择 `LegacyWrite`。`get_operation` 保持原操作键的 128 UTF-8 字节预算。

输入错误可附 `details`：首项包含字段 JSON Pointer、稳定原因、预期类型/范围、长度单位和修正动作，`additional_violations` 最多七项，合计最多八项。未知字段指向所属对象，错误不回显字段值或任意未知键。读取资源错误通过 `limit_kind` 区分预算类型；完整响应容纳不下详情时先移除详情，保留基础错误与 request_id，再执行原有有界降级。

`server/output` 定义 16 个读取工具的 `outputSchema`，按真实成功、错误、逐项失败和完整响应降级分支描述封装；对象字段、可空值和状态分支均显式声明。`server/output_tests` 通过真实 HTTP MCP handler、隔离 TLS IMAP 与 jsonschema 验证器核验结果，覆盖旧调用、空页、错误和降级；写工具沿用原 Schema。

default 的 `AccountKey` 包含持久 UUID 与配置世代（v1 为 1），存放于独立 `accounts.sqlite`。注册库绑定 IMAP 登录身份/端点及规范化操作库路径，摘要域区分文件操作库与内存操作库；修改绑定拒绝启动。初始化在文件锁下清理固定暂存文件、构建完整 SQLite、同步并原子无覆盖发布，再加载正式路径核验。未知版本、损坏或权限不符返回脱敏错误；初始化成功后才打开 Journal。附件句柄携带 AccountKey，GET 再核对，重启继续使原内存句柄失效。

显式账户配置使用注册库 v2，保留原 default 的 UUID、世代与绑定。追加账户按稳定 `registration_key` 登记，别名/显示名变化保留身份；登录身份或端点变化、删除后重新加入均递增世代。删除保留 tombstone。注册库最多 1 MiB、256 个历史键，升级保留 v1-backup 和最近完整 previous 副本，持久锁覆盖多账户运行期。配置、秘密、迁移全部成功后才打开 Journal。配置格式、迁移事务与故障恢复见[多账户设计](multi-account-design.md)。

14 个账户读取工具接受可选 `account_id`，省略时选择配置默认读取账户再授权。`list_accounts` 仅显示当前主体可读账户，隐藏默认项时 `default_account_id=null`。全局 subject 准入与账户 ACL 取交集；未知、未授权、已删除或旧世代统一 `ACCOUNT_UNAVAILABLE`，附件 GET 统一 404。工具发现按当前主体过滤，直接调用读取工具仍执行账户授权。旧写工具始终选择原 default、拒绝 `account_id`，受原模式及写 ACL 共同控制。`get_operation` 保留原参数与 principal + operation_key 语义，全局已准入主体即使失去邮箱权限，仍可检查自己的历史发送结果。

共享 MIME 解析器的可选预算分类也适用于原有写操作，错误码和发送状态顺序保留。`raw_message_bytes`、`decoded_attachment_bytes`、`decoded_body_bytes`、`mime_structure`、`header_bytes`、`wire_response_bytes`、`scan_items` 按实际失败来源报告；数值单位使用 bytes 或 items。持久会话提交额度拒绝使用 `cache_quota`。

## MCP and mail

`get_email` 的正文先受 MIME 解码和正文长度选项约束，再由 `server::bounded_email_result` 按实际 JSON-RPC 响应二分选择可容纳的最长 UTF-8 前缀。计量包含真实 id、request_id、全部元数据及两份 JSON 内容；需要额外截断时设置 `body_truncated=true`，保留已有截断标记。`body_byte_limit` 仍表示正文选项与解析硬上限的交集，实际前缀可能更短。非正文元数据不裁剪，空正文仍超限时保留有界 `LIMIT_EXCEEDED`。批量读取和会话也按实际完整响应预算统一缩短展示正文，保留元数据、UID和逐项结果；发送结果使用状态降级路径。

服务使用官方 Rust MCP SDK rmcp 3.8.0 与 Streamable HTTP。IMAP 使用 async-imap 和证书校验的 TLS；SMTP 使用 lettre 的隐式 TLS 或必须成功的 STARTTLS。每个需要邮箱后端的操作获取四个执行槽位之一，连接归单次操作所有，在完成、超时或解析失败后关闭。只读的本地账户/发送状态查询不需要邮箱连接。

账户共享 4 个后端槽及 20 个运行/排队请求，排队最多 10 秒。每账户常规 2 槽，同类任务中先恢复常规份额，再借用空闲槽。释放槽位时优先选择可运行的前台请求，后台全局最多 1 槽，并为前台保留最后一个准入名额；已获许可的任务运行到当前有界步骤结束。后台按账户、目录和工作阶段轮转，退避及暂停释放许可。取消、超时及 future 丢弃释放排队名额与许可；已经开始的发送保留原持久化结果流程。附件 64 个句柄、4 个传输许可和头部缓存 256 项/16 MiB 均为全实例额度。

每账户实际 TCP/TLS、欢迎响应或登录失败，以及后端操作超时，触发 1 秒读取退避。期间该账户的新读取返回可重试 `BUSY`，附件 GET 返回 429；服务不自动重试，其他账户可继续。队列等待、本地输入/MIME 错误及连接建立后的命令错误沿用各自结果。账户发现、历史发送状态和旧写工具独立于读取退避。下载的后端计时从取得执行槽开始，客户端取消读取时释放对应队列、连接及许可；旧写任务继续持有其执行状态直到原生命周期结束。

读取必须使用 `EXAMINE` 和 `BODY.PEEK`。状态 FETCH 只接受所请求 UID 的结果，忽略其他 UID 或无 UID 的非请求更新；目标 UID 未返回时报告 `NOT_FOUND`，已返回目标 UID 但缺少 FLAGS 时报告 `MAIL_BACKEND`。使用 vendored `Fetch::has_flags` 保留同一 UID 最后实际出现的 FLAGS，显式 `FLAGS ()` 成功返回空集合。整封读取按目标 UID 及所需 SIZE/BODY 属性筛选，异步 FLAGS 不代替所需属性；缺失所需属性仍返回 `MAIL_BACKEND`。列表/搜索按请求 UID 归并 HEADER、SIZE 与最后实际出现的 FLAGS。只有明确返回 HEADER（包括真实空头部）才生成摘要，每个 UID 一条、按 UID 降序且数量不超过 limit，忽略其他 UID/无 UID 响应；缺失 HEADER 不从缓存伪造摘要。所有结果均等待匹配 tag 的 OK，不以已收到部分数据代替成功终结。依据 [RFC 3501 §5.2](https://www.rfc-editor.org/rfc/rfc3501#section-5.2) 与 [§7.4.2](https://www.rfc-editor.org/rfc/rfc3501#section-7.4.2)。不得以关闭会话为由引入 `CLOSE/EXPUNGE` 等额外副作用。

连接、原始响应和单封邮件都必须有预算；`BudgetStream` 对连接累计读取字节计量；`Client::with_buffer_limit` 在真实 parser 的 `decode_needs`、扩容及剩余数据重置前执行同一个应用字节上限（`max_message * 4`，默认 128 MiB，另保留上游 512 MiB 硬上限）。已用字节与 parser 所需追加字节采用 checked arithmetic；初始化、分块取整和替换缓冲区同样不超过该上限。移除独立的行尾 literal 猜测扫描，合法响应文本中的花括号不会被当作 literal。预算约束每个 parser 缓冲块，非进程 RSS；增长时新旧块、已交付响应及协议对象可能同时存活。列表和搜索最多扫描 500 个 UID、返回 50 条摘要，批量读取最多 10 封并保留逐项结果。具体字节、结构及输出限额见 README；调整数值仍须满足 D3 并评估客户端兼容性。

R06 的协议基础在本地 `imap-proto` 中增加 `Response::parse_bounded`，async-imap 的 decode 默认使用该入口；原 `Response::parse` 继续作为上游兼容接口。每个完整响应中的 BODYSTRUCTURE 共用深度 32、节点 1024、单字符串 128 KiB、单节点参数对 256、总参数对 8192 和累计结构记账 1 MiB 的预算。嵌套 message/envelope、扩展列表、参数、语言和地址均纳入预算，literal 声明在请求更多输入前检查，字符串转换和容器容量在分配路径核算。可恢复的解析分支回溯恢复相应计数；预算耗尽沿不可恢复 Failure 传播。

预算错误仅携带固定类别、上限和单位。async-imap 通过 `error::body_structure_limit` 识别该类型，预算错误路径直接返回静态信息；读取路径使用统一错误映射生成 `LIMIT_EXCEEDED/mime_structure`。原连接字节/缓冲限制与 tagged completion 规则继续生效。来源和维护规则见 [imap-proto 补丁记录](../vendor/imap-proto/PATCHES.md)。

R09 搜索基础提供 opt-in 的 `Response::parse_bounded_with_search_limit`、`CommandReadLimits::search` 与 `Session::uid_search_bounded`。每条命令从响应开始到匹配 tagged completion 共用 128 KiB 字节上限，SEARCH 最多 200 个 UID，在分配结果向量前检查。异步 FLAGS/FETCH 的嵌套容器以独立 128 KiB 解析分配预算核算容量；状态文本和响应码只扫描，标量 EXISTS/RECENT/EXPUNGE 可以转交，其他通知在消费后释放，防止通知队列保留多块原始响应。普通 SEARCH/FETCH 入口沿用原契约。

有界搜索必须收到一条显式 SEARCH 和匹配 tagged OK；空 SEARCH 合法。重复 SEARCH/UID、错误 tag、BYE/EOF、意外 continuation 和预算错误终止当前读取及后续命令写入。开始前已缓冲的 SEARCH/完成响应不参与新命令，匹配完成后的缓冲尾部保留供后续读取。typed `SearchLimitError` 只携带类别和数字上限。该基础接口由后续跨文件夹编排按最多 200 个 UID 数值的闭区间调用，窗口检查点在命令成功后推进。

IMAP 文本模式固定为 rev1：所有邮箱参数编码为 modified UTF-7，LIST 名称解码为 Unicode 后验证规范往返。不得直接把 UTF-8 字节放入 quoted search；非 ASCII 查询使用 `CHARSET UTF-8` 与同步 literal，等待 continuation 和对应命令的 tagged OK。`BADCHARSET` 返回 `UNSUPPORTED_CHARSET`，不降级为含义不同的查询。当前未协商 `UTF8=ACCEPT` 或 IMAP4rev2。

async-imap 0.12 的 APPEND flags 原样序列化，必须传入 `(\Draft)` 这样的带括号列表。合成 duplex 回归覆盖 literal continuation 及 OK/NO/BAD。vendored completion-status 补丁只允许匹配 tag 的 OK 成功；NO/BAD、缺失终结响应和错误 tag 均失败。SELECT/EXAMINE（含 CONDSTORE）和 STATUS 共用这一完成边界；无响应或只有 `0 EXISTS`/部分元数据的断连不能解释成成功空邮箱，列表/搜索不得在未完成 EXAMINE 时返回空页。来源、退出条件与审计方式见 [PATCHES.md](../vendor/async-imap/PATCHES.md)。

IMAP TCP 连接在 TLS 握手前启用 `TCP_NODELAY`。命令编码器按 tag、空格、命令和 CRLF 分次写入，小包应及时发出，避免 Nagle 与对端延迟 ACK 在连续分块读取中累积等待；连接期限、传输预算和 tagged completion 规则保持一致。设置失败沿用固定 Backend 错误。真实 TLS 登录后的客户端 socket 选项由 `mail::tests::production_imap_login_enables_tcp_nodelay` 验证，依据 [RFC 9293 §3.7.4](https://www.rfc-editor.org/rfc/rfc9293.html#section-3.7.4)。

APPEND 等待同步 literal continuation 时，先将合法 untagged 通知交给现有有界 unsolicited 队列，再继续读取；只有 `Continue` 才发送正文。continuation 前任何 tagged 结果（包括提前 OK 和错误 tag）、BYE、EOF 或读取错误均失败且不发送 literal，拒绝后排队的 continuation 不能恢复命令。完整 literal 后仍等待匹配 tag 的 OK。`save_draft_waits_for_continuation_after_notifications` 和 `save_draft_append_protocol` 覆盖这一 D2 协议边界；不改变草稿 flags/MIME 或发送策略。

`mail/parts` 负责有界 BODYSTRUCTURE/必要 MIME 头与原 raw MIME 的统一 PartManifest；`mail/attachments.rs` 编排附件清单、准备和下载生命周期，`mail/attachments/metadata.rs` 负责稳定引用、索引选择及描述。`get_email`、列表、内联 Base64 和二进制下载共用原 MIME 遍历索引（0–1023）与附件子树边界。`att_v1_` 引用用带域分离的 SHA-256 绑定 AccountKey 命名空间、文件夹、UIDVALIDITY、UID、索引和 MIME 引用版本；同时传入 id/index 时核对一致性。描述中的原文件名保留展示用途，安全文件名另行约束路径字符、控制字符、长度及保留设备名。

附件列表和准备先以独立 UID FETCH 命令取得 RFC822.SIZE 并等待 tagged OK，再取得 BODYSTRUCTURE，逐条筛选目标 UID 和必需属性。只有确认原始大小不超过配置/32 MiB 上限后，才允许结构超限、明确结构拒绝或映射待核对触发一次新连接 raw 回退。发布索引前，逐个核对实际 DFS 节点的完整 MIME 头；确认整体附件或 message/rfc822 后停止展开该子树。重复 MIME 元数据、Base64/QP multipart 或缺少 boundary 的 multipart 进入受控整封回退，保持与 raw 解析的索引一致。必要头 section 由服务器端生成，每命令最多 16 个、最多 64 命令、单头 128 KiB、合计 1 MiB；按 UID、section、origin 和必需属性核对，并等待每个命令完成。协议失败直接返回；头证据超出预算允许一次 raw 回退。第二连接重新 EXAMINE 并核对 UIDVALIDITY，沿用原操作 deadline，raw 失败直接结束。成功结果通过固定 `read_diagnostics` 保留来源和回退原因。

`prepare_attachment_download` 保存最多 64 个主体绑定、600 秒有效的元数据句柄。GET 再核对主体、AccountKey、UIDVALIDITY 和部件清单，按服务器生成的 section 以最多 64 KiB 的 `BODY.PEEK[section]<offset.count>` 读取目标编码字节。每条命令核对 UID、section、origin、必需属性和唯一片段，等待匹配 tagged OK 后才推进偏移；完整编码长度来自已核对的清单，末尾空片段探针确认结束。空附件也执行命令及完成状态验证。命令期 literal 限额在 parser 请求更多字节或分配前执行，超限终止连接；默认 raw reader 的连接与缓冲预算保持一致。依据 [RFC 3501 §6.4.5](https://www.rfc-editor.org/rfc/rfc3501#section-6.4.5)。

增量解码器保存跨块 Base64/quoted-printable 状态，每步写入最多 64 KiB 输出；解码累计上限为 32 MiB，连接累计上限为 128 MiB。整封邮件超过 raw 上限时，已验证映射的小附件仍可按部件读取；需要整体解析的结构继续只在 raw 大小上限内回退一次。既有内联 Base64 保持 raw 路径和输出预算。`read_diagnostics` 继续描述列表/准备阶段的元数据来源。

`mail/attachments/staging` 管理实例共享的四份私密下载文件，每份最多 32 MiB。文件写入、完整解码及所有命令完成后，可信 reader 提供绑定本文件的完成凭据，才生成可交付文件；内容长度与 SHA-256 取自最终文件元数据。异步文件操作将文件和额度交给受控阻塞任务；请求取消时，运行中的任务仍持有资源，完成或失败后清理。响应消费或丢弃后释放传输许可并删除文件；清理失败保留占用并停止新文件写入。启动时取得专属目录锁，只清理可识别的残留文件。配置及恢复见[部署指南](deployment.md#准备目录与配置)。

下载从创建暂存文件开始固定一个期限，等待邮箱槽位、协议读取、解码、写入和最终校验均使用剩余时间。邮箱队列仍保留 10 秒等待上限；超时发生在实际网络读取阶段时记入账户退避，暂存或队列阶段的超时保持各自状态。暂存创建超时返回 HTTP 504、`phase:staging`，运行中的文件任务继续持有许可直到结束。文件开始发送后，响应消费继续持有下载与文件额度，邮箱执行槽已释放。

HTTP 内容固定为 octet-stream，带 no-store/nosniff、`X-Content-SHA256` 和 `Content-Digest`。响应头发出前的错误使用[附件契约](capabilities.md#附件与兼容性)中的小型 JSON；文件传输中的读取失败终止流，由客户端校验长度和摘要。仅完成命令且未出现目标 UID 才判为消息不存在；目标 UID 缺必需属性、大小不一致及命令失败返回后端错误。

`mail/insights.rs` 提供有界标识符图会话、精确 Message-ID 定位、严格 STATUS 统计、脱敏诊断与正文展示。头部缓存最多 256 项/16 MiB、60 秒，只保存不可变头部并以新 UIDVALIDITY 失效；正文共用 `mime`/`html` 选择器。统计用 `imap_text::status_counts` 验证完整字段和命令完成。批量修改使用明确 UID、总体期限和逐项结果，失败项保守为 unknown、后续 unprocessed；没有持久化修改日志，重试前必须检查邮箱。范围与限制见 [能力说明](capabilities.md)。

## Cross-mailbox conversations

`read_conversation` 通过 `mailboxes`、`scope` 或 `cursor` 显式进入跨文件夹模式，旧单文件夹参数与输出保留。账户 ACL 在排队、游标访问和连接之前验证，游标绑定 principal 与 AccountKey 命名空间。standard 范围只采用协议 INBOX、服务端 Sent 与管理员 archive 配置，排除 All/Junk/Trash/Drafts/NoSelect；名称候选不授予范围。首次固定每目录 UIDVALIDITY 和 UIDNEXT−1，初始不可用目录保留错误，需重启查询才能重新冻结。

每个 `(目标标识符, HEADER 字段, 文件夹, UID 数值窗口)` 独立推进，匹配 tagged OK 后保留全部命中并将搜索窗口记为完成；待核验命中单独计入 pending_candidates，核验结束前关联图保持不完整。HEADER 子串筛选后精确解析关联字段；合法非 ASCII/quoted ID 使用稳定 ASCII 搜索锚点或空 HEADER 条件，避免筛选漏掉合法标识符。每页共享 200 次候选头读取、20 个输出位置、256 KiB 展示正文、256 个搜索窗口和配置期限与 60 秒中的较小值。每窗口最多 200 UID、命令及 parser 128 KiB；种子头单独读取。普通及恢复连接最多 8 个，正文 raw 回退按完整 MessageRef 计一次，各连接保留原累计传输预算。身份 raw 核验每页最多 256 KiB、每项至多 2 秒或剩余时间四分之一。

同一 Message-ID 的关键头按固定字段顺序核对各字段原始值，同名字段保留出现次序和长度；RFC2047 显示文本不参与证据归一化。完整 root MIME 正文字节的 SHA-256 使用固定指纹方法版本，raw 身份头还需与已取得的候选头一致。全部关键头与完整指纹一致才折叠；已知头/内容冲突标 collision，指纹缺失或不可比较保留 identity_unverified。缺失、非法或重复自身 ID 按完整引用独立保留。循环和 frontier 的所有增长计入状态容量，跨页输出新实体、新变体和新增位置，原来源始终保留。

全实例游标最多 64 项、8 MiB，实际 owned capacity 与执行中租出的状态均计入；TTL 为 10 分钟，成功推进后刷新。互斥锁只保护租约、配额及提交，不跨网络等待。首次未发布租约取消后释放，已有租约取消可保留已完成搜索进度，但不确认未交付位置。先生成含实际下一 token 的 Prepared 快照并按完整 JSON-RPC 预算裁剪，再同步 CAS 提交位置和重放信息；超限失败保留原 token。容量失败返回覆盖缺口与重启/缩小范围动作。

同 token 重放固定最近已交付的位置、覆盖、错误快照及原响应裁减标记；续查和重放均复核冻结目录的 UIDVALIDITY，任一变化使整个游标失效。复核轮转顺序并分摊期限；正常推进只交付本页已核对目录的来源，副本代表可改选为同变体的健康位置。同页某完整引用的头读取失败后，其他关联搜索命中该引用也保持阻断，下一页再处理。重放目录复核失败返回可重试错误并保留 token，避免交付旧的完整性状态。正文覆盖通过 body_requested/body_returned 和 omitted_refs 单独报告。

对应 D1/D2/D3/D5 的验证入口为 `src/mail/conversation/tests`、`src/server/output_tests/conversation.rs` 与 `tests/thread_search_budget.rs`：覆盖身份碰撞、配额及取消、跨目录关联和固定水位、窗口失败、主体/账户边界、UIDVALIDITY、响应预算、重放和虚拟时间过期。真实 handler 结果用声明的 outputSchema 核验，所有邮件与 IMAP 服务均为本地合成数据。

## MIME and HTML

`mail/listing` 在一个绝对期限内完成列表或搜索。附件筛选使用 PartManifest，必须取得目标 SIZE 和验证的 MIME 头；显式策略回退每页最多一次，关闭旧连接、复查 UIDVALIDITY 与大小，再严格读取 raw。回退后以最后已检查 UID 形成短页，剩余候选由原游标继续。两条串行连接共享同一 `BudgetStream` 剩余额度，新连接的 parser allocation limit 采用该剩余值，保持每页原有传输上限。

摘要先完整消费匹配 tagged OK，再解析或缓存。HEADER 请求长度与单条 literal 的分配前上限均为 1 MiB，严格核对目标 UID、HEADER section、origin 0 和唯一头数据；FLAGS-only 不构造空摘要，实际 FLAGS（含空值）按到达顺序更新。短于取样上限的头可以是无正文、无终止空行的完整头；达到上限且没有结束标记时，分类仅使用已完整观察到的逻辑字段，标记 partial。摘要解析共用 MIME 头行数限制。头部缓存沿用各自的 128 KiB 字节及完整性检查，超出缓存上限的摘要头只用于本次响应。连接累计字节和 JSON-RPC 输出预算同时生效。

`mail/parts/preview` 共用完整正文的候选选择、transfer/charset 解码与 HTML 安全策略。每封 8 KiB、整页 128 KiB 编码申请额度包含非空和 EOF 探针，失败命令不退还；取样最多输出 200 Unicode 码点并跨叶折叠空白。只有完整取得所选叶且验证结束时才 flush transfer/charset 或 HTML EOF。部分 HTML 只处理已完成 token，保留原复杂度与 foreign-content 拒绝。预览期限设为 5 秒与总期限剩余一半中的较小值；网络等待按期限取消，同步 MIME 解析受字节/结构界限约束，取样完成后再核对期限。本地错误可继续、协议错误或超时停止后续预览，已经取得的摘要与游标始终保留。筛选已读回的单个 raw 只为匹配 UID 保留至该项本地取样结束，最多 32 MiB，并共用页预览额度。

列表分类采用固定 `headers-v1`：信号只包含允许字段名和类别。退订 URI 的语法、2048 字节及协议检查均在本地完成，不访问外部地址。完整响应预算通过 `bounded_listing_result` 计入真实 JSON-RPC id、request_id 及转义副本；必要时从尾部撤掉预览并标记 wire_budget_skipped。状态字段仍超限时，整页省略全部可选预览字段，保留摘要、分类及分页信息；仅基础元数据超限返回 LIMIT_EXCEEDED。

`get_email`、`get_emails` 和 `read_conversation` 共用 `mail/parts/body` 的正文读取会话。先完成独立 SIZE、BODYSTRUCTURE 和必要 MIME 头核对，再由 `mime/selection` 在 raw 树与已核对部件树上使用同一候选规则。所有选中叶按原 DFS 顺序共享编码长度四倍的 32 MiB 解码预留和 HTML 预算；transfer 与 charset 状态跨 64 KiB 块保留。完整正文验证后应用显示截断，包括 `max_body_length:0`。成功输出增加有界 `read_diagnostics`，UTF-7 的固定 raw 回退原因为 `charset_utf7`。正文末尾探测按目标 UID、section 和声明长度偏移请求 1 字节。匹配 tagged OK 后，空结果表示正文结束，1 字节触发 `body_eof_mismatch`，在同一累计传输预算和期限内按整封大小上限回退一次。新连接重新 EXAMINE，核对 UIDVALIDITY 和 RFC822.SIZE 后读取整封。

`get_emails` 按明确的命令完成状态区分失败：匹配 tagged OK 后的本地 MIME、字符集或展示错误只记录单项失败并继续；映射失败且整封大小超限时，同样检查元数据命令是否完成。目标数据不匹配、必要属性缺失、header/body 协议失败或连接预算错误停止后续读取，后续 UID 明确 `unprocessed`。`completed` 保持已尝试输入项数的含义，重复 UID 保留输入顺序及逐项结果。错误码与连接可继续性分别记录。依据 [RFC 3501 §2.2.2](https://www.rfc-editor.org/rfc/rfc3501#section-2.2.2) 的命令终结语义。

需要 raw 策略时，每 UID 最多回退一次，先关闭旧连接，在原操作期限内重新连接、EXAMINE 并核对 UIDVALIDITY。新连接完成精确的目标 UID/section/origin/长度与 tagged OK 核对后，才进入本地解析并用于后续 UID。候选头扫描和部件命令的协议失败直接终止；raw 失败不再次回退。头部证据的字节与 8192 行预算跨整个已核对 MIME 树共享，默认字符集按 MIME 缺省语义比较。

邮件正文、主题、头部和附件均按不可信输入处理。构建 MIME 结构前检查共享的节点、深度和头部预算；transfer/charset 解码前预留预算。不得恢复对不可信输入调用 `mailparse::parse_mail`；回复和草稿发送只需要有界头部。boundary 按完整分隔符匹配，接受允许的尾部空白，不能把相同前缀当作分隔符。

依据 [RFC 2045 §6.1、§6.4](https://www.rfc-editor.org/rfc/rfc2045#section-6.4)，缺省 transfer encoding 为 7bit，编码名不区分大小写；未知编码实体不按声明的 MIME 类型解释。解析时对未知编码停止 multipart 递归，选择时把非空内联实体视为不支持，允许 alternative 回滚并选另一完整候选；没有候选时返回既有 `UNSUPPORTED_BODY`。`encoded()` 在调用 mailparse `Body::new` 前显式验证编码，不能依赖该库把未知值默认为 SevenBit 的行为。`body_raw` 同样拒绝未知编码；附件工具返回 `UNSUPPORTED_TRANSFER_ENCODING`，认证 GET 映射 HTTP 422，坏编码解码失败单独映射 HTTP 502。显式附件分类和共享索引不变，真正空实体不被当作非空正文错误，原有结构、头部和解码分配前预算继续适用。

正文选择排除显式 attachment 或带文件名的部件及其整个子树；`multipart/alternative` 选一个受支持表示并优先纯文本，`multipart/related` 只选主部件。读取和转发共用选择逻辑，附件列表和下载共用分类规则，multipart 附件可整体下载。真实空正文与“不支持或无法完整解释的正文”必须区分。

`mime::Part::is_attachment` 按 [RFC 2183 §2.8–2.9](https://www.rfc-editor.org/rfc/rfc2183#section-2.8) 将显式但服务不支持的 disposition 类型（含 form-data）按附件处理；multipart 分类作用于整个子树。锁定的 mailparse 0.18.0 将缺省/空类型解析为 `Extension("")`，因此不能机械地把全部 Extension 判为附件；缺省、空或仅空白类型保持原有语义，inline 仍可内联，filename/name 的既有附件判断独立生效。正文、转发、附件发现和下载统一调用该分类，不新增索引或解码路径。

依据 [RFC 2046 §5.1.4](https://www.rfc-editor.org/rfc/rfc2046#section-5.1.4)，alternative 每个候选表示同一份信息，选择必须覆盖候选的完整内联正文。纯文本探测遇到非空 HTML 时判定该候选不完整，回滚其中已收集的所有片段（如 plain 签名），继续寻找完整纯文本表示；不存在时才允许 HTML 转换。此规则沿 mixed、嵌套 alternative 和 related 主部件递归传播，附件仍排除；真正空的 HTML 不引入待转换内容，真正空的 plain 仍是有效完整表示。探测不解码、不分配正文，最终选中叶子继续共用既有解码预算。

HTML 回退只用有界 tokenizer sink，不引入 DOM/layout 引擎、不执行脚本或加载资源。tokenizer 前检查输入和属性复杂度，处理时继续限制 token 与输出；属性复杂度必须覆盖无空白的相邻 quoted attributes 和 slash recovery。tokenizer-only sink 无法解释 SVG/MathML 外来内容及其 CDATA，出现时即使已有可见文字也拒绝整段正文。其他非空但无法完整解释的内联内容同样显式失败，转发必须在发送 claim 前拒绝不完整原文。所有新构建纯文本邮件显式声明 UTF-8，保持转发中的 Unicode 内容。

读取可显式选择 `html_display:readable`；默认和转发继续使用 `legacy`。两种格式复用同一 MIME 候选选择器，读取解析一次 MIME 后分别提取元数据及所选格式正文。`html/readable` 维护有界格式状态：16 层列表/引用/表格、256 个链接、每链接 2048 UTF-8 字节；实体解码后验证 http/https/mailto 协议、控制字符和长度。多个所选 HTML 叶子共用 token、输出和链接预算。输出含 `legacy-v1` / `readable-v1` 及固定原因省略计数，保持外来内容拒绝和原有输入/标记复杂度约束。

## Send lifecycle

发送先提交 operation key、参数摘要与 Message-ID，再进入 SMTP。参数摘要包含工具名与参数；并发请求通过同一去重键竞争执行权，重复调用读取已存状态。SMTP 最终响应丢失、超时或取消后不能猜测已失败，更不能自动重试。进程重启将未完成的 `sending` 记录改为 `unknown`。操作键默认持续保留；保留和迁移规则见维护文档。

SMTP 使用 lettre 的低层 connection/command/message API，保留 TLS、AUTH、SMTPUTF8 和 8BITMIME 策略。连接及事务共用发送期限；最终 DATA 250 确认后先记录 `accepted`，明确拒绝先记录 `rejected`，再执行独立的最多一秒 QUIT 清理。`unknown` 或取消的事务直接关闭，不 QUIT、不进连接池。执行槽位持有到清理结束，避免积累清理任务。HTTP/MCP 调用者断开不能把发送生命周期变成不受记录的重试。

进程收到停机信号即封闭写入登记并停止接受新 HTTP 连接。已登记的写任务保留共享队列和执行许可，HTTP、同步、写入和取消后仍持有许可的读取或附件传输共用从信号开始计时的绝对截止。只有任务和缓存队列完成排空且仍在期限内，缓存才发布 clean receipt。若排空未完成，进程保留运行见证锁至退出。SMTP 已确认结果先持久化再清理，未完成的 sending 在下次启动恢复为 unknown。

异步 SMTP 响应在解密后按每行 1,000 字节、每个完整回复 100,000 字节限制累积，计入行结束符。每次追加前核对剩余预算。回复用尽预算仍未完成时丢弃连接和预读数据，跳过 QUIT。该路径覆盖 greeting、EHLO、STARTTLS、AUTH、DATA 和 QUIT。DATA 最终确认超限按未知结果处理，已持久化的接受结果在 QUIT 超限时保留。来源与替换条件见 [lettre 补丁记录](../vendor/lettre/PATCHES.md)。

已知 SMTP 接受结果不得因后续日志/数据库更新或清理失败而改为失败。操作记录更新失败时，本次响应保留 `accepted` 并附 `journal_update_failed`；这不等于数据库已经成功保存该结果，后续必须检查操作状态及服务商记录。

草稿发送使用同一个有界规范化头部缓冲区做收件人解释和隐私过滤，在 claim/SMTP 前验证字段名、续行及头/体分隔符，并明确拒绝不支持的 `Resent-*` 语义。To/Cc/Bcc 的每个原始结构化头通过 `mailparse::addrparse_header` 解析，编码显示名称中的标点保持显示数据；重复字段和组地址共同构成信封。BCC 参与信封投递但不进入发送头部；可用于 DATA 的正文原始字节（包括 Base64/quoted-printable 编码附件）保持不变，源草稿保留并返回 `draft_retained`。在 `prepare_draft` 和共同 `claim_send` 边界验证 CRLF、无 NUL、每行最多 998 原始字节（dot-stuffing 新增点不计）；不合规返回固定 `UNSUPPORTED_DATA_FORMAT`，不 claim、不连接，不盲目规范化二进制/MIME 正文。末行无 CRLF 仍允许，由现有 lettre DATA codec 补终结。`SmtpSender::submit` 同样在连接前做防御检查，不将确定未提交的非法格式标为 unknown。8bit 数据仍需 SMTPUTF8/8BITMIME 能力检查，当前未协商 BINARYMIME/BDAT。发送前格式验证见 `smtp::tests`，结构化收件人及隐私过滤见 `mail::tests::draft_envelope_and_privacy_filter_share_canonical_fields`。

SMTPUTF8 需求为信封中非 ASCII 地址或 `mime::requires_smtputf8` 检出的原始国际化头。按 [RFC 6531 §3.2、§3.6](https://www.rfc-editor.org/rfc/rfc6531#section-3.6) 检查所有 MIME 子头，附件子树也不排除；只检查原始字节，RFC 2047 编码词和叶子正文不展开。identity 编码的 message/rfc822、message/global 递归共用结构/头部预算；编码封装的 message/global 保持不透明，符合 [RFC 6532 §3.5](https://www.rfc-editor.org/rfc/rfc6532#section-3.5) 对 7bit 封装的支持。此检查不调用正文选择、HTML 渲染或 transfer/charset 解码；非 ASCII 输入无法在结构/头部预算内确定需求时，连接前返回确定 `rejected`。

服务端缺少邮件所需的 SMTPUTF8 或 8BITMIME 时，在 MAIL 前返回类型化拒绝，持久化为 `rejected` 后执行有界 QUIT。SMTP DATA 已提交但最终确认丢失时保持 `unknown`，已知接受仍为 `accepted`。


## Authentication

Rauthy 0.37.2 是选用的身份服务，使用嵌入式 Hiqlite/SQLite，负责 authorization code/PKCE、用户登录、refresh rotation 和会话持久化。邮件服务对每次进入 MCP 的 HTTP 请求校验 RS256 签名、issuer、resource audience、expiry、nbf、scope 与 subject allowlist；不能把已有 MCP 会话当作免验凭据。公开发现信息和健康检查保持各自有限职责。

MCP 和附件 GET 共用 HTTP Bearer 鉴权。认证方案按 ASCII 大小写无关规则匹配，方案与 JWT 由一个或多个 ASCII 空格分隔，JWT 以原始字节进入验证。签名 HTTP 回归覆盖两条路由的合法大小写、非法分隔、无效和过期令牌。

JWKS 的硬缓存期限为 300 秒，不因刷新失败延长。刷新 I/O 使用独立协调器，仍有效的已缓存 key 不等待其他 kid 的网络刷新；成功和失败刷新结果共享 10 秒冷却。JWKS 暂不可用返回 HTTP 503 与 `Retry-After`，无效令牌返回 401。JWT 选择 AWS-LC 后端时的动机是避开当时锁文件中存在已知风险的 RustCrypto 传递依赖；持续安全状态以当前依赖审计为准。

静态 OAuth 客户端明确回调 allowlist，生产与测试分离用户和客户端。Rauthy 允许提前 refresh，保留轮换和五秒宽限。单节点 Hiqlite 使用 immediate durable Raft log sync、一个读连接、一个 HTTP worker 和一次并发 Argon2id 哈希（32 MiB、三次迭代、两通道）。多会话互不干扰、提前刷新、重启恢复及资源观测是认证/部署变更的验收内容。

部署时保留 WebAuthn origin 的显式 `:443`，ChatGPT callback 精确复制客户端当前设置界面值。Rauthy 0.37.2 的 discovery 遗漏其支持的 public-client `none`，按 [deployment](deployment.md) 修正；升级或变更域名、端点、scope 时重新导出核对。登录限流不覆盖静态 asset/theme；验收全部登录页脚本/样式及突发加载，HTML 200 本身不代表登录页可用。

## Persistent metadata cache

`CACHE_CONFIG_PATH` 在 Journal 恢复前解析严格版本化配置，省略或显式 off 不打开缓存。metadata/full 只接受已登记账户的明确文件夹列表，使用内部 AccountKey 命名空间分库。每账户 `cache.sqlite` 保持 application_id、schema 版本、namespace 和恢复世代，元数据键包含 mailbox/UIDVALIDITY/UID。metadata 默认新建 v1；full 以独立版本事务依次升级为 v2/v3/v4，metadata 入口也可打开已升级的数据库。v3 保存列表观察、角色化精确标识索引及完整正文索引；v4 保存历史、新 UID 和存在性核对检查点以及逐层内容任务。目录权限、根清单、符号链接、硬链接与保留路径冲突先核对，未知非空目录不接管；根锁在所有连接关闭后显式释放；子进程短暂继承文件描述符时仍可立即重开，活动实例继续互斥。

全局与每账户额度按物理文件占用计算，包含主库、DELETE 回滚日志、清单、暂存及旧分区。事务前预留日志与主库增长，限制 SQLite page_count，避免用逻辑行长度代替磁盘用量。短事务内更新元数据、部件证据和索引。提交成功后保留返回值及 revision，后置磁盘诊断失败只阻止后续写入并报告故障。原操作库与账户注册库保持独立。

在线读取成功完成相应 tagged 命令后才登记证据。`mail/persistence` 使用 16 项/8 MiB 的单 FIFO 队列，任务和 active worker 均持有容量许可。成功 EXAMINE 的世代观察先更新共享隔离状态，再排队持久化；旧世代消息、状态和游标立即被过滤。完整头、部分头和已核对部件清单分别计数；索引只使用完整、版本一致的输入。长词 FTS5 trigram 先有界取得至多 201 个 posting 再过滤范围，短词最多检查 200 条并保留续查位置，空页也可续查。

会话 codec 采用固定版本和宽度、长度/计数前置检查、命名空间与摘要校验，所有 owned capacity 和验重索引 scratch 在创建及增长时计入持久 reservation。恢复时验证图索引、目标/候选/位置唯一性、水位、已交付状态和 replay。休眠磁盘记录、内存状态、恢复过程和执行租约共用 64 项/8 MiB。完整 wire fit 后，在一个 SQLite CAS 事务中提交 state 投影、active token 和最近 replay；执行中 blocking job 继续拥有租约，取消与数据库失败保留未交付项。持久期限为原绝对到期时间，重启不刷新。

`cursor_storage` 反映实际 memory/persistent 模式。仅整个冻结范围都被管理员配置选中时使用持久模式；选中但不可用的范围明确失败，已存在持久 token 不转入内存。`get_cache_status` 先授权，只返回当前账户的配置、覆盖、配额及故障；本地读屏障最多等待 2 秒，不连接邮箱。备份恢复命令 `--cache-restored` 在 Journal 前执行，原子提升根恢复世代、清除游标、标记 dirty，未启用分区以后打开时也应用该世代。单次 EXAMINE 更新目录世代，完整存在性核对负责清除 dirty。

metadata 提供持久元数据、部件证据和在线会话续查；full 增加原始邮件/派生内容、显式离线读取及写操作 dirty 范围通知。R12 接入全量扫描和受影响范围的只读收敛。D1/D2/D3/D5 验证位置为 `src/cache/tests.rs`、`src/config/cache_config.rs`、`src/mail/conversation/tests/durable.rs` / `persist.rs` 和 `src/server/output_tests/cache.rs`；配置/运维入口见[缓存设计](cache-design.md)。

### Full-cache storage foundation

full 配置经 `Cache::open_full_with_run` 绑定运行见证。`cache/blob` 保存完整 raw、带解析版本的完整正文和按原 MIME index 定位的附件。状态区分 materialized、derivable_from_raw、missing、omitted 与 unsupported，层覆盖统计本地已观察数据。完整历史范围的完成标记由 R12 扫描检查点决定。

文件使用私有随机名称和 64 KiB I/O 块，单个暂存最多 32 MiB。暂存预留、数据库及回滚日志预留共用全局/账户物理预算，旧世代和待清理文件继续计费。完整文件同步并无覆盖发布后才用短事务登记；失败或取消保留资源所有权直到清理完成。原始内容身份与派生引用保持一致，文件丢失、损坏或回收也必须保留这一约束。

文件引用查找、打开与 pin 在同一临界区完成，同一描述符完成长度及摘要校验后才可交付。离线 lease 绑定目录修订号和恢复世代，dirty 后旧离线 lease 保持失效，读取按块复核。LiveVerified lease 由调用方先在线核对，保持完整邮件引用、文件与恢复世代绑定。回收等待 pin 归零，关闭描述符后释放 pin，物理删除完成后释放磁盘额度。

`cache/dirty` 在同账户事务中登记多个源/目标 scope，包括尚未 EXAMINE 的配置目录。dirty ticket 绑定当前 Cache 实例、恢复世代及 claim ID，关联受影响 scopes；核对 stamp 另绑定 UIDVALIDITY 和 scope 修订；存在活跃 claim 时拒绝取得核对 stamp，清除时再用 UIDVALIDITY/修订/无活跃 claim 条件 CAS。结束 claim 保留 dirty，异常退出和备份恢复要求核对。`mail/cache_write_guard` 在既有写操作的首个实际副作用前登记范围，保留超时、取消和阻塞清理的所有权；缓存失败先关闭离线访问，再保留邮件操作的实际结果。

前台列表和搜索在读取 FLAGS 的 UID FETCH 前捕获所选 UIDVALIDITY 和 SQLite 目录修订。捕获时要求该目录没有活跃写 claim，写入列表事实的同一事务再检查账户、目录、世代、恢复世代、修订及活跃 claim。写前的读取或排队证据在写后核对完成后仍会跳过缓存写回，在线结果照常返回。回归覆盖延迟回包、排队写入、写后新读及清洁重启。

`accounts/cache_run` 在每次启动（包括 off）于 Journal 前发布注册库旁的私有运行见证，并持有生命周期锁。full 清洁凭据绑定前驱运行、规范根目录及选定范围。写操作在调度排队前取得准入，关闭时封闭准入并等待既有写入及清理，再写入清洁凭据；异常退出和恢复使旧范围待核对。具体路径、备份和旧程序降级流程见[部署规程](deployment.md#持久缓存备份与恢复)。

`mail/cache_read` 和离线会话先授权，再使用与在线调用相同的 4 槽/20 请求调度；本地 blocking 工作持有共享许可直到实际结束。显式 cache 分支执行本地查询与完整性核对，保持零 IMAP；默认 live 分支继续核对 UIDVALIDITY 和目标存在性。结果报告来源、观察时间和覆盖；FLAGS 缺失保留未知状态。完整正文的文本负载与有界叶元数据分离，维持单文件 32 MiB，渲染与输出裁减在读取时执行；全文索引只接收完整内容。

离线日期搜索按 INTERNALDATE 原时区的日历日处理前台 IMAP 原文和后台已解析的日期。已知系统 FLAGS 在协议读取时按 ASCII 大小写无关规则识别，离线筛选兼容早期已存为自定义拼写的系统标记。显式空 FLAGS 按已知空集合处理，字段缺失仍为未知。对应回归位于 `src/mail/cache_read/query.rs`、`src/mail/sync/tests.rs` 和 `src/mail.rs`。

D2/D3/D4/D5 验证入口为 `src/cache/full_tests.rs`、`src/cache/content_tests.rs`、`src/accounts/cache_run/tests.rs` 和 `src/mail/cache_write_guard/tests.rs`；metadata 和持久游标继续运行 R10 回归。显式离线工具矩阵及故障场景见[实施设计](cache-content-design.md#验收矩阵)。

已完成 FETCH 的删除观察绑定原会话 UIDVALIDITY 与 UIDNEXT 上界；范围内未知 UID 留下持久墓碑以封存迟到证据，未来 UID 保持可分配。`fetch_miss_tombstones_only_uids_below_the_observed_uidnext` 和 `live_missing_future_uid_does_not_disable_cache_or_tombstone_later_arrival` 验证持久层和签名 HTTP 路径；`batch_observes_only_confirmed_existing_generation_misses_and_preserves_item_statuses` 覆盖批量写入前的目标存在性检查。协议失败及缺必要属性沿用原错误和停止规则。

### Synchronization foundation

R12A 的 `cache/sync_state` 使用 V4 保存独立历史、新 UID 与存在性/FLAGS 核对检查点，收据绑定命名空间、目录、UIDVALIDITY、冻结上界和恢复/dirty 修订。事实与检查点在同一短事务提交；完整头、头部全文与角色化标识索引共用既有实现。核对先于 EXAMINE 取得范围凭据，完成时 CAS 检查原修订、范围内存在性变化及活跃 claim。文件撤销会重排内容任务；正文索引等待新的完整正文，旧世代任务按每批至多 200 项清理。

内部扫描使用有界 SEARCH，只有合法单条结果的 UID 数量溢出可在新连接缩窗重试。新增 metadata FETCH 入口分别限制完整 wire 与 header literal，在解析分配前核对；Facts 每批至多 8 UID、2 MiB，FLAGS 核对最多 200 UID、128 KiB。重复 SEARCH/UID、缺属性、错误 tag 和协议预算失败保留原进度。共享 MIME 选叶和分块解码继续服务在线正文，内容工作单元只在 tagged completion、完整解码和文件登记后返回持久收据。

R12B 的 `mail/sync/worker` 接入 full 配置的可选后台循环，省略 sync 保持暂停。每步重新核对当前账户、有效读取授权与管理员范围；历史、tail、FLAGS/存在性、内容和清理交错执行，任务提交使用真实收据。关闭先取消并等待后台实际工作，阻塞 SQLite/文件操作及暂存清理继续持有共享许可；未完成清理或尚未持久化的世代/删除观察保留未清洁状态。

V5 独立保存派生文件的访问顺序，前台成功读取更新顺序，后台验证保持原值；压力回收复核同版本原件、产物和访问序号，保留租约。GC 按有界候选 keyset 推进，过滤后空批仍保留继续位置。范围发现与存在性核对分别报告，数据层完成还核对对应内容及索引证据。D1/D2/D3/D5 对应 `src/cache/sync_state/tests.rs`、`src/mail/sync/tests.rs`、`src/mail/sync/content/tests.rs`、`src/server/output_tests/sync_acceptance.rs` 与 `tests/thread_search_budget.rs`。完整分期、恢复及验收条件见[同步设计](cache-sync-design.md)。

R12C 通过可选 `sync.protocol:"auto"` 接入 CONDSTORE，默认 rev1。`mail/sync/delta` 在认证连接上执行有界 CAPABILITY、只读 EXAMINE 和 CHANGEDSINCE，每命令共用 128 KiB 线路预算，parser 独立约束 token、节点和 owned allocation；只接纳正 63 位 MODSEQ、完整选中证据与匹配 tagged OK，EXAMINE 必须确认 READ-ONLY。语义或解析失败关闭当前命令流，回退最多使用一个新连接并保留原期限及累计字节预算。普通前台协议接口保持原有语法。

V6 独立持久化扩展水位与修订。基线只能来自同一完整 rev1 核对首个页面之前的 EXAMINE；FLAGS 与 FLAGS/删除分别使用独立水位。每轮冻结 base、target、UID 上界及恢复/dirty/事实修订，每页至多 200 个已知 UID，事实与下一位置原子提交，末页才将 committed 提升至 target。并发范围内成员或 FLAGS 变化使旧页失效；范围外新增 UID 留给后续覆盖。遗漏的 CHANGEDSINCE UID 保留原存在性，周期 rev1 核对负责删除和 dirty 收敛。明确 NOMODSEQ 或 committed 回退先撤销进程内水位可见性，再持久化；存储失败保留撤销标记并阻止 clean receipt。D1/D2/D3 对应 `src/cache/sync_state/delta/tests.rs`、`src/mail/sync/delta/tests.rs`、`src/server/output_tests/condstore_acceptance.rs` 和 `tests/sync_delta_budget.rs`。

R12D 的 `mail/sync/qresync` 在 auto 策略中优先协商 QRESYNC，严格要求同连接 CAPABILITY、ENABLED QRESYNC 和匹配 tagged OK。完整非空已知集合至多 200 UID 时使用只读 EXAMINE QRESYNC，较大范围先 EXAMINE CONDSTORE 再按精确 UID 页 FETCH CHANGEDSINCE VANISHED；空集合跳过 FETCH。预览与实际页面通过范围修订和 UID 序列 CAS 复核，生成命令完整长度最多 8192 字节。parser 采用独立有界入口，完整命令预算 128 KiB，VANISHED 区间在分配前限数并保持压缩；EARLIER 必须属于请求集合，实时删除核对 UIDNEXT 与事件顺序。各协议回退共享每步骤至多两条实际连接、原期限和累计传输预算。

已确认删除先建立进程内范围围栏。已选择目录的 UID FETCH 逐帧处理实时 VANISHED，失败或取消保留围栏，EARLIER、FLAGS 与水位继续等待匹配 tagged OK。小量删除与 FLAGS/分页原子提交；超出有界提交额度时推进持久 dirty 修订并开启新的完整 rev1 核对，完成前暂停离线交付和 clean receipt。前台读写入口在网络之前捕获证据修订，正向缓存写入在入队和执行时核对；核对结束保留范围修订，拒绝旧网络结果的迟到写回。已开始的 FIFO 写入通过队列屏障结清后才冻结新的审计，使先前未知 UID 也纳入存在性核对。失败保留围栏，正常关闭仍等待队列与许可释放。该机制沿用 V6，D1/D2/D3 的验证位置为 `src/cache/sync_state/delta/tests.rs`、`src/mail/sync/qresync/tests.rs`、`src/server/output_tests/qresync_acceptance.rs` 和 `tests/sync_qresync_budget.rs`。

## Local operation store

发送操作数据库由 `journal` 独立管理。下面的 SQLite/WAL/FULL 契约专用于发送意图和结果；持久邮件元数据采用前述独立 cache 分区。

SQLite 仅保存主体、发送键、参数摘要、Message-ID、时间戳和结果，不保存邮件正文或收件人列表。WAL 允许读写并行，FULL 同步使发送意图在网络提交前落盘；事务保持短小且不跨网络调用。数据库位于本地文件系统，只由一个服务进程拥有，避免另一个进程在启动恢复时改写正在发送的记录。

备份使用 SQLite backup API 或停止服务后复制完整数据目录；恢复、数据库格式变更及回滚遵循 [maintenance](maintenance.md) 与 [deployment](deployment.md)。压测工作负载、环境和测量结果统一保存在 [validation](validation.md)，一次测量不构成性能承诺。

## Logs and failure handling

JSON 日志使用固定事件名和受控字段，记录服务端生成的 request ID、白名单工具名、耗时、HTTP 状态、稳定错误码、发送结果及服务版本。凭据、token、authorization code、邮件主题、地址、正文、附件和底层库错误原文不得进入日志。库协议日志使用固定过滤器关闭，panic hook 只发固定事件，不格式化 panic payload。部署示例的容器日志按 5 MiB、两份轮换。

工具错误使用结构化内容，批量读取保留逐项结果；发送通过持久化状态和 `get_operation` 恢复判断。`/healthz` 只说明进程/配置健康，不能充当邮箱连通性或真实投递成功证明。邮箱探测使用隔离身份和只读操作，真实写入测试另行取得明确授权。

## Invariants and verification

当前验证采用精简默认测试与发布恢复烟测。2026-11-06 以 `42873c0` 为基线削减测试规模，统计入口为 `uv run scripts/test-inventory.py`。代表性回归直接调用生产实现，HTTP 场景共用 `server::http_router`，同时验证签名身份、Schema 和实际邮件后端行为。

| 原则 | 当前验证入口 |
| --- | --- |
| D1 身份与授权 | `auth::tests::jwt_trust_boundaries`、`failed_refresh_is_shared_and_expired_keys_fail_closed`；`server::output_tests` 的 HTTP 请求鉴权、账户隔离及附件主体检查 |
| D2 读取与引用 | `mail::tests` 的目标 UID 属性匹配、状态和摘要的 FLAGS 合并、rev1 文件夹往返；`tests/imap_completion.rs` 的完成状态；HTTP 同 UID 双账户与过期 UIDVALIDITY |
| D3 资源有界 | 六个 `tests/*.rs` 协议集成入口；MIME 头部/解码预算、HTML 分词前属性预算；异步 SMTP 分片响应在累积前的限额；`server::tests` 的完整 wire 预算与正文前缀；暂存取消与调度配额 |
| D4 发送与幂等 | `journal::tests` 的重启/主体隔离与并发 claim；`mail::tests` 的 SMTP 已知结果持久化、MAIL 前能力拒绝和 DATA 确认丢失后的去重；Unix 子进程的 SIGTERM 和已接纳写入排空；`smtp::tests` 的 DATA 格式、TLS/AUTH 与响应超限阶段结果 |
| D5 隐私与内容 | `mail::tests::draft_envelope_and_privacy_filter_share_canonical_fields`；MIME 未知编码与完整 alternative 选择；附件精确字节、授权 GET 及私有暂存 |
| D6 维护与恢复 | `src/accounts`、`src/cache` 的注册库/世代/迁移/回放回归；CI fmt/test/clippy、依赖审计和容器 release 构建；`scripts/check-cache-recovery.py` 的实际候选二进制恢复检查 |

测试精简移除了大量故障排列、细粒度边界组合、独立客户端原型和 R13 持续负载设施。开发者针对新缺陷增加最小回归，并同步替换重叠用例，保持总体量预算。历史长跑和完整矩阵记录位于 [validation](validation.md)，下方设计记录中的旧测试名称随对应历史工作包保留。

Vendored 包的协议回归集中在根包 `tests/`。`tests/imap_completion.rs` 通过原始 CAPABILITY 响应核对 MOVE、UIDPLUS 等扩展能力的 ASCII 大小写匹配，覆盖写入能力检查和诊断共用的查询接口。各上游身份继续由 `scripts/audit-dependencies.sh` 检查，Docker 上下文由 `scripts/test-docker-context.sh` 以合成哨兵和实际构建器核验。

## Decision records

R06 parser 基础对应 D2/D3/D5：`tests/body_structure_budget.rs` 在默认根测试中覆盖结构预算、分配记账、正常响应分段、typed 错误及 Client 终止；`scripts/test-audit-dependencies.sh` 核对两个 path 包的上游审计视图与失败清理，`scripts/test-docker-context.sh` 核对两个允许的 vendor 目录。原完成状态、字节预算和邮件回归继续执行。解析器的预算限制与流式附件/PartManifest 功能分工作包交付。

R09 搜索基础的 D2/D3/D5 入口为 `tests/thread_search_budget.rs`：覆盖 199/200/201 个结果、合法前缀分片、显式空 SEARCH 与缺失响应、完整命令字节阈值、密集 FLAGS 的分配预算、通知消费后释放、错 tag/断连/失败闭锁，以及命令前数据隔离和完成后尾部恢复。普通 SEARCH 超过 200 个结果及既有 BODYSTRUCTURE/部件 FETCH 测试继续检查兼容性。

R06 附件映射对应 D2/D3/D5：`mail::parts` 的 mapping/raw 差分与 reader 回归核对原 DFS 索引、每节点头证据、特殊 multipart 和头预算；`server::output_tests::structure` 使用本地 TLS IMAP 与认证 HTTP MCP 核对实际命令、一次新连接回退、世代变化和协议失败。账户测试继续检查同 UID 的引用隔离；`imap_text` 回归覆盖 ASCII/Unicode 搜索及 STATUS 的结构预算错误分类。

R07 附件部件读取对应 D2/D3/D5：`tests/part_fetch_budget.rs` 验证命令期 literal 预算、完整 section 语法和每个合法前缀、quoted 逻辑字节、typed 错误隐私及完成后的默认解析恢复；`mail::parts::transfer_decode` 与原整段解码做跨块差分；`mail::attachments::staging` 验证私密文件、四份额度、跨进程锁、重启清理及真实阻塞任务取消。`server::output_tests::parts` 通过认证 HTTP 核对大邮件中的小附件、64 KiB 边界、末端探针、坏编码、协议失败、权限重查及慢消费清理。`download_begin_timeout_keeps_transfer_permit_until_blocking_io_finishes` 覆盖下载入口的暂存超时与实际资源释放。

R05 的 D1/D3/D4/D5 回归入口：`accounts/catalog/tests` 与 `config/account_config/tests` 覆盖显式 ACL、默认账户、世代、秘密引用、v1/v2 迁移、发布失败恢复和原 Journal 字节保留；`mail/scheduler/tests` 覆盖全局额度、公平借槽、取消和超时。`server/output_tests/accounts` 通过真实 HTTP/JWT 与双模拟邮箱核对同 UID 隔离、按主体发现、默认读取与旧写目标、历史发送查询、附件撤权、账户独立退避及完整响应预算。`cancelled_http_reads_release_admission_backend_slots_and_connections` 验证 4 个运行/16 个排队读取的取消释放，`in_flight_attachment_gets_stop_on_disconnect_revocation_and_account_removal` 验证下载断连及账户取消释放。

R01 的字符与错误回归入口：`read_lengths_count_unicode_code_points_and_publish_byte_limits`、`read_structure_errors_are_bounded_private_and_point_to_array_items`、`read_mailbox_unicode_and_wire_budgets_keep_legacy_writes_frozen`、`read_search_transmits_full_unicode_literal_at_character_boundary`、`legacy_write_argument_schemas_and_byte_validation_stay_frozen`、`legacy_write_mailbox_inputs_and_configured_directories_stay_at_512_bytes`、`recorded_send_status_remains_available_after_legacy_directory_change`、`input_detail_budget_preserves_original_error_and_measures_real_wire`、`budget_errors_identify_header_structure_attachment_and_body_resources`。分别覆盖 D2/D3/D4/D5 的输入、协议、幂等与错误预算边界。

R04 的 D1/D2/D3/D5 回归入口为 `opaque_ids_bind_every_reference_dimension_and_preserve_all_mime_indices`、`attachment_fetch_reports_absence_only_after_successful_tagged_completion`、`http_failure_classes_are_small_private_and_release_transfer_permits`、`four_slow_consumers_cancel_and_timeout_without_losing_capacity`、`expired_wrong_principal_or_account_handles_return_private_reprepare_errors`，以及真实 JWT MCP/GET 的 `server::output_tests::attachments`。它们核对稳定引用、认证隐私、命令完成、失败分类、许可释放、8 组文件完整字节和输出 Schema。跨运行时的 Node 适配验收作为显式 ignored 测试执行，默认 Cargo 测试仅需 Rust。

R02/R03 的新增验证入口：`accounts/tests.rs` 覆盖 D1/D4 的身份绑定、并发初始化、损坏与恢复；`every_read_output_schema_validates_real_handler_success_and_empty_results` 和其余 `server/output_tests` 覆盖 D3/D5 的正常、局部失败、诊断超时和完整 wire 压缩结果；`readable_reads_use_one_display_policy_while_forward_keeps_legacy_body`、`readable_html_expansion_uses_display_budget_after_bounded_charset_decode` 及 `html/readable` 回归覆盖 MIME 候选、旧转发、显示膨胀与跨部件预算。

R07 正文的 D2/D3/D5 验证入口为 `server::output_tests::body_parts`：14 组认证 HTTP 场景对照 raw 与 parts 的三种正文工具，覆盖 40 MiB 邮件的小正文、隐式 charset、alternative/related、拆块字符、HTML 共享预算、完整 wire 前缀、重复 UID、UTF-7 和 UIDVALIDITY、局部错误继续，以及 raw/HEADER/部件协议失败停止。`mail::parts::charset_decode` 覆盖编码/BOM/输出边界，`mail::parts::body` 对照传输与字符解码，`mail::parts::mapping::body_selection_tests` 核对完整候选、默认字符集和全树头部预算，`mail::parts::reader::tests` 核对 tagged completion 与预算失败阶段。

R08 的 D2/D3/D5 验证入口为 `server::output_tests::listing`，覆盖默认关闭的预览、完整/部分头分类、严格摘要协议、非首正文部件、FLAGS 更新、HTML 与字符截断、页取样额度、逐项错误和摘要保留、完整 wire 预算及附件筛选游标。`preview_reuses_the_single_validated_attachment_filter_fallback` 核对筛选回退与预览组合只读一次；`replacement_stream_spends_only_the_page_budget_left_by_the_first` 核对替换连接共用传输余额。`mail::classification` 与 `mail::parts::preview` 的合成回归分别覆盖固定头规则、URI 与字段截断，以及网络/本地 raw 共用的前缀解码和字符数限制。

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

### 0.2.1 容量与局部失败

会话完整头预算128 KiB与MIME单头预算一致。候选HEADER命令已完整消费后，局部超限/头解析错误加入skipped_candidates并继续；所选消息在完整FETCH后的内容错误加入message_errors。原始FETCH的LIMIT_EXCEEDED仅由已完成SIZE检查或完整BODY检查产生，可以跳过；传输/协议错误立即终止连接。种子头是关联起点，失败仍中止。完整性字段同时考虑跳过的候选。

默认32 MiB单封、128 MiB累计连接预算、2 MiB完整输出；parser与BudgetStream共用同一累计上限参数。mail默认4槽，20个工具请求准入与10秒等待共同约束排队。二进制下载另有4个传输槽，取邮件共用邮箱槽，读取阶段60秒期限含等待。

新增回归：conversation_large_headers_skip_local_failures_but_stop_protocol_errors；batch_and_conversation_fit_wire_budget_without_losing_item_outcomes；relaxed_inline_attachment_fits_complete_response；relaxed_parallel_attachment_decode_accepts_large_payloads。
