# 邮件读取与附件交付实施方案

方案日期：2026-11-02。源码基线：`0.2.1`，`e58090129eda40a47e593a8b89928291559bf1bd`。状态：实施设计，按工作包逐项交付。

目标是让用户通过邮件助手完成附件分析、完整往来阅读、多个邮箱查询和本地离线检索。顺序为：完善现有读取契约和附件交付，加入多账户与按部件读取，再形成可恢复、可核对的完整本地缓存。

本轮包含附件交付、HTML 展示、中文参数与错误、工具输出契约、跨文件夹会话、正文预览、邮件头分类、多账户、完整本地缓存。草稿创建与编辑、新邮件、回复、转发、附件发送、定时发送及发送后的归档暂缓。既有写工具遵守原权限与行为，本轮新增账户先提供只读能力。

附件功能当前只做离线测试（2026-11-02 用户确认）：使用本地模拟邮箱、合成附件及标准 HTTP MCP 客户端验证下载和内容读取。ChatGPT 实际连接测试放到后续阶段；R00 的接口调研用于保留后续接入方案。

## 当前基础与目标

| 能力 | 当前基础 | 交付结果 |
| --- | --- | --- |
| 附件 | 内联 Base64、同主体 Bearer GET、600 秒句柄 | 元数据准确、失败可恢复、客户端收到文件并可分析；随后按部件读取 |
| HTML | 有界 tokenizer、段落和图片 alt | 链接地址、列表、表格边界和引用层次可读 |
| 输入与错误 | Schema 校验和统一错误码 | Unicode 字符与字节分别约束，错误指出字段、原因与下一步 |
| 工具协作 | 结构化封装、游标及逐项结果 | outputSchema 覆盖实际封装，公开能力与限制，明确调用流程 |
| 会话 | 同文件夹关联图、跨文件夹精确定位 | 单账户内跨指定文件夹读取、去重、保留所有来源并可续查 |
| 列表 | 邮件头摘要 | 可选正文预览、基于邮件头的订阅与自动通知标记 |
| 账户 | 每实例一个邮箱，id 为 default | 一个实例配置多个邮箱，按主体授权和路由 |
| 缓存 | 256 项、16 MiB、60 秒内存头部缓存 | 持久化元数据、原始邮件、正文、附件、索引和同步检查点 |

附件句柄容量的实现值为 64，二进制下载最多 4 路。当前二进制 HTTP 类型统一为 `application/octet-stream`，工具元数据包含邮件声明类型。单封邮件上限为 32 MiB、连接累计读取上限为 128 MiB、完整工具响应上限为 2 MiB。

实现入口：[附件](../src/mail/attachments.rs)、[HTML](../src/html.rs)、[输入与响应](../src/server.rs)、[读取编排](../src/mail.rs)、[会话与头部缓存](../src/mail/insights.rs)、[配置](../src/config.rs)。

## 工作包与交付顺序

每个包完成实现、相关测试和契约文档后交付。大包继续拆成可独立验证的 PR。

| 编号 | 工作包 | 依赖 | 退出条件 |
| --- | --- | --- | --- |
| R00 | 客户端附件交付验证 | 无 | 完成能力矩阵和合成最小探针，确定可行适配路径、必要组件及待实现项 |
| R01 | Unicode 与错误细节 | 无 | 读取工具字符契约一致；字段错误与字节预算边界通过回归 |
| R02 | 输出契约与能力发现 | R01 | 读取结果、错误及降级符合 outputSchema；能力和文件夹用途可直接消费 |
| R03 | HTML 展示增强 | R01 | 链接、列表、表格、引用可读，既有发送正文回归一致 |
| R04 | 附件完整交付 | R00、R01、R02 | 本地模拟邮箱中选择、认证下载、文件内容读取及过期重取通过离线测试 |
| R05 | 多账户读取基础 | R02、R04 | 双账户同文件夹/UID 隔离，旧 default 调用保持行为 |
| R06 | 有界 BODYSTRUCTURE 与附件映射 | R05 | 解析阶段有分配前预算，新旧附件索引和语义一致 |
| R07 | 部件下载与局部正文读取 | R06 | 目标部件读取、流式解码、tagged completion 和受控回退通过验证 |
| R08 | 正文预览与邮件头分类 | R03、R07 | 低成本预览、确定性分类、逐项失败和完整响应预算通过验证 |
| R09 | 跨文件夹会话 | R02、R05、R07 | 往来聚合、来源去重、游标保留关联进度 |
| R10 | 持久化元数据与索引 | R05、R06、R09 | 独立 cache 数据库、迁移、世代失效、账户隔离和恢复 |
| R11 | 正文附件缓存及离线读取 | R07、R10 | 原始邮件及派生内容持久化，配额/驱逐/校验/离线语义完整 |
| R12 | 全量回填与增量同步 | R10、R11 | 任务可恢复，标记与删除收敛，历史覆盖进度可查询 |
| R13 | 整体验收与发布准备 | R04–R12 | 多客户端、双账户、故障恢复和持续负载验收，迁移说明完整 |

第一批 R00–R04 改善单账户阅读和附件分析；第二批 R05–R09 完成多账户及高效读取；第三批 R10–R13 完成本地缓存。R00 遇到客户端集成阻碍时，R01–R03 可继续推进。

## 统一身份与兼容契约

第一批内部类型就保留账户维度，避免后续更换附件和缓存键：

```text
AccountKey = 持久化账户 UUID + 账户配置世代
MessageRef = AccountKey + Unicode 文件夹名 + UIDVALIDITY + UID
AttachmentRef = MessageRef + 稳定附件标识 + MIME 解析版本
```

外部 `account_id` 是管理员配置的稳定别名，单账户兼容值为 `default`。内部 UUID 持久保存，修改显示名保持 UUID；更换实际邮箱或服务端身份创建新世代，旧句柄、游标和缓存随之失效。凭据不进入工具参数或输出。

外部附件优先使用不透明 `attachment_id`，旧 `attachment_index` 继续支持。MIME 部件路径由服务端生成并校验。所有结果提供可直接用于下一调用的账户、文件夹、UID 和 UIDVALIDITY。

- 保留工具名、`data` / `error` / `request_id` 封装、字段含义和内联 Base64 格式。
- 保留旧分页字段。复合范围采用不透明 `cursor`；`before_uid` 只用于原单文件夹调用，冲突的分页参数明确拒绝。
- 读取工具增加可选 `account_id`。省略时解析为配置的默认账户，多账户配置必须指定默认项。
- 原有写工具固定使用旧默认账户和原参数、模式、操作库；新增账户仅开放读取。`get_operation` 保留主体与操作键范围，本轮不迁移发送记录。
- `get_operation` 在请求通过全局 OIDC 验证及主体准入后，仅按原 principal + operation_key 查询，独立于当前邮箱 ACL。撤回邮箱写权限后，原主体仍可核对历史 unknown。若撤销全局主体准入，历史记录由管理员按维护流程核对。legacy default 的 UUID、邮箱绑定与操作库路径固定；把旧 default 指向另一实际邮箱时拒绝启动，另建账户或执行显式迁移。
- outputSchema 覆盖实际成功、部分结果、工具错误及预算降级分支。新增字段、资源链接和错误均计入响应预算。
- 输入限制、Schema 和能力信息共用类型或常量；底层另需线路预算时明确公开单位。

## R00 与 R04 附件交付

### 验证客户端的实际路径

隔离服务提供合成 fixture：含已知合计值的 XLSX、含已知段落的 PDF、UTF-8 CSV、中文/特殊字符名称、同名附件以及超过内联限额的附件。

| 路径 | 验证内容 | 完成依据 |
| --- | --- | --- |
| ChatGPT 远程连接（后续阶段） | 直接下载附件并读取内容 | 后续核对表格合计值、文件和工作表；本阶段不执行 |
| 标准 HTTP MCP 客户端 | 工具结果、Bearer GET、文件落盘 | SHA-256 与 fixture 一致，可由解析器打开 |
| 支持资源的客户端 | resource_link 展示、resources/read 路由与授权 | 明确接收范围、响应大小及二进制交付方式 |

记录客户端名称/版本、连接类型、结果形状、鉴权方式、文件大小/类型、下载和分析结果。以实际接收路径决定适配代码。

MCP 的[资源链接](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)与 OpenAI 的[文件输入和组件文件辅助接口](https://developers.openai.com/plugins/reference#file-apis)分别提供不同能力，R00 逐项核对当前远程连接适用的输出路径。

交付选型顺序：

1. 客户端支持认证文件获取时，保留同主体 Bearer GET，使用其已验证的文件接收接口。
2. 宿主需要文件句柄时，增加客户端或连接器侧适配器：受信任组件持有认证上下文，下载原文件，再交给宿主文件系统或文件注册接口。服务端返回附件描述及交付引用。
3. 后续接入 ChatGPT 时，按实际支持能力选择适配方式；需要手工下载上传时，返回 `client_delivery_required` 和明确下一步。当前先完成标准客户端的本地下载和内容读取测试。

默认不启用匿名链接，令牌不进入模型上下文、URL 或日志。若选定宿主只能读取签名链接，R00 追加独立的限时文件代理设计，明确文件范围、有效期、撤销和日志策略，再纳入实现。分析使用客户端的文件能力，邮件服务保持 Rust 运行时。

R00 的选型记录给出适配组件归属、令牌来源或委托机制、principal/account 绑定、文件接收接口、授权有效期和撤销方式。独立适配器使用自己的已授权会话。R04 本阶段完成标准为：本地模拟邮箱经真实 Rust MCP/认证 GET 下载全部合成附件，字节与 SHA-256 一致，CSV/XLSX/PDF 可正确读取，错误及过期重取通过离线回归。ChatGPT 实际下载和读取测试后移，不作为本阶段退出条件。

### 元数据和失败恢复

`list_attachments` / `prepare_attachment_download` 保留旧字段，增量加入：

| 字段 | 语义 |
| --- | --- |
| attachment_id / attachment_index | 新稳定引用和旧索引 |
| filename / safe_filename | 邮件名称和清理后的落盘建议名称 |
| declared_content_type / detected_content_type | 声明类型；有受控检测时返回检测类型，否则 null |
| encoded_size | 编码后的线路字节数，保持现单位 |
| decoded_size / size_accuracy | 解码实测或 null；exact/estimate/unknown |
| disposition / content_id | 附件/内联语义及 CID |
| downloadable / unavailable_reason | 根据编码支持、预算和引用有效性判定 |
| delivery | 交付方式、认证要求、到期时间、后续动作 |
| checksum | 下载完成后的 SHA-256，准备阶段可为 null |

兼容字段 content_type 保持含义。Base64 推算只标估计；未知传输编码在列表阶段给出不可下载原因。第一批保持 64 句柄、600 秒 TTL、4 路下载，多账户阶段加入公平限制。

GET 成功继续使用 attachment disposition、no-store、nosniff；保持 octet-stream 默认，可靠类型通过工具元数据传递。改变 HTTP Content-Type 时单独补类型白名单和客户端测试。

| 场景 | HTTP 与恢复语义 |
| --- | --- |
| 无有效认证 | 401，更新认证 |
| 句柄未知、过期或主体不匹配 | 404，持有原引用的客户端重新准备；避免暴露其他主体句柄 |
| UIDVALIDITY 改变 | 409，重新列出邮件取得引用 |
| 邮件或附件被移除 | 404，重新搜索或定位 |
| 不支持的传输编码 | 422，固定编码类别和支持路径 |
| 解码附件超限 | 413，限制类型与上限 |
| 下载槽满 | 429 和 Retry-After，只读下载可有限退避重试 |
| 后端失败或超时 | 502/504，固定阶段、request_id 和恢复动作 |

错误使用小型 JSON，保留固定错误码和 request_id。完成解码、校验及 IMAP tagged OK 后才能交付完整文件；消费或取消时释放下载许可，失败清理临时数据。R04 可沿用整封 reader，R07 切换按部件读取。

验收覆盖错误主体、过期、认证失效、UIDVALIDITY 改变、删除、坏编码、超限、慢客户端、取消、并发饱和、中断和中文名称；最终文件可打开、字节和内容正确。

## R01 Unicode 与错误

读取工具的 minLength/maxLength 按 Unicode 码点计数，Rust 使用 `chars().count()`；组合字符按多个码点。UTF-8 字节预算另设并公开，协议编码后的字节数另计。[JSON Schema 字符长度规则](https://json-schema.org/draft/2020-12/json-schema-validation#section-6.3)是契约依据。

查询字段保留 512 字符，将 UTF-8 上限设为 2048 字节，同步修改 `imap_text::uid_field_search` 的 literal 输入检查。文件夹读取入口同步调整 `mail::folder`、`imap_text::mailbox_name` / `display_name`：512 字符、2048 UTF-8 字节、8192 modified UTF-7 线路字节作为起始预算。避免 server 放行后底层仍以旧 512 字节拒绝。HTTP 请求体 256 KiB 与消息/连接/输出预算继续限制总量。

校验返回具体失败项，每次最多 8 项。JSON Pointer 指向字段，输出预期类型/范围、实际长度与稳定原因，不回显查询词或邮件内容。基础错误码保留，新增可选 details：

```json
{
  "error": {
    "code": "INVALID_INPUT",
    "message": "A field exceeds its character limit.",
    "retryable": false,
    "details": {
      "field": "/subject", "reason": "character_limit",
      "limit": 512, "actual": 513, "unit": "unicode_code_points",
      "action": "shorten_field"
    }
  },
  "request_id": "synthetic-request"
}
```

`LIMIT_EXCEEDED.details.limit_kind` 区分 raw_message_bytes、decoded_attachment_bytes、decoded_body_bytes、mime_structure、header_bytes、wire_response_bytes、scan_items、cache_quota。正文按既有展示契约截断时继续返回截断信息；附件必须完整交付。decoded_body_bytes 在 R01 实施时细分，用于准确报告共享正文解码预算。

结构和语义校验使用同一错误模型，错误详情受完整响应预算限制，必要时退回小错误。旧写工具校验及发送行为保持现状，共享校验改造加入冻结回归。

实施时分别定义 ReadInputLimits 与 LegacyWriteInputLimits，并把用途显式传入 folder/mailbox 编码校验；读写工具各自生成 Schema。仅当所有写入口仍经过旧限制时才合并底层扩容，覆盖 create/rename/move/delete、草稿及回复转发取原邮件的路径。原 legacy default 的配置目录限制同样保持。

测试包含 ASCII、汉字、emoji、组合字符、512/513 字符、UTF-8/modified UTF-7 字节、数组字段、未知字段、类型错误、交叉参数冲突和错误预算。

## R02 输出契约与调用指引

对全部读取工具声明 outputSchema，描述真实 `{data, request_id}`、`{error, request_id}` 和预算降级分支。先列出真实返回类型，再写 Schema 与样例；最终 Schema 不能退化为空对象。既有写工具保留原契约。

通用字段按功能落地增量统一：scope（账户/文件夹/日期/UID 范围）、coverage（已检查、是否扫完、截断原因）、continuation（续查参数和到期时间）、errors（条目引用、错误与动作）、freshness（live/cache、核对时间和世代）。旧 next_before_uid、message_errors、skipped_candidates 等保持含义。

`list_accounts` 按主体返回账户、默认账户、实际能力和限制。能力包括 attachment_delivery_modes、part_fetch、cross_mailbox_conversation、cache_modes，按配置及已实现功能返回。

R02 同时建立 default 的持久化内部 UUID 及最小 AccountKey 类型，保存到独立账户注册文件；R05 扩展为账户列表，继续使用同一 default 身份。第一批附件句柄已经携带该键，避免多账户上线时重定义引用。

`list_mailboxes` 保留 attributes 字符串，增加 attribute_flags 数组、delimiter、selectable、special_use、role_source。用途优先服务器标记，其次管理员映射；名称猜测只作为候选。映射属于账户配置。

```text
list_accounts → list_mailboxes → search_emails → get_emails
                                              ├→ read_conversation
                                              └→ list_attachments → prepare_attachment_download → 客户端分析
```

文档覆盖中文查信、空页续查、批读部分失败、跨文件夹会话、同名附件、离线查询。测试直接用真实 handler 结果校验 Schema，包含错误、fallback、长 id、转义内容和全部新增元数据。

## R03 HTML 展示

继续使用 tokenizer，不创建 DOM、不加载资源。读取展示增加带版本的格式化策略：

| 元素 | 展示 |
| --- | --- |
| a[href] | 标签文字后显示地址，相同文字/地址去重 |
| ul/ol/li | 符号或编号，受限层次缩进 |
| table/tr/td/th | 行换行、单元格分隔、单元格内换行规范化 |
| blockquote | 分层引用前缀 |
| pre/code | 保留有意义空白与行界 |
| img | 保留 alt；CID 可关联附件，外部图片不请求 |

地址允许 http/https/mailto；实体解码后检查协议、控制字符和长度，拒绝 javascript/data/file。相对地址保留文字并报告地址省略，不使用邮件 base 元素扩展地址。不自动清理链接参数或访问链接。

展示层起始限制为 16 层缩进、单链接 2048 UTF-8 字节、256 个链接；现有 1 MiB 输入/输出、32768 token 及标记复杂度上限保持。新增转义和地址也计入输出预算；保留 foreign-content 拒绝、相邻属性、未知编码、alternative 回滚回归。区分展示省略和不支持结构。

读取和转发保持同一 MIME 候选选择规则。新增格式通过独立展示选项启用，旧转发继续使用原格式；合成测试比较旧转发内容。本包不调整 plain 占位正文的择优策略。

样例覆盖会议链接、订单表格、多层引用、嵌套列表、空锚点、重复 URL、畸形标签、长链接、实体、Unicode 和多种 MIME 组合。

## R05 多账户读取

配置拆为服务级配置及账户列表。保留旧环境变量作为 legacy default；配置文件与旧变量同时提供时启动检查冲突。凭据通过环境变量名或受限 secret 文件引用，配置展示脱敏。

新增 accounts 模块，负责注册、默认解析、主体授权和服务实例选择；auth 保持 OIDC 身份验证。每次工具调用及附件 GET 重新检查账户授权，先授权后缓存。第一版权限粒度为整个账户，账户内文件夹采用同一授权集合。

每账户独立 IMAP 配置、特殊目录映射、缓存命名空间和健康状态。ACL 由管理员配置，模型工具不修改账户。配置先采用重启生效，重启失效内存句柄和游标；后续热加载另设计在途撤销。

全局保留 4 个执行槽、20 个准入请求、10 秒排队和 60 秒操作期限。起始调度每账户最多 2 个后端操作，后台全局最多占 1 槽；单账户可使用全局空闲容量。所有读取、下载、同步共享全局额度；账户故障退避只影响自身。

跨账户查询先由客户端显式分别调用，结果带 account_id；会话重建限制在单账户内。多账户运行时读取工具按账户检查权限，工具发现只公开该主体可访问的能力；原有写工具只对具备默认账户原权限的主体可见。

迁移：载入旧配置 → 固定 default UUID → 保留旧操作库 → 创建独立账户注册及缓存目录 → 添加第二只读账户 → 核验授权和同 UID 隔离 → 验证省略 account_id 的旧调用。账户移除后拒绝新请求、取消同步、清理缓存及未交付句柄，旧操作记录独立保留。

## R06 与 R07 MIME 部件读取

新增 mail/parts，将 BODYSTRUCTURE 转为有界 PartManifest，描述 section、编码、类型、大小、disposition、文件名、CID 和附件映射。[RFC 3501 FETCH](https://www.rfc-editor.org/rfc/rfc3501#section-6.4.5)提供协议依据，实际 API 以锁定依赖为准。

BODYSTRUCTURE 预算必须进入协议解析阶段，限制深度、节点、字符串及数组分配。若依赖缺少支持，先做 parser 预算子 PR，同步 provenance、审计和 Docker allowlist；不能只对已分配的结构计数。

起始结构预算为深度 32、节点 1024、单字符串 128 KiB、单节点参数对 256、总参数对 8192、结构字符串及容器累计记账 1 MiB；checked arithmetic 在分配和压栈前执行。parser buffer 沿用应用 128 MiB 上限，结构预算另行限制，不能由连接字节数替代。触发时返回 LIMIT_EXCEEDED/mime_structure 并关闭连接；只有在已知整封大小不超过 32 MiB 时，允许新连接尝试一次现有有界 raw reader，原解析失败原因随诊断保留。测试用分配记账和超深/超多节点输入核对拒绝发生在越界分配前。

附件分类沿用现有规则：显式附件、未知 disposition 和文件名判断一致；缺省/空/inline 保持原语义；multipart 附件整体处理；message/rfc822 作为整体，不展开内部附件。旧 attachment_index 按原遍历位置映射，不能把过滤后的列表重新编号。缺字段或与 MIME 头冲突时读取必要 MIME 头，列表/正文元数据/下载共用同一清单。

旧索引算法固定为深度优先：叶子或已判定为附件的子树根进入序列，附件根进入后停止向下遍历，其他 multipart 继续遍历；序列包含非附件叶子，索引取该序列的位置。section 编号、筛选后的附件序号与旧索引分别存储。raw parser 和 PartManifest 对全部合成 MIME 结构做差分；分类无法等价时整封回退，成功建立等价映射前不对外发布新索引。

按 UID FETCH BODY.PEEK 获取编码字节，section 由服务端生成。起始分块 64 KiB，维护跨块 Base64 组、quoted-printable 转义和字符集状态；完整下载与整封 reader 的 SHA-256 对比。

每次 FETCH 匹配 UID、section、起始偏移和必需属性，忽略无关推送，等待匹配 tag 的 OK。NO/BAD、BYE、EOF、缺属性、异常重复片段、范围外数据和提前结束终止读取，未消费连接关闭。

保持连接 128 MiB、解码附件 32 MiB 上限。符合部件预算时可读取超过整封限制的邮件中的小附件。multipart 整体附件保留 MIME 整体语义，不拼凑子部件。

完整文件先写受限临时区，解码完成且 tagged OK 后原子提交或交付；取消、超时及重启清理残留。响应消费结束前持有下载许可。

服务器拒绝结构、可靠映射失败或特殊结构需要整体解析时，在整封预算内回退旧 reader；超限给出具体原因。协议失败后先关闭原连接，不能复用未完成命令的连接回退。

R07 局部正文子包让 `get_email`、`get_emails` 和 `read_conversation` 共用正文读取编排。整个工具调用持有连接和一个固定期限；批量及会话中的最终读取目标每 UID 最多进行一次明确的 raw 策略回退。新连接重新 EXAMINE 并核对原 UIDVALIDITY，成功完成 raw 命令后继续服务后续 UID。候选头扫描不因本策略重连，传输或部件命令失败也不自动重试。

正文候选规则与原 MIME 读取共享：alternative 优先完整纯文本候选，失败候选整体回滚；related 仅在没有 start 时取首子，存在 start 时精确匹配 Content-ID；附件子树保持剪枝。所有选中正文共用编码长度乘四的解码预留，字符集状态跨块保留，UTF-7 走有界 raw 回退。选中正文的字符集或解码失败按该正文返回错误，展示截断和 `max_body_length:0` 仍在完整正文验证后执行。

继续批量读取依赖明确的命令完成状态：当前命令匹配 tagged OK 后发生的本地 MIME、字符集或展示错误可以记录为逐项错误；目标数据缺失、部件不匹配、协议或连接预算错误停止后续读取。批量返回 unprocessed，任何会话部分结果继续列出错误和遗漏。错误码与连接是否可继续使用分别记录。

验收要求常见叶子附件只取得结构、必要 MIME 头和目标部件。覆盖单部件、alternative、related、mixed、multipart 整体附件、message/rfc822、RFC2231 名称、空附件、坏编码、断流、异步 FLAGS、世代变化。

## R08 预览与邮件头分类

list_emails/search_emails 增加默认 false 的 preview。启用时每封最多 200 码点，取样起始预算 8 KiB 编码字节，每页累计最多 128 KiB，达到取样额度后的条目标记 preview_status:budget_skipped，仍返回摘要。全部结果受完整 2 MiB 响应约束。

按正文候选语义选择部件，不固定读 section 1。只解码完整编码前缀，规范化空白，HTML 使用新展示。返回 preview_truncated、preview_status；单项失败保留摘要并记录原因。预览缓存带独立类型，不能作为完整正文。

增加有界读取 List-Id、List-Unsubscribe、List-Unsubscribe-Post、Auto-Submitted、Precedence：newsletter 由列表头或 Precedence:list 触发；automated 由非 no 的 Auto-Submitted 或相应 Precedence 触发。两者可并存；没有信号返回 unclassified。

classification_signals 返回触发字段及规范化类别，附固定规则版本。unsubscribe_available 只表示受支持 URI 存在；本轮不自动访问、退订或改标记。

## R09 跨文件夹会话

扩展 read_conversation，保留原参数，新增 mailboxes 显式列表或 scope:standard。standard 使用已核对的收件箱、已发送及管理员归档映射，最多 20 个文件夹，默认排除垃圾箱/废纸篓/草稿；显式选择可只读纳入。

从种子 Message-ID、References、In-Reply-To 建图，通过 HEADER 搜索缩小范围后精确解析比对，支持父引用和后续回复双向展开。同主题不自动合并。

每页起始预算为全文件夹合计 200 个候选头、20 封结果、256 KiB 正文、60 秒期限。超额游标续查，禁止按文件夹倍增预算。cursor 请求不得更换种子、账户或范围。

游标保存主体、账户世代、每文件夹 UIDVALIDITY/水位、已发现 Message-ID、待展开边、已输出引用和解析版本。第一版服务端有界状态最多 64 项、空闲 TTL 10 分钟、合计 8 MiB；每次成功推进后刷新 TTL，满额返回 complete:false 及缩小范围建议。R10 再持久化；失效明确要求重启查询。

首次请求固定实际文件夹列表和每文件夹扫描上界。关联搜索的最小进度单元为 `(目标 Message-ID, HEADER 字段, 文件夹, UID 区间, 下一位置, finished)`；同一目标在各目录的 Message-ID/References/In-Reply-To 搜索分别推进。每个完整 tagged OK 后才提交该单元进度，新增关联加入有界 frontier。只有全部单元完成、frontier 为空且未解决错误为零时才达到关联完整。

每条 HEADER SEARCH 再限制为最多 200 个 UID 数值的区间，搜索响应及其 parser 分配使用独立 128 KiB 预算；不能先接收全目录结果再截成 200 项。逐窗口验证 tagged OK 后推进，预算失败关闭连接并保留上一检查点。稀疏 UID 范围按数值窗口继续推进，耗尽本次命令/时间预算时返回游标。

按 Message-ID 分组，locations 保留所有来源。只有完成关键头和可用内容指纹核对后才折叠为单一内容；尚未取得足够信息时保留 variants 并标 identity_unverified。相同 ID 的头或内容冲突标 collision；缺合法 ID 按 MessageRef 独立保留。跨页只返回新实体或新增来源/变体更新。

分别报告扫描覆盖、关联图覆盖、正文覆盖。complete 仅在所选范围候选扫描及关联队列完成且没有未解决的文件夹/候选错误时为真；因展示预算省略的正文列入 omitted_refs。出现错误或游标容量不足时保持 complete:false，返回可继续项或缩小范围动作。世代变化使相应游标失效。

测试覆盖跨收件箱/已发送/归档、同主题无关联、多副本、ID 冲突、缺头、循环引用、跨页发现关联、单目录失败、世代变化、主体隔离。

## R10 至 R12 完整本地缓存

### 范围和完成定义

同步管理员选定账户/文件夹的全部历史，不以最近 N 封或 N 天静默截断。预算内原始邮件完整保存，正文、部件、预览和索引可重建；超限邮件保存元数据和明确 omitted 原因。

cache_mode 为 off/metadata/full，新增持久化默认 off；启用 full 要求显式磁盘额度。full 目标为元数据、原始邮件、受支持正文、附件和索引齐备。分别报告每层 present/missing/omitted/complete，附 scope、scan_generation、uid_watermarks、checked_at、pending/failed/quota_blocked。

每个 complete 针对该层、范围和最近扫描水位。完整 raw 已保存但正文不受支持时，报告 raw complete 与 body unsupported；额度不足保留进度和缺口，扩容后继续。

扫描边界固定为 `(账户世代, 文件夹, UIDVALIDITY, high_uid=首次UIDNEXT-1, scan_id)`，每文件夹独立记录。该区间的枚举、存在性/删除核对及本层失败队列收敛后才能 complete；之后新增 UID 进入下一轮，标记另报核对时间。物理字节分别报告 materialized、derivable_from_raw、missing、omitted；raw 可派生附件表示可用，附件 materialization_complete 仍须全部实际解码缓存完成。完整模式提供全量派生任务，R12 验收同时检查原始层和物化层。

### 持久化与恢复

operations.sqlite 保持独立。新增 cache.sqlite 及受限 blob 目录，按账户 UUID 分区，文件名由摘要或随机 id 生成，不接受任意服务器路径。不同账户独立保存内容及索引。

逻辑表：accounts、mailboxes、messages、parts、blobs、sync_jobs、sync_checkpoints、conversation_cursors、schema_version。messages 主键包含账户世代、文件夹、UIDVALIDITY、UID；Message-ID 仅是索引。派生内容记录 MIME/HTML 版本。

原始 RFC822 blob 是重建依据；附件可从完整 raw 按需生成并缓存。整封过大而单部件可读时独立保存附件，raw 标 omitted。流式计算摘要，先写文件、原子 rename，再短事务登记；恢复清理孤立文件并核对引用。SQLite 事务不跨网络等待。

目录采用专用权限，部署存储明确磁盘加密和备份访问策略。日志/指标不含正文、地址、标题或文件名。缓存版本化迁移，失败可重建，发送操作库保留。旧缓存恢复后先核对远端世代及状态。

正常重启后本地校验通过、没有 dirty scope 且账户授权有效时，可立即开放显式 cache 读取，标记 remote_generation_unverified、stale 和最后在线核对时间。备份恢复、未清洁退出或未决写操作涉及的范围先要求在线核对；已知 tombstone、失效世代及撤权继续阻断。第一次在线观察到 UIDVALIDITY 变化后立即封存旧世代。

### 在线与离线

现有默认在线读取核对 UIDVALIDITY、目标存在性；标记/统计来自在线命令，不可变内容可命中缓存以省去重复 FETCH。认证与账户授权始终先于缓存。

新增 read_source:live|cache，默认 live。cache 为显式离线选择，返回来源、最后同步时间、范围覆盖与过期状态；get_email_status 保持在线，离线标记通过明确缓存结果展示。网络故障提示可选 cache，由调用方决定切换。

离线附件准备把 read_source、缓存世代及 blob 引用一并绑定到句柄。GET 仍检查当前主体/账户权限和本地失效状态；显式 cache 路径读取已校验 blob，不临时切回 IMAP。live 句柄继续在线核对目标，断网时由客户端重新选择 cache 并申请新句柄。

已知无效世代、tombstone、撤权账户停止交付。离线期间尚未观察的删除按快照展示最后核对时间；重连优先处理删除与世代。

本地检索使用 search_source:server|cache，默认 server，保持 IMAP 语义。cache 支持中文子串，先验证 SQLite FTS5 trigram 编译与短词行为；不足三字符查询用有界候选扫描。返回匹配依据、索引覆盖、待索引数量及续查信息。只有完整解析的正文用于 full-body 索引，预览和截断展示不冒充全文。

### 回填与同步

初次 EXAMINE 获取 UIDVALIDITY/UIDNEXT 水位，按有界 UID 区间枚举并建立检查点。先头部和部件清单，再获取正文、附件、原始邮件；前台请求内容优先。

每批仅在 tagged OK 后提交进度。重启从提交点继续，主键与摘要处理重复。禁止一次把 SEARCH ALL 全部装入内存。

同步分档：基础 rev1 新 UID 获取、周期标记及存在性扫描为必备；CONDSTORE 用 MODSEQ 获取变化；完整协商与只读 EXAMINE 支持后再启用 QRESYNC/VANISHED。[RFC 7162](https://datatracker.ietf.org/doc/html/rfc7162)提供协议依据。当前依赖可见 select_condstore，实施时核对 EXAMINE 支持，禁止为同步改用可写 SELECT。

删除检测覆盖已缓存 UID 的目标区间，仅成功扫描且确认缺失才写 tombstone。NO/BAD、断流、超时不推进扫描完成标记。移动作为旧位置删除、新位置加入，新 UID 独立存储，Message-ID 用于展示关联。

UIDVALIDITY 改变立即停用旧世代数据、句柄和游标，后台清理重建。目录重命名缺稳定服务端标识时按旧位置失效、新位置同步处理。账户撤权在下一请求生效并取消同步。

所有会改变所选缓存范围的既有写操作增加失效通知，包含标记、移动、删除、目录操作及 save_draft APPEND；草稿 APPEND 在执行前将对应 Drafts scope 标 dirty。服务商在 SMTP 后自动保存副本的可能性由已发送范围的核对处理，不增加归档操作。既有操作结果保持原语义，unknown 将范围保留为待核对，后台仅做只读核验。

缓存进程启动先持久化未清洁运行标记，成功后才开放缓存读取；干净退出且没有未决 dirty scope 时才清除标记。写操作执行前将源/目标范围持久标为 dirty，完成后由只读核对清除。dirty 登记失败时先停用该实例的缓存读取，再按原路径执行用户写操作，保留原发送/修改结果；缓存故障不能把已知结果改成可重试。崩溃或存储故障留下未清洁标记，重启通过在线核对恢复受影响缓存。该一致性适配随 R11 引入，不增加写功能。

### 配额与调度

验收起始配置采用全局 10 GiB、每账户分配可配置、单临时文件最多 32 MiB、后台全局 1 个任务。实际 full 额度按扫描大小配置，10 GiB 是测试起点。数据库、索引、WAL、blob、临时文件及待清理数据都计入预算。

热缓存按 LRU 淘汰可重建副本；full 模式保留已完成原始邮件，空间不足暂停回填。允许驱逐 raw 时立刻标对应 coverage partial 并登记待下载。正在交付的 blob 持有引用租约，消费结束再回收。

后台每完成有界批次释放许可，前台优先；退避时释放槽位。停机停止领任务、取消可取消读取、落盘检查点再退出。慢下载与回填仍共用全局 4 槽和 1 GiB 部署内存目标。

增加只读 get_cache_status 展示范围、进度、额度、更新时间和失败摘要。启动/暂停同步、额度和清理通过管理配置或维护命令完成，模型先只开放状态与读取。

## 模块安排

| 模块 | 计划职责 |
| --- | --- |
| server / error | 工具契约、校验、封装、预算、结构化错误与动作 |
| accounts | 注册、默认账户、主体授权、世代 |
| mail | 读取编排、既有写兼容、全局调度 |
| mail/attachments | 引用、句柄、下载、客户端交付结果 |
| mail/parts | 清单、section、局部 FETCH、解码 |
| mail/insights | 会话、定位、统计、预览分类编排 |
| mime / html | 共享 MIME 语义、展示策略和解析预算 |
| cache | 持久化、blob、索引、检查点、迁移 |
| sync | 只读回填、增量、删除核对、退避与调度 |

模块随工作包引入，不先做全仓重构。JSON 逐步由类型化结构承担；Schema 从类型生成或用契约测试约束。缓存不依赖 MCP 请求对象，协议解析不依赖账户授权实现。

## 验收和发布

每包提交合成回归、旧调用样例及契约文档。行为变更执行 CONTRIBUTING 的 fmt/test/clippy/release build；依赖/容器变化追加审计及上下文/镜像检查。

| 场景 | 通过标准 |
| --- | --- |
| 附件分析 | 文件摘要一致，客户端正确计算 fixture 内容 |
| HTML | 链接、列表、表格可读，资源请求数为零 |
| Unicode | 字符边界符合 Schema，字节超限报对应预算 |
| Schema | 正常、部分失败、错误、fallback 均符合声明 |
| 双账户 | 同名目录/UID/ID 不串数据，越权引用不可访问 |
| 部件 | 字节与整封解析一致，无需获取无关部件 |
| 会话 | 关联、冲突、去重与续查正确 |
| 缓存 | 热读取复用字节；显式离线可读取已缓存文件 |
| 恢复 | 断网、重启、配额后从提交点继续 |
| 变化 | 标记、移动、删除、目录重建、撤权正确失效 |
| 负载 | 双账户前台、后台同步、4 路慢下载无额度突破或长期饥饿 |

负载至少包含 10,000 封合成邮件、多层 MIME、长中文头、边界附件及跨目录副本。记录冷/热 p50/p95、IMAP 往返/字节、CPU、峰值 RSS、磁盘/WAL/临时空间、排队时间；持续至少 30 分钟。目标为服务 RSS 在当前 1 GiB 部署额度内，结果写入 validation。

客户端验证用隔离配置与合成数据；真实邮箱兼容性、身份服务测试及生产切换按部署流程单独执行，OAuth 使用隔离主体和客户端。

兼容新增按维护政策发布补丁版本；改变旧字段或默认语义时递增次版本并迁移。多账户保持旧 default 配置可用；缓存独立格式版本和重建路径。回滚先停同步、保留操作库，再启旧程序与默认配置，新 cache 可保留供恢复或管理重建。

每批更新 README 已实现能力、architecture 生效设计、capabilities 参数、maintenance/deployment 迁移恢复；带提交依据的结果写 validation。本方案状态表补 PR、提交和验收位置。

## 实施状态

| 范围 | 状态 | 下一交付 |
| --- | --- | --- |
| 方案 | 已纳入 `7def28a`，按依赖推进 | 主 agent 统筹；执行使用 sol/high、无完整历史 fork |
| R00 | 已完成选型及隔离探针，提交 `1b7afd3`；8 组 fixture、12 类负例通过 | [能力矩阵与证据](attachment-delivery-probe.md)；真实 ChatGPT 下载和读取测试后移 |
| R01 | 已完成，提交 `7def28a`；121 单元 + 2 集成、fmt/clippy/release 通过，交叉审阅闭环 | R00/R01 的 [PR #63](https://github.com/black-zero358/rust-email-mcp/pull/63) 已合并；[验证记录](validation.md#2026-11-02-r00r01-首批实现) |
| R02 | 已完成，提交 `b60a686`；输出 Schema、default 持久身份、账户能力与文件夹用途 | R02/R03 的 [PR #64](https://github.com/black-zero358/rust-email-mcp/pull/64) 已合并，URI 审阅发现修复于 `928a866`；最终 CI 全部通过；见[验证记录](validation.md#2026-11-02-r00r04-pr-审阅与合并) |
| R03 | 已完成，提交 `b60a686`；独立 HTML 展示策略，交叉审阅与预算修复闭环 | 随 R02 完成集成检查；旧转发与 MIME 候选回归通过 |
| R04 | 本阶段已完成，提交 `60b8cca`；164 单元 + 2 集成、fmt/clippy/release、7 组客户端测试及 8 文件联调通过 | [PR #65](https://github.com/black-zero358/rust-email-mcp/pull/65) 已合并；最终 head `99aa4e4` 增补 URI 回归并通过全部 CI；[离线验证记录](validation.md#2026-11-02-r04-附件离线下载与内容读取)；真实 ChatGPT 测试后移 |
| R05 | 已完成，源码 `ffd1bda`；200 单元 + 2 集成、fmt/clippy/release、8 文件客户端联调通过 | [PR #67](https://github.com/black-zero358/rust-email-mcp/pull/67) 最终 head `9019ef7` 全部 CI 通过，已合并为 `501e539`；[设计与配置](multi-account-design.md)、[离线验证](validation.md#2026-11-02-r05-多账户离线验证) |
| R06 | 已完成，源码 `885a1ce`；234 单元 + 29 集成、fmt/clippy/release、8 文件联调、审计及实际镜像通过 | parser 基础 [PR #68](https://github.com/black-zero358/rust-email-mcp/pull/68) 已合并；清单与回退 [PR #69](https://github.com/black-zero358/rust-email-mcp/pull/69) 最终 head `351353b` 全部 CI 通过，已合并为 `41fb821`；[离线验证](validation.md#2026-11-03-r06-附件清单与受控回退) |
| R07 | 实施中；附件部件下载源码 `d0a1e62`，277 单元 + 34 集成、fmt/clippy/release、8 文件联调、审计及镜像通过 | [PR #70](https://github.com/black-zero358/rust-email-mcp/pull/70) 最终 head `bcb2b45` 全部 CI 通过，已合并为 `7837396`；[离线验证](validation.md#2026-11-03-r07-附件部件下载)；下一批接入局部正文，两批完成后关闭 R07 |
| R08 | 待实施；分类规则已完成独立合成原型 | 预览分类；列表/搜索的 `has_attachment` 筛选改用已验证清单，保持扫描游标和附件判定语义 |
| R09 | 待实施 | 跨目录会话 |
| R10–R13 | 待实施 | 持久化、完整回填、离线及发布验收 |

## 对照实现

codefuturist/email-mcp 基线为 `7d31cd5eac8788bd69237c18e3ee1e1fe2816ab7`：

- [IMAP 服务](https://github.com/codefuturist/email-mcp/blob/7d31cd5eac8788bd69237c18e3ee1e1fe2816ab7/src/services/imap.service.ts)：参考部件读取、预览及服务商适配。
- [同步引擎](https://github.com/codefuturist/email-mcp/blob/7d31cd5eac8788bd69237c18e3ee1e1fe2816ab7/src/cache/sync-engine.ts)：参考同步分档、世代失效及回填组织。

实现复用本项目 Rust 模块和约束；引入外部代码或依赖时核对来源和许可证。
