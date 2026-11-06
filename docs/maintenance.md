# 维护与发布规范

本文件规定版本兼容、发布和持续维护要求，统一入口为 [CONTRIBUTING.md](../CONTRIBUTING.md)。部署操作见 [单节点部署](deployment.md)，安全报告与支持范围见 [SECURITY.md](../SECURITY.md)。规范要求与已经启用的自动化分别记录，不能以文档代替实际门禁。

## 支持边界

- 当前交付范围为 Linux amd64 单节点部署。Docker Compose 使用一个邮件服务进程和独立认证服务；每实例保留一个原默认邮箱，可追加只读邮箱，总计最多 16 个。一个操作数据库及配套账户注册库只由一个服务进程拥有。
- 已发布的独立程序采用 `x86_64-unknown-linux-musl` 目标。每次发布该产物都必须验证目标架构、静态链接和实际运行；默认目标的 CI 构建或 Docker 构建不能替代该验收。其他平台不在当前发布承诺内。
- Rust 仅承诺仓库 [rust-toolchain.toml](../rust-toolchain.toml) 固定的工具链，不另行承诺更早的最低支持版本。升级时同步核对 CI、Dockerfile、README、Cargo.lock 和依赖要求，并执行完整质量检查及目标产物验证。
- 认证服务镜像和部署配置的升级也属于维护变更，必须检查发现文档、客户端配置、登录/刷新、重启恢复及数据兼容性。

## 版本与兼容性

`0.y.z` 阶段采用以下服务版本政策：破坏已支持的对外契约时增加 `y` 并将 `z` 归零；兼容的功能新增和修复增加 `z`。发布说明必须列出兼容性影响、迁移步骤和回滚限制。安全修复若改变已支持行为，也必须评估这些影响；不能只以“安全修复”为由省略迁移说明。

| 契约 | 变更必须检查的内容 |
| --- | --- |
| MCP 工具 | 名称、参数类型/必填项/默认值、返回字段及含义、稳定错误码、`retryable`、发送状态与操作键语义；限制收紧是否拒绝此前支持的调用 |
| 权限与认证 | 默认只读模式、各模式允许的操作、确认要求、scope/audience/subject 边界；不得因升级隐式扩大写权限 |
| 配置 | 环境变量名称、格式、默认值和必需性；旧配置的兼容或明确迁移路径 |
| 持久化数据 | 操作表结构、键和参数摘要语义、已知发送结果、旧数据可读性，以及旧程序能否使用升级后的数据库 |
| 客户端与部署 | 已支持的 OAuth 流程、发现信息、传输方式及已发布目标；具体受影响客户端和部署条件 |

删除或改义应先说明替代方式和迁移窗口。发布前用相关旧参数、旧配置或合成旧数据库验证受影响的兼容路径，并记录结果；无关路径不要求重复全量验收。

### 0.3.0 升级要点

0.3.0 调整单封和批量标记、移动、删除的成功结果契约。旧客户端如果只识别 `updated`，升级前应加入以下分支。六个工具均发布对应的输出 Schema。

| 结果 | 含义与客户端处理 |
| --- | --- |
| `updated` | 收到了目标 UID 的结果证据，例如返回的 FLAGS 符合请求，或 COPYUID 确认了移动。 |
| `accepted` | IMAP 命令返回匹配的 tagged OK，逐 UID 结果仍待核对。没有 COPYUID 的移动和永久删除可以返回此状态。遵循 `retry_allowed:false`，先检查邮箱状态。 |
| 单封 `NOT_FOUND`、批量项 `not_found` | 写入前的 UID 核对确认目标缺失。重复删除沿用此结果。 |

- 列表和搜索摘要新增 `header_complete`。值为 false 时，`subject`、`from` 和 `date` 的 null 表示未知。按 Message-ID 定位支持在限额内读取末尾没有空行的完整纯头邮件。
- `save_draft`、`send_email`、`reply_email` 和 `forward_email` 的顶层 `body` 接受 CRLF 和原有 LF。发送操作仍按原始参数计算指纹。复用既有 `operation_key` 时请沿用原请求的换行写法。
- 在线及离线附件清单为 `INBOX` 的 ASCII 大小写拼写生成同一 `att_v1` ID。升级前按其他 `INBOX` 拼写生成的引用仍可使用，其他文件夹按原拼写匹配。

完整缓存沿用 SQLite schema V6，正文搜索改用投影索引 v2。升级前保存账户注册库、相邻运行见证、最新发送操作库和缓存备份。用 `search_emails` 发起离线正文搜索时，提供 `query` 和 `search_source:"cache"`，省略 `cache_cursor` 和 `before_uid`。服务从已有本地正文记录逐批重建旧索引，每次最多尝试 4 封，按候选正文 blob 大小累计最多 32 MiB。同步暂停时也会推进此步骤。查看 `data.cache.scopes[].coverage.search_index` 的 `indexed_body` 和 `rebuild_pending` 判断本次搜索覆盖及重建进度。升级前取得的 `cache_cursor` 返回 `CURSOR_UNAVAILABLE` 时，省略该游标重新开始查询。

手动运行或自备编排器时，为正常停止预留至少 165 秒。0.3.0 收到停止信号后，在 153 秒上限内排空已经登记的写入及收尾任务，仓库 Compose 设置 165 秒停止宽限。操作步骤见[部署指南](deployment.md#验收与回滚)。

回退前先停止同步并正常关闭。保留升级后的缓存目录，给旧版本配置独立的私有缓存目录，沿用原账户注册库、运行见证及最新发送操作库。恢复缓存备份时，执行[缓存恢复和在线核对](deployment.md#持久缓存备份与恢复)。客户端按实际连接版本解释整理状态。升级验收见[0.3.0 兼容性验证入口](validation.md#030-兼容性验证入口)。

### 0.2.2 升级要点

`0.2.2` 汇总附件下载与内容读取、HTML 展示、Unicode/输出契约、多账户只读、跨目录会话、正文预览和持久离线缓存。新增配置保持显式启用：未配置缓存时为 off，后台同步默认暂停，启用同步后默认 rev1；`sync.protocol:"auto"` 按服务器能力使用 QRESYNC、CONDSTORE 或基础核对。既有默认账户的写工具和发送操作键保持原作用域。

升级前保留账户注册库、相邻运行见证和发送操作库；启用多账户时使用已验证的账户 JSON，启用 full 时配置明确目录与磁盘额度。full 缓存升级至独立 schema V6，metadata 入口兼容 V1–V6。首次同步按冻结范围回填全部历史，使用 `get_cache_status` 分别查看发现、标记/存在性、原件、正文、索引与附件物化进度。完整模式额度不足时保留缺口，扩容后继续；显式离线调用使用 `read_source:"cache"` 或 `search_source:"cache"`。

回退时先停止同步并正常关闭，保留最新操作库、账户注册和升级后的缓存；旧程序使用其支持的配置及新的私有缓存目录。恢复备份或跨越不维护运行见证的旧程序时，执行[缓存恢复流程](deployment.md#持久缓存备份与恢复)，完成对应范围在线核对后开放离线读取。实际静态产物、长跑数据、客户端联调和最终 CI 统一记录在[验证记录](validation.md)。

R09 保留原单目录会话调用和版本 1 账户 JSON。`ARCHIVE_MAILBOX` 与 `folders.archive` 为可选配置，沿用现有数据库格式。回滚到 R09 之前且仍支持当前注册库格式的程序时，使用该程序可解析的账户 JSON，保留当前注册库和最新操作记录。跨目录游标在内存中保存，空闲 10 分钟到期，重启后从种子邮件重新查询。操作步骤见[部署指南](deployment.md#备份与恢复)。

当前操作库没有通用的版本化迁移机制。首次需要改变数据库结构时，必须同时提供格式版本识别、迁移步骤、旧数据回归、失败恢复及回滚边界。迁移和清理不得把 `accepted` 或 `unknown` 变成可重新执行的发送意图。只要仍需要重复发送保护，就必须保留相应操作键和结果。

`0.1.1` 的工作流扩展为兼容新增：旧 `download_attachment` 继续返回内联 Base64，认证 URL 由独立的 `prepare_attachment_download` 提供；无需数据迁移或新增配置。只有支持同主体 Bearer GET 的客户端才能使用新二进制通道。该源码版本号不表示产物已发布或生产已部署。

R10 通过可选 `CACHE_CONFIG_PATH` 新增独立的 metadata 缓存和持久会话游标，默认 off，旧账户与操作库格式保留。缓存按 AccountKey 命名空间分区，每份 SQLite 使用统一的 cache 专用 application_id、schema 版本及各自 namespace 绑定；配置、目录所有权或版本异常使用固定错误。已提交的游标状态保留已知结果，提交后磁盘诊断异常停止后续写入并报告存储故障。正常重启沿用原 token 到期时间，备份恢复使用[专用命令](deployment.md#持久缓存备份与恢复)清除旧游标和标记范围待核对。

metadata 默认新建 R10 schema v1。R11A 的内部 full 入口在短事务中升级到 v2，保留元数据和会话检查点，增加文件引用、原始内容指纹、分层状态与 dirty claim。R11B 的 full 入口继续以独立事务升级到 v3，增加列表观察、角色化 Message-ID 索引、完整正文元数据和分段全文索引；已发布的 v2 迁移内容保持原值。R12A 再以独立事务升级到 v4，增加同步检查点、工作缺口及存在性修订；R12B 以独立 v5 迁移增加派生文件访问顺序。R12C 的独立 v6 迁移增加扩展同步水位、基线及范围修订，v1–v5 的既有迁移内容保持。当前 metadata 入口兼容 v1–v6，旧程序按各自已支持的缓存版本处理。回退旧 full 程序时保留升级后的缓存目录，使用旧程序支持的配置并另设新的私有缓存目录，沿用原账户注册、运行见证和发送操作库；从备份恢复缓存仍执行恢复标记流程。

R11B 每次启动在账户注册库旁推进独立运行见证，off 模式也执行该步骤。见证发布与配置校验先于 Journal，正常关闭凭据绑定前后相邻运行、缓存根和选定账户/目录范围。恢复缓存或跨越旧程序降级时，按[部署规程](deployment.md#持久缓存备份与恢复)执行 `--cache-restored`。发送操作库格式沿用原版本。

新建时在短事务中创建完整 schema，已有库核验版本、账户绑定和 quick_check；未知版本或损坏保留原文件。升级失败保留已提交的旧格式，重试使用相同显式版本入口。后续格式变化递增 schema 版本并提供完整事务迁移与失败恢复回归。缓存可按独立目录重建，发送操作记录保持原恢复流程。启用完整离线内容后，跨越不维护运行见证的旧程序执行回滚与重新升级，须先按[运行代次与恢复设计](cache-content-design.md#写操作与运行代次)失效旧缓存，再进行只读核对。

## 发布检查

发布源码、发布二进制和部署生产是三个独立动作。合入 PR 或创建 Release 不代表批准生产部署或真实邮件写入。

1. **确定源码。** 选择干净提交，记录完整 commit SHA，确认 `Cargo.toml` 版本与 `v<version>` tag 一致。不覆盖已发布的 tag 或悄悄替换同名产物；修正产物应发布新版本并说明原因。源码部署也必须记录 commit，不能只依赖程序中的 package version。
2. **核对质量证据。** 候选提交的 `check`、`audit`、`container` 必须实际执行并成功。候选只触发了纯文档检查时，在该提交手动运行 CI。记录对应 CI run 和必要的手动验收，验证记录绑定 commit/tag、环境、命令和结果。
3. **构建并核验产物。** 在固定工具链及记录过的 musl 编译环境中安装 `x86_64-unknown-linux-musl` target，执行 `bash scripts/package-release.sh --source-sha <完整提交SHA> --output <新的绝对目录> --schema-version 6`。该[打包脚本](../scripts/package-release.sh)从指定 Git 提交导出源码，在独立 Cargo target 中执行锁定的 musl release 构建，记录编译器/链接器环境、核对 amd64 静态 ELF，并运行[隔离恢复验收](../scripts/check-cache-recovery.py)。预制二进制使用独立的 snapshot 模式，包名带快照标识；正式候选使用提交模式。
4. **归档与说明。** 发布归档包含目标程序、README 和许可证，为最终归档生成 SHA-256 校验文件。Release 说明关联 tag、完整 SHA、平台、构建/验证证据、行为变更、已知限制及升级/回滚说明。校验文件只证明内容一致性，不能代替源码和构建证据。
5. **部署验收。** 如另行执行部署，按 [部署指南](deployment.md#验收与回滚) 在隔离环境验收 OAuth、重启恢复、工具发现与相关只读行为，再切换流量。真实邮件发送或修改仍须另行取得明确授权，使用约定的测试邮箱、收件人与操作范围。

涉及认证、发送结果、MIME/附件预算或数据库语义的变更，必须把相应故障回归和迁移检查列入发布记录。性能或内存结论须注明负载、环境和采样条件；单次空闲快照不能代替峰值或持续负载结论。

推送新的 `v<version>` tag 会触发 [Release 工作流](../.github/workflows/release.yml)。先确认同一提交在 `main` 上的最新 CI 运行中，`check`、`audit` 和 `container` 均通过。工作流核对 tag 与 `Cargo.toml` 版本，执行提交模式打包和恢复验收，再将 Linux amd64 musl 归档及 `SHA256SUMS` 上传到对应 Release。新建 Release 会在附件上传完成后发布。维护者复核自动生成的变更说明，并补充该版本的兼容性和升级事项。

若 CI 或附件上传暂时失败，排查后重新运行失败的 Release 工作流。同名附件与本次产物的 SHA-256 一致时直接复用，有冲突时停止发布。修正已发布产物时按新版本发布。

修改发布工作流、打包脚本或恢复烟测的 PR 会在 GitHub runner 上执行打包与恢复验收。发布权限仅授予版本 tag 的发布 job。

## 离线持续验收

当前测试策略采用精简的默认 Rust 回归和发布二进制恢复烟测。运行 `cargo test --locked` 验证生产工具入口、鉴权、协议完成与预算、发送结果和缓存持久化，发布时由 `scripts/package-release.sh` 自动执行 `scripts/check-cache-recovery.py`。

2026-11-06 精简移除了 R13 长跑驱动、独立 Node 客户端原型测试和 Python 文件解析探针。既有 R13 30 分钟负载与附件消费实验保留在 [validation](validation.md) 作为历史记录。后续性能或真实客户端兼容性工作按目标场景单独设计实验。

使用 `uv run scripts/test-inventory.py --revision 42873c0` 与 `uv run scripts/test-inventory.py` 对比测试源码行数、字节数和 Rust 用例数。统计包含独立测试文件、内嵌测试及其辅助模块、vendored 测试、专项脚本、恢复烟测与计数脚本本身。每项均保持在原基线的 20% 以下。

## 持续维护

账户注册库与操作数据库分别存储。纯旧环境配置保留 v1；显式账户 JSON 升级为 v2，沿用原 default 的 UUID、世代和操作库绑定。追加账户使用稳定登记键、世代和删除记录，使身份变化后的旧引用失效。迁移在持久锁内构造并验证完整暂存库，保存 v1-backup 和 previous 副本后原子发布；配置或迁移失败发生在 Journal 恢复之前。多账户运行期持续持锁，v2 启动必须提供账户配置。恢复步骤见[部署指南](deployment.md#备份与恢复)，格式和平台限制见[多账户设计](multi-account-design.md)。

R02 的 `jsonschema = 0.60.2`（MIT）仅作为开发依赖检验真实工具结果，默认特性关闭，使用本地 Schema 定义；产品运行时依赖保持原集合。对应官方 [API](https://docs.rs/jsonschema/0.60.2/jsonschema/) 与[许可证](https://github.com/Stranger6667/jsonschema/blob/master/LICENSE)作为维护入口；锁文件变化继续执行专用审计和容器验证。

R04 的独立 Node/Python 附件原型已归档到 Git 历史；当前下载回归通过 Rust 服务的实际暂存和授权 HTTP GET 路径核对字节。历史工具的依赖与实测结果见 [validation](validation.md)。

R06 将已锁定的 `imap-proto 0.17.0` 纳入本地补丁，以便在 BODYSTRUCTURE 解析及容器扩容前执行预算。版本和依赖解析保持原值；归档 SHA-256 为 `5ccf963d57074747b455398a1763d174da80bcaba6f51e3671a82252b531a68b`，许可证 MIT OR Apache-2.0。来源、API、边界和替换条件见[补丁记录](../vendor/imap-proto/PATCHES.md)。根 Cargo patch 同时覆盖直接依赖和 async-imap 的传递依赖。

R07 局部正文读取将已有传递依赖 `encoding_rs 0.8.42` 声明为直接依赖，复用其跨块字符解码状态；Cargo.lock 的包版本保持不变。许可证为 `(Apache-2.0 OR MIT) AND BSD-3-Clause`。替代方案是收齐整个部件后解码，当前采用增量接口以保留有界块处理。小输出缓冲采用固定 64 字节私有暂存，调用状态相关的 `max_utf8_buffer_length` 核对输入前缀的输出上界，再按 UTF-8 边界交付；升级需重跑拆分 BOM、空输入 EOF、ISO-2022-JP 和原 mailparse 字符结果的差分回归。维护入口为[锁定源码](https://github.com/hsivonen/encoding_rs/blob/v0.8.42/src/lib.rs)；UTF-7 继续使用现有有界 raw 读取路径。

R09 搜索基础继续使用现有两份 vendored 版本、许可证及校验和，增加 opt-in 的命令字节、SEARCH UID 数量及异步通知解析分配预算。升级替换时运行 `tests/thread_search_budget.rs`，核对完整 tagged completion、预读隔离、尾部恢复、通知释放和 typed 错误后的读写闭锁，并重跑原 BODYSTRUCTURE/部件 FETCH 回归。接口及补丁范围分别记录在两库 PATCHES.md。

R12A 延续相同上游身份，新增 opt-in metadata FETCH 的完整命令和 header literal 双预算，保留普通 FETCH/SEARCH 入口。同步只能对合法单条 SEARCH 的 UID 数量溢出缩窗；重复结果或 UID 提前归为协议错误。升级须同时运行同步、既有正文与根协议回归，核对严格 INTERNALDATE、空 FLAGS、header 原点与分配前限额；V4 检查点和持久收据还需迁移/取消/恢复验证。源码 API 与取代条件继续由两库 PATCHES.md 管理。

R12C 在相同 vendored 上游身份上增加 opt-in CONDSTORE 命令 grammar 与终止接口，R12D 扩展独立 QRESYNC grammar；包版本、许可证及锁文件保持。升级须运行 `tests/sync_delta_budget.rs`、`tests/sync_qresync_budget.rs`、命令辅助层、V6 存储和签名 HTTP 同步回归，覆盖分片、完整命令预算、正 63 位水位、只读 EXAMINE、同连接 ENABLE、压缩 VANISHED 范围与顺序、分页/重启/撤权和两连接回退；并重跑普通 FETCH/SEARCH/APPEND 的完成状态与原 BODYSTRUCTURE 预算回归。R12D 沿用 V6，V1–V6 迁移保持不可变；升级失败保留原版本。完整核对还须覆盖已开始 FIFO 写入及核对结束后才返回的旧网络证据，防止已删除 UID 被重新缓存。

R09 签名 HTTP 回归在 dev-dependencies 中为锁定的 Tokio 版本启用 `test-util`，使用虚拟时钟核对 60 秒请求期限与 10 分钟游标有效期。生产构建沿用原有 Tokio 功能集。

R10 为现有锁定的 `rusqlite 0.41.0` 启用 `blob` 功能，使用 SQLite 增量 BLOB 接口有界读写游标，包版本、许可证及 Cargo.lock 保持原值。采用 bundled FTS5 trigram，并以实际锁定库测试中文匹配和有界分页。数据库升级时同时复核 `src/cache` 的物理主库/回滚日志预算、已知提交结果、schema/恢复世代和 `src/mail/conversation` 的 codec/CAS/TTL 回归。

SMTP 响应预算使用本地 lettre 0.11.23 补丁，保留锁定的依赖解析与 MIT 许可证。补丁在异步 SMTP 读取器分配前检查行和响应预算，覆盖隐式 TLS、STARTTLS 和 AUTH 的共用响应路径。同步 SMTP 保留上游实现，服务使用异步连接。归档校验和、回归和替换条件见 [lettre 补丁记录](../vendor/lettre/PATCHES.md)。

专用审计脚本在同一临时锁文件中恢复 async-imap、imap-proto 和 lettre 的精确上游 registry 身份，再运行 cargo-audit；原 Cargo.lock 保持原字节。版本、校验和、缺失/重复记录、意外 source/checksum 及未登记的 path/git 包均触发拒绝。`scripts/test-audit-dependencies.sh` 在 CI 中验证这些边界和失败清理。Docker builder 与上下文仅允许这三个 vendored 目录，context 回归验证三个允许项及其他文件拒绝项。

- 每周运行 `bash scripts/audit-dependencies.sh`，在没有代码变更时也检查新公开的漏洞；遇到相关安全公告时及时复核。审计失败应记录受影响版本、可达性、处置和验证结果，不能为保持绿色而无说明地忽略。
- 至少每月复核 Rust 依赖、工具链、GitHub Actions 和部署镜像的更新；升级通过 PR 处理，保留 Cargo.lock、Actions 完整 SHA 和当前 Rauthy 部署镜像 digest。Dockerfile 的 Rust/Debian 基础镜像目前仍按 tag 引用，复核时记录实际使用的镜像来源；不能据此宣称构建环境已全部不可变固定。固定版本不代表永久不升级。
- 修改 vendored `async-imap`、`imap-proto` 或 `lettre` 时遵循各自 PATCHES.md：同步更新来源、版本/checksum、许可证、审计映射和回归。替换为上游版本前验证协议完成、解析预算和错误行为，并独立审阅本地补丁差异。
- 运行故障先使用 request ID、错误码和发送状态定位，维持日志脱敏。认证或邮箱依赖故障按相应状态处置；发送结果不确定时先核查记录，禁止自动重试 `unknown`。重大故障的修复须附合成回归，设计约束有变化时同步更新规范。
- 备份与恢复遵循 [部署指南](deployment.md#备份与恢复)，记录备份时间、对应应用版本和恢复演练结果。应用回滚与操作记录恢复分别决策，不能用旧数据库快照抹去升级后发生的发送。

## 自动化配置核验

1. [CI](../.github/workflows/ci.yml) 覆盖面向 `main` 的 PR 和 `main` 推送。同一 PR 的新运行取消旧运行，合并后的 `main` 检查独立运行。
2. `scope` 只对列入白名单的纯 Markdown 文档跳过 Rust 检查、依赖审计和容器构建。混合改动或无法确认差异时运行完整检查。
3. 启用 `main` 规则集时，要求经 PR 合并，指定 GitHub Actions 的 `scope`、`check`、`audit`、`container` 为必需检查。复核绕过权限，并分别用纯文档和代码 PR 核验门禁。

依赖审计每周运行，定时触发落地前手动留存结果。

这些要求的实际配置状态及后续项统一记录在 [待闭环事项](../CONTRIBUTING.md#待闭环事项)，不在多份文档重复维护状态。只有取得配置、运行或发布证据后，才能标记为已落实。

## 0.2.0 DATA 格式兼容性

0.2.0 按本规范提升次版本，因为此前显式接受的 LF/原始二进制正文现在可能以 `UNSUPPORTED_DATA_FORMAT` 拒绝。这是有意的契约收紧，不能仅以安全修复省略版本说明。Cargo.toml 与 Cargo.lock 同步，仅源码版本改变，不代表创建 Release、发布产物或部署生产。

升级前由客户端将不规范草稿按 MIME 语义修复：规范 CRLF 行，二进制使用 Base64 或 quoted-printable，避免超过 998 原始字节的行。服务不盲目修改正文。支持范围内的 CRLF MIME/编码附件保持原字节，末行无 CRLF 可继续发送。新拒绝发生于 claim 前，不预留操作键；无数据库迁移，已有 accepted/unknown 记录保留。回滚旧程序会重新接受这些非规范格式，不应把该行为当作可靠发送支持；未知发送结果仍先核验，禁止自动重试。

依据：[RFC 5321 §2.3.8](https://www.rfc-editor.org/rfc/rfc5321#section-2.3.8)、[§4.5.2](https://www.rfc-editor.org/rfc/rfc5321#section-4.5.2)、[§4.5.3.1.6](https://www.rfc-editor.org/rfc/rfc5321#section-4.5.3.1.6) 和 [RFC 6152](https://www.rfc-editor.org/rfc/rfc6152)。
本轮 0.2.0 发送规范还配套 [issue #32](https://github.com/black-zero358/rust-email-mcp/issues/32) 的 SMTPUTF8 原始头检测和有界检查：非 ASCII 顶层/子 MIME 及 message 嵌套头需要正确的能力要求，超出相应结构/头部预算可在连接前确定拒绝。其失败语义与 DATA 的 claim 前输入拒绝分别核验；相关实现和验证以 #32 修复 PR 为准，最终发布需同时包含并验收这些收紧。不能只因字段/Schema 未变而声明完全兼容。
