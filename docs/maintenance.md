# 维护与发布规范

本文件规定版本兼容、发布和持续维护要求，统一入口为 [CONTRIBUTING.md](../CONTRIBUTING.md)。部署操作见 [单节点部署](deployment.md)，安全报告与支持范围见 [SECURITY.md](../SECURITY.md)。已启用的自动化与待办统一见[贡献指南](../CONTRIBUTING.md#待闭环事项)。

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

### 持久化格式与恢复

| 数据 | 当前格式与维护要求 |
| --- | --- |
| 发送操作库 | 保持原格式，按主体和操作键保存发送意图与结果。首次结构变更须补充版本识别、事务迁移、旧数据回归与失败恢复。保留仍需去重的操作键和 `accepted`/`unknown` 结果。 |
| 账户注册库 | 旧环境配置使用 v1，显式多账户使用 v2；迁移保留 default UUID、世代与操作库绑定，先于 Journal 执行。已有 v2 库启动时继续提供账户配置。详见[多账户设计](multi-account-design.md#注册库与启动事务)。 |
| metadata 缓存 | 默认新建 schema V1，兼容 V1–V6；账户分区、路径、application_id 和版本在启动时核验。 |
| full 缓存 | 使用 schema V6；V2 引入文件层与 dirty，V3 引入完整内容和搜索，V4 引入同步检查点，V5 引入派生文件访问顺序，V6 引入扩展同步水位。已发布迁移保持不可变。 |
| 运行见证 | 每次启动均先于 Journal 推进，缓存关闭时也执行。清洁关闭凭据绑定相邻运行及账户/目录范围；恢复备份或跨越旧程序后执行 `--cache-restored`。 |
| 会话游标 | 查询范围全部纳入缓存配置时持久化，否则保存在内存；空闲有效期为 600 秒，有进展的页面提交刷新期限。持久游标保存绝对到期时间，正常重启沿用原期限，恢复缓存时清除旧游标。 |

已有库在短事务中核验和迁移；未知版本、损坏或迁移失败时保留原文件。回退 full 缓存使用旧程序支持的配置和新的私有缓存目录，保留升级后的缓存、最新发送操作库、账户注册库与运行见证。恢复步骤见[缓存备份与恢复](deployment.md#持久缓存备份与恢复)。

早期附件 URL 扩展保留原 `download_attachment` 内联 Base64；二进制通道由 `prepare_attachment_download` 提供，客户端使用同主体 Bearer GET。0.2.0 的原始 MIME 发送输入变化见[DATA 格式兼容性](#020-data-格式兼容性)。

## 发布检查

源码、二进制与生产部署分别验收。生产部署及真实邮件写入按约定范围单独授权。

1. **确定源码。** 选择干净提交，记录完整 commit SHA，确认 `Cargo.toml` 版本与 `v<version>` tag 一致。不覆盖已发布的 tag 或悄悄替换同名产物；修正产物应发布新版本并说明原因。源码部署也必须记录 commit，不能只依赖程序中的 package version。
2. **核对质量证据。** 候选提交的 `check`、`audit`、`container` 必须实际执行并成功。候选只触发了纯文档检查时，在该提交手动运行 CI。记录对应 CI run 和必要的手动验收，验证记录绑定 commit/tag、环境、命令和结果。
3. **构建并核验产物。** 在固定工具链及记录过的 musl 编译环境中安装 `x86_64-unknown-linux-musl` target，执行 `bash scripts/package-release.sh --source-sha <完整提交SHA> --output <新的绝对目录> --schema-version 6`。该[打包脚本](../scripts/package-release.sh)从指定 Git 提交导出源码，在独立 Cargo target 中执行锁定的 musl release 构建，记录编译器/链接器环境、核对 amd64 静态 ELF，并运行[隔离恢复验收](../scripts/check-cache-recovery.py)。预制二进制使用独立的 snapshot 模式，包名带快照标识；正式候选使用提交模式。
4. **归档与说明。** 发布归档包含目标程序、`SOURCE.txt`、README 和许可证，为最终归档生成 SHA-256 校验文件。Release 说明关联 tag、完整 SHA、平台、构建/验证证据、行为变更、已知限制及升级/回滚说明。`SOURCE.txt` 保存源码与构建身份，校验文件用于复核归档字节。
5. **部署验收。** 如另行执行部署，按 [部署指南](deployment.md#验收与回滚) 在隔离环境验收 OAuth、重启恢复、工具发现与相关只读行为，再切换流量。真实邮件发送或修改仍须另行取得明确授权，使用约定的测试邮箱、收件人与操作范围。

涉及认证、发送结果、MIME/附件预算或数据库语义的变更，必须把相应故障回归和迁移检查列入发布记录。性能或内存记录注明负载、环境、采样时长、峰值和持续运行结果。

推送新的 `v<version>` tag 会触发 [Release 工作流](../.github/workflows/release.yml)。先确认同一提交在 `main` 上的最新 CI 运行中，`check`、`audit` 和 `container` 均通过。工作流核对 tag 与 `Cargo.toml` 版本，执行提交模式打包和恢复验收，再将 Linux amd64 musl 归档及 `SHA256SUMS` 上传到对应 Release。新建 Release 会在附件上传完成后发布。维护者复核自动生成的变更说明，并补充该版本的兼容性和升级事项。

若 CI 或附件上传暂时失败，排查后重新运行失败的 Release 工作流。同名附件与本次产物的 SHA-256 一致时直接复用，有冲突时停止发布。修正已发布产物时按新版本发布。

修改发布工作流、打包脚本或恢复烟测的 PR 会在 GitHub runner 上执行打包与恢复验收。发布权限仅授予版本 tag 的发布 job。

## 离线持续验收

当前测试策略采用精简的默认 Rust 回归和发布二进制恢复烟测。运行 `cargo test --locked` 验证生产工具入口、鉴权、协议完成与预算、发送结果和缓存持久化，发布时由 `scripts/package-release.sh` 自动执行 `scripts/check-cache-recovery.py`。

2026-11-06 精简移除了 R13 长跑驱动、独立 Node 客户端原型测试和 Python 文件解析探针。既有 R13 30 分钟负载与附件消费实验见[历史验证](archive/2026-11/validation.md)。后续性能或真实客户端兼容性工作按目标场景单独设计实验。

使用 `uv run scripts/test-inventory.py --revision 42873c0` 与 `uv run scripts/test-inventory.py` 对比测试源码行数、字节数和 Rust 用例数。统计包含独立测试文件、内嵌测试及其辅助模块、vendored 测试、专项脚本、恢复烟测与计数脚本本身。每项均保持在原基线的 20% 以下。

## 持续维护

依赖与补丁的维护入口如下。包版本以 [Cargo.lock](../Cargo.lock) 为准，功能选择见 [Cargo.toml](../Cargo.toml)，代表性回归见[架构验证表](architecture.md#invariants-and-verification)。

| 依赖或设施 | 维护重点与入口 |
| --- | --- |
| vendored async-imap / imap-proto | 完整命令终结、分配前预算、部件 FETCH、rev1 搜索、CONDSTORE/QRESYNC；[async-imap 补丁](../vendor/async-imap/PATCHES.md)、[imap-proto 补丁](../vendor/imap-proto/PATCHES.md)及根 `tests/` 六个协议入口 |
| vendored lettre | 异步 TLS/STARTTLS/AUTH/SMTP 共用的响应预算与结果分类；[lettre 补丁](../vendor/lettre/PATCHES.md)、`src/smtp.rs` 与 `src/mail.rs` 回归 |
| encoding_rs | 跨块字符/BOM 解码、输出边界与 MIME 候选选择；`src/mail/parts/charset_decode.rs` 和正文回归 |
| rusqlite | bundled FTS5、backup 与增量 BLOB；注册库、缓存事务/配额/迁移和游标持久化回归 |
| jsonschema / Tokio test-util | 开发期验证实际工具输出和时间相关行为；运行构建沿用生产功能集 |
| 附件验证 | Rust 服务实际暂存与授权 GET 核对精确字节；夹具见[说明](../tests/fixtures/attachment_delivery/README.md)，旧客户端实验见[归档](archive/README.md) |

`scripts/audit-dependencies.sh` 在临时锁文件中恢复三个 vendored 包的精确上游 registry 身份，再运行 cargo-audit；原 Cargo.lock 保持原字节。版本、校验和、缺失/重复记录、意外 source/checksum 及未登记的 path/git 包均触发拒绝。`scripts/test-audit-dependencies.sh` 验证这些边界和失败清理。Docker builder 与上下文只允许这三个 vendored 目录，`scripts/test-docker-context.sh` 检查允许项与拒绝项。

- 每周运行 `bash scripts/audit-dependencies.sh`，在没有代码变更时也检查新公开的漏洞；遇到相关安全公告时及时复核。审计失败应记录受影响版本、可达性、处置和验证结果，不能为保持绿色而无说明地忽略。
- 至少每月复核 Rust 依赖、工具链、GitHub Actions 和部署镜像的更新；升级通过 PR 处理，保留 Cargo.lock、Actions 完整 SHA 和当前 Rauthy 部署镜像 digest。Dockerfile 的 Rust/Debian 基础镜像按 tag 引用，复核时记录实际镜像身份。
- 修改 vendored `async-imap`、`imap-proto` 或 `lettre` 时遵循各自 PATCHES.md：同步更新来源、版本/checksum、许可证、审计映射和回归。替换为上游版本前验证协议完成、解析预算和错误行为，并独立审阅本地补丁差异。
- 运行故障先使用 request ID、错误码和发送状态定位，维持日志脱敏。认证或邮箱依赖故障按相应状态处置；发送结果不确定时先核查记录，禁止自动重试 `unknown`。重大故障的修复须附合成回归，设计约束有变化时同步更新规范。
- 备份与恢复遵循 [部署指南](deployment.md#备份与恢复)，记录备份时间、对应应用版本和恢复演练结果。应用回滚与操作记录恢复分别决策，不能用旧数据库快照抹去升级后发生的发送。

## 自动化配置核验

1. [CI](../.github/workflows/ci.yml) 覆盖面向 `main` 的 PR 和 `main` 推送。同一 PR 的新运行取消旧运行，合并后的 `main` 检查独立运行。
2. `scope` 只对列入白名单的纯 Markdown 文档跳过 Rust 检查、依赖审计和容器构建。混合改动或无法确认差异时运行完整检查。
3. 启用 `main` 规则集时，要求经 PR 合并，指定 GitHub Actions 的 `scope`、`check`、`audit`、`container` 为必需检查。复核绕过权限，并分别用纯文档和代码 PR 核验门禁。

依赖审计每周运行，定时触发落地前手动留存结果。

实际配置状态及后续项统一记录在[待闭环事项](../CONTRIBUTING.md#待闭环事项)，完成时附配置或运行记录。

## 0.2.0 DATA 格式兼容性

0.2.0 收紧原始 MIME 草稿的 DATA 格式：非规范 LF、原始二进制正文或超长行会以 `UNSUPPORTED_DATA_FORMAT` 拒绝。此规则用于 `send_draft` 的原始 MIME；0.3.0 的纯文本写工具接受 LF/CRLF，并由服务生成规范 MIME。

升级前由客户端将不规范草稿按 MIME 语义修复：规范 CRLF 行，二进制使用 Base64 或 quoted-printable，避免超过 998 原始字节的行。服务不盲目修改正文。支持范围内的 CRLF MIME/编码附件保持原字节，末行无 CRLF 可继续发送。新拒绝发生于 claim 前，不预留操作键；无数据库迁移，已有 accepted/unknown 记录保留。回滚旧程序会重新接受这些非规范格式，不应把该行为当作可靠发送支持；未知发送结果仍先核验，禁止自动重试。

依据：[RFC 5321 §2.3.8](https://www.rfc-editor.org/rfc/rfc5321#section-2.3.8)、[§4.5.2](https://www.rfc-editor.org/rfc/rfc5321#section-4.5.2)、[§4.5.3.1.6](https://www.rfc-editor.org/rfc/rfc5321#section-4.5.3.1.6) 和 [RFC 6152](https://www.rfc-editor.org/rfc/rfc6152)。
0.2.0 同时包含 [issue #32](https://github.com/black-zero358/rust-email-mcp/issues/32) 的 SMTPUTF8 原始头检测和有界检查：非 ASCII 顶层/子 MIME 及 message 嵌套头需要正确的能力要求，超出相应结构/头部预算可在连接前确定拒绝。其失败语义与 DATA 的 claim 前输入拒绝分别核验；实现和历史验证见 #32 修复 PR，后续升级继续核对这些输入约束。
