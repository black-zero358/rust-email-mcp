# 验证记录

本文件保存带版本与环境边界的证据。每次新增记录注明提交/tag、日期与时区、环境、命令、结果、CI 链接及未覆盖项；保留历史结果，不把其测试数量或部署状态当作当前版本的证明。验证要求见 [贡献指南](../CONTRIBUTING.md#验证与完成标准)。

## 2026-11-02 R05 多账户离线验证

实现提交 `c3136348ff75709b1208d1cc40750f2c10b3e2a0`，同步 main 的合并提交 `a0a81aac391a240902881df52109c1ab23b5557a` 与其 tree 一致。最终源码 `ffd1bda62ac80de4bfd13008675334173e2962b6` 仅追加 Clippy 要求的静态错误回退简化。环境为 WSL Ubuntu、Rust 1.99.1、锁定 Cargo.lock 和本地模拟 IMAP/JWT/MCP；附件均为合成文件。

| 检查 | 结果 |
| --- | --- |
| `cargo fmt --check` | 通过 |
| `cargo test --locked` | `a0a81aa`：200 单元、2 集成通过；1 个可选 Node 联调默认忽略；8.21 秒 |
| `cargo clippy --locked --all-targets -- -D warnings` | `ffd1bda` 通过；vendored 上游保留原有 4 条 deprecated 宏提示 |
| `cargo build --locked --release` | `ffd1bda` 通过，21.54 秒 |
| 显式执行 `standalone_node_adapter_consumes_real_rust_mcp_and_authorized_get -- --ignored` | `ffd1bda`：1 项通过，7.12 秒；Node 24.15.0 下载并校验 8 组 fixture |
| 独立审阅 | 配置/注册由协议执行者审阅；协议、授权和 runtime/GET 由配置执行者审阅；主 agent 复核集成。最终源码 head 复核无剩余发现 |

新增验证覆盖严格 JSON 与秘密文件限制、v1/v2 迁移失败恢复、原 Journal 字节保持、同 UID 双账户读取/附件隔离、每次请求 ACL、固定旧写账户、撤权后的历史操作查询、全局 4 槽/20 请求、256 项/16 MiB 缓存、账户独立退避、完整响应预算。真实 HTTP 取消测试分别覆盖 4 个运行加 16 个排队读取，以及附件 GET 断连/撤权/账户移除后的连接与许可释放。

审阅发现并修复了读取任务 detach 导致取消后继续运行，以及附件 GET/连接诊断超时退避记账遗漏；写任务保留原独立生命周期。调度明确采用基础份额优先、同一层级前台优先。全部测试离线完成，真实 ChatGPT 连接与真实邮件测试后移。Linux 文件凭据权限校验已执行，Windows 使用环境变量引用；Windows 专用拒绝文件引用回归待该平台 Rust 环境执行。

[PR #67](https://github.com/black-zero358/rust-email-mcp/pull/67) 的最终 head 为 `9019ef7a2edbcdaded854933b6d0b95fcc37ecf1`，相对最终源码仅追加验证文档。[CI 41426244691](https://github.com/black-zero358/rust-email-mcp/actions/runs/41426244691) 的 check/audit/container 全部通过；合并前复查 GitHub reviews 与 review threads 均为空，独立交叉审阅记录已写入 PR。合并提交为 `501e53989ccab7562d2c1ced28602594f0350b0a`，随后本地 main、开发分支及两者远端引用全部同步到该提交。

## 2026-11-02 R00–R04 PR 审阅与合并

按工作包依赖顺序完成审阅和合并，远端 main 与本地 main 同步到 `8b659baf2551ecfec7060e275c29cd13b0ed7f36`。R05 在开发分支继续推进。

| 范围 | PR 与最终 head | CI | 合并提交 |
| --- | --- | --- | --- |
| R00/R01 | [#63](https://github.com/black-zero358/rust-email-mcp/pull/63)，`d654ace` | [check/audit/container 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/41419415243) | `896126a1d57128d5b87a3cc8353c3e7704524498` |
| R02/R03 | [#64](https://github.com/black-zero358/rust-email-mcp/pull/64)，`928a866` | [check/audit/container 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/41421214346) | `c13a59826af3c392c42f1018b6974a6dc57b5746` |
| R04 | [#65](https://github.com/black-zero358/rust-email-mcp/pull/65)，`99aa4e4` | [check/audit/container 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/41421541914) | `8b659baf2551ecfec7060e275c29cd13b0ed7f36` |

GitHub Codex 自动审阅遇到账户额度限制。此次改由未实现相应模块的子 agent 交叉审阅指定提交，主 agent 复核集成差异，结果逐一记录在 PR。审阅发现 R02 的 `download_url` URI Schema 与直接拼接 Unicode 资源地址冲突；修复 `928a866` 使用 URL 编码，并增加真实 MCP→本地 IMAP 的 Unicode 配置回归。原发现者复核闭环后才合并 #64。#65 整合该修复，生产代码保持原 R04 实现，新增同一回归，最终提交重新通过 CI。合并时三个 PR 均无未解决 review thread。

## 2026-11-02 R04 附件离线下载与内容读取

源码提交 `60b8cca`，本地分支 `codex/read-capabilities-plan`。按用户确认的范围，本阶段使用本地模拟 IMAP、JWT/MCP 服务和合成文件完成离线测试；ChatGPT 实际连接测试放到后续阶段。

实现包含稳定附件引用、原索引兼容、文件描述与安全名称、认证下载错误、完整邮件长度核对、SHA-256、并发许可释放，以及标准客户端的下载和本地保存。两个实现 agent 交叉审阅服务端与邮件层；主 agent 核对完整性、预算和 Git 中的原始文件字节。Base64 空白造成的编码输入预算也纳入可下载方式判定。

| 检查 | 环境与结果 |
| --- | --- |
| `cargo fmt --check` | WSL Ubuntu、固定 Rust 1.99.1，通过 |
| `cargo test --locked` | 164 单元、2 集成通过；1 个 Node 联调测试默认 ignored |
| `cargo clippy --locked --all-targets -- -D warnings` | 通过，vendored async-imap 原有四处 deprecated 宏警告保留 |
| `cargo build --locked --release` | 通过，最终构建 18.94 秒 |
| `pnpm --dir scripts/attachment-client-adapter test` | Node 24.15.0，7 组通过，覆盖 JSON/SSE、32 MiB 边界、摘要、取消、目录失败和离线 UI mock |
| 显式 Rust→Node 联调 | 1 项通过，8 组 fixture 经真实 JWT MCP→TLS IMAP→GET→Node 下载，长度与原 SHA-256 全部一致；[命令与证据](attachment-client-adapter-design.md) |
| 文件内容 | 同组字节由锁定解析器读取：CSV/XLSX 金额 194、XLSX 数量 6、同名 CSV 分别 11/29、PDF 段落 CEDAR-729；见 [R00 记录](attachment-delivery-probe.md) |
| `git diff --check` 与文件检查 | 通过；68 个本地文档链接有效；8 个暂存 Git blob 的原始大小和 SHA-256 全部核对通过 |
| 客户端开发依赖审计 | 新增独立 pnpm 锁文件，以官方 npm registry 审计，无已知漏洞；机器默认镜像缺少审计端点，命令临时指定 registry，未修改全局配置 |

CSV/PDF/XLSX fixture 设置 Git binary 属性，保留 CRLF、PDF 偏移及 512 KiB 边界。R04 未修改 Cargo 依赖、vendored 代码或容器构建上下文；标准客户端使用 Node 内置模块，UI Schema 验证依赖只在开发目录。结果保存为本地提交，后续源码 PR 继续执行 CI。

## 2026-11-02 R02/R03 读取契约与展示

日期与时区：2026-11-02，Asia/Shanghai。源码提交 `b60a686ac379bea4178b7325bd8d6eb0f50aa5d8`。该提交完成 15 个读取工具 outputSchema、default 持久身份与文件夹用途、独立 HTML 可读策略，并同步配置、备份和兼容说明。

三个执行 agent 分别负责服务端契约、账户/邮件接线、HTML/MIME；交叉审阅覆盖彼此模块。审阅修复包括：账户未知 schema 匹配、读取避免先执行旧 HTML 渲染、可读格式膨胀与解码预算分离；补齐诊断失败/超时、集合正文压缩和畸形 HTML 样例。旧写输入规则、发送状态顺序、MIME 候选和转发 legacy 格式继续回归。

| 检查 | 结果 |
| --- | --- |
| `cargo fmt --check` | 通过 |
| `cargo test --locked` | 149 单元测试 + 2 集成测试，零失败 |
| `cargo clippy --locked --all-targets -- -D warnings` | 通过；保留 vendored async-imap 原有四处 deprecated 宏警告 |
| `cargo build --locked --release` | WSL Ubuntu / Rust 1.99.1，通过，22.13 秒 |
| `bash scripts/audit-dependencies.sh` | cargo-audit 0.22.2，340 个锁依赖、1395 条公告，exit 0；覆盖 vendored 上游身份 |
| `bash scripts/test-docker-context.sh` | Docker 实际上下文白名单通过 |
| `docker build --tag rust-email-mcp:r02-r03-local .` | 本机 Docker Desktop Linux 引擎 29.5.3，通过；builder release 编译及首次依赖获取约 5 分钟 |
| `git diff --check` | 通过 |

6 项 `server/output_tests` 使用真实 HTTP MCP handler、合成 TLS IMAP 和 jsonschema 验证器，覆盖全部读取工具、正常/空值/逐项失败、STALE_UID/NOT_FOUND、诊断失败/超时、长 ID、转义正文与完整 wire 压缩。账户测试覆盖 UUID 重用、凭据轮换、文件与内存操作库、绑定变更、并发初始化、权限、损坏/未知格式、残留恢复和路径别名冲突。HTML 回归覆盖链接、列表、表格、引用、预格式、外来内容拒绝、跨部件预算及旧转发内容。

`jsonschema = 0.60.2` 只在开发依赖中，关闭默认特性，锁文件增加 33 包，`cargo tree -e normal` 中无该包。运行时继续使用原 Rust 依赖集合。Docker 本轮实际基础镜像摘要：Rust `sha256:9181b56c2a21ab1d5e73cffe04fc762426d30aa7fac3c91141dd033f49a7fc8b`，Debian `sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3`；本地结果镜像 manifest `sha256:2c75e2e0b370e2a387de8a0a1e405596cc32df5c4dc04c3c352ca65d73c1d105`。构建完成后保留为本地验收镜像；源码尚未推送，GitHub CI 随后续 PR 验收。

## 2026-11-02 R00/R01 首批实现

日期与时区：2026-11-02，Asia/Shanghai。R01 源码提交 `7def28a01494685f6575730270258dc0c3d2c8e3`；R00 探针提交 `1b7afd35afacaff26294684a5c22efa24d23f8b0`。基线为 `e58090129eda40a47e593a8b89928291559bf1bd`，本次结果对应本地分支 `codex/read-capabilities-plan`。

R01 实现读取 Unicode 码点与独立字节预算、有界字段错误、资源预算分类及旧写输入规则隔离。两个执行 agent 分别完成服务端与 IMAP 层，再交叉审阅；主 agent 复核接口、幂等顺序和兼容范围。发送已有记录优先于当前配置校验，缺头终结保留原错误码，共享 MIME 错误增量提供可选 details。

| 检查 | 环境与结果 |
| --- | --- |
| `cargo fmt --check` | WSL Ubuntu、固定 Rust 1.99.1，通过 |
| `cargo test --locked` | 121 单元测试、2 集成测试通过；最终缺头终结调整后，追加 `conversation_large_headers_skip_local_failures_but_stop_protocol_errors` 通过 |
| `cargo clippy --locked --all-targets -- -D warnings` | 通过；vendored async-imap 原有四处 deprecated 宏警告仍由依赖编译报告 |
| `cargo build --locked --release` | WSL Linux 本机 release 构建通过，首次构建 1 分 12 秒 |
| `git diff --check` | 通过 |
| R00 `uv run --locked probe.py selftest` | Windows CPython 3.13.13；8 组 fixture 与 12 类负例通过，完整摘要及运行参数见[探针说明](../scripts/attachment-delivery-probe/README.md) |

WSL 构建使用 Linux 文件系统内的 `CARGO_TARGET_DIR`，源码保留在 Windows 工作区；共享磁盘上的首次构建曾出现 I/O 阻塞，迁移构建缓存后完成检查。Rust 依赖、vendored 源码和 Docker 上下文均未修改；本轮执行上述本地行为检查，GitHub CI 和容器检查留待源码 PR 验收。

R00 的合成服务支持 MCP/Bearer GET、resource_link/resources/read 和内联 Base64。8 个文件下载后逐字节与 SHA-256 一致，CSV/XLSX 合计 194、XLSX 数量 6、同名 CSV 分别 11/29、PDF 提取 CEDAR-729。负例覆盖认证、主体/账户隔离、撤销、句柄过期重取及 RPC id 预算，详细证据见[交付报告](attachment-delivery-probe.md)。Rust 附件 handler fixture 集成和目标 ChatGPT 自动文件消费在 R04 执行；本轮使用合成数据及本地服务。

## 2026-10-29 Issue #24–#34 源码修复索引（Asia/Shanghai）

最终集成源码为 [`e6495729cef5bf2b7303de91e74e19ead3bdcdc8`](https://github.com/black-zero358/rust-email-mcp/commit/e6495729cef5bf2b7303de91e74e19ead3bdcdc8)，源码版本 `0.2.0`。11 项启动时仍存在的问题均已逐项核实、修复，并经独立子代理审阅和最终 PR head 的 `check`、`audit`、`container` 成功后合并；各项根因、回归摘要、最终 head、审阅、CI 与 merge SHA 见 [完成清单](../ISSUE_PROGRESS.md)。2026-10-29 核对 GitHub 开放 issue 列表为空。

| 独立验证记录 | 内容 |
| --- | --- |
| [Issue #28](validation/issue-28.md) | SELECT/EXAMINE 与 STATUS 的匹配 tagged completion |
| [Issue #29](validation/issue-29.md) | 按请求 UID 归并 FETCH 属性及唯一摘要 |
| [Issue #30](validation/issue-30.md) | APPEND continuation 前的非请求通知 |
| [Issue #31](validation/issue-31.md) | 批量读取的本地 MIME 错误隔离 |
| [Issue #32](validation/issue-32.md) | 原始国际化头的 SMTPUTF8 需求与确定拒绝 |
| [Issue #33](validation/issue-33.md) | claim/SMTP 前的 DATA 格式校验 |
| [Issue #34](validation/issue-34.md) | 未知 Content-Transfer-Encoding 的不透明处理 |

#24–#27 的早期带版本验证段保留在本文件下方；不改写其中的提交、环境或测试数量。各独立记录也只证明标注的代码和环境，后续整合结果以完成清单中对应最终 head 的审阅、PR COMMENT 和 CI 为准，不将不同提交的历史测试数量相加为一次最终验收。

`0.2.0` 按 [兼容政策](maintenance.md#版本与兼容性) 记录发送输入收紧：非规范 DATA 格式与无法在预算内确定国际化头需求的非 ASCII 草稿可被明确拒绝。没有配置或数据库迁移；既有发送结果与操作键仍需保留。源码合并和版本号不代表产物已发布或生产已部署；本轮没有真实邮件操作、生产部署、持续负载或 musl 发布产物验收。

## 2026-10-29 disposition 兼容分类（Asia/Shanghai，issue #26）

- 代码提交：`2d279eb948fba69ffaf7ae50e99521cbb8bcdda6`；基线 main `36177076aae667cd72ba3ca81556b94575ab0fc6`，独立分支 `codex/fix-26-disposition-extensions`。
- 整合提交：`5a3940319a05f8d114f523ea5189d1949e274f7a` 合并 main `18b26dd`，保留 #25 完整正文选择及 #26 分类规则；仅文档冲突人工整合，源码自动合并。整合后独立 target 重跑 fmt、81 单元 + 1 集成、Clippy、release 全部通过；以下 79 项记录对应原分类代码提交。
- 环境：Windows 隔离工作树、WSL Ubuntu、固定 Rust/Cargo 1.99.1；独立 `CARGO_TARGET_DIR=/home/oeic_fe/issue-disposition-target`，复制依赖缓存后清除本项目及 vendored async-imap 构建结果，明确重编。分类测试命名执行为 1 项通过；`--lib -- --list` 确认分类测试存在且共 79 项。
- 修复前分类回归在显式扩展类型处失败，修复后通过；公开测试只包含分类头值，覆盖缺省、空/空白、inline 大小写、未知类型、form-data 及 filename/name。依据 [RFC 2183 §2.8–2.9](https://www.rfc-editor.org/rfc/rfc2183#section-2.8) 和锁定 mailparse 0.18.0 源码，区分空 Extension 与显式不支持类型。
- 本地临时扩展既有 `text_attachments_stay_downloadable_but_leave_body_and_forward` 与 `discovery_matches_message_indices_and_multipart_attachment_trees` 各执行 1 项通过，验证分类在读取/转发、附件发现、整体 multipart 下载与索引间一致；测试后恢复原文件。依照 issue #26 和 SECURITY 的公开范围，完整触发邮件未提交，公开仓库保留最小分类回归和既有普通附件集成回归。
- 最终公开源码的 `cargo fmt --check`、`cargo test --locked`（79 单元 + 1 IMAP 完成状态集成测试）、`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --locked --release` 与 `git diff --check` 全部通过；cargo-audit 0.22.2 的专用 `bash scripts/audit-dependencies.sh` 覆盖 307 项依赖并通过。vendored 既有 deprecated 宏警告保留。
- 兼容性：显式但不支持的 disposition 按附件处理，正文排除，仍可发现和下载；缺省/空类型、inline、文件名规则与 multipart 子树边界保持。工具 Schema、配置、依赖、权限和操作库不变，无迁移。最终 PR 的 `check`、`audit`、`container` 须在合并前核对。
- 未使用真实邮箱、未发布产物或部署、未验收持续负载或 musl 目标；本地 release 构建使用 Linux GNU 默认目标。

## 2026-10-29 IMAP parser 分配预算（Asia/Shanghai）

- 修复代码：`b52e7dfa09c745d644b88a441161cb9d52cf7aa7`，基于 `36177076aae667cd72ba3ca81556b94575ab0fc6`；对应 issue #24。
- 环境：Windows 隔离工作树、WSL Ubuntu、固定 Rust 1.99.1；合成小响应、本地 duplex，无真实邮箱、发送或部署。
- `cargo fmt --check`、vendored 两个修改文件的 `rustfmt --edition 2024 --check`、`cargo test --locked`（79 单元、1 集成）、`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --locked --release` 均通过。
- 独立临时 vendor 副本以根 Cargo.lock 为解析种子运行 `cargo test --no-default-features --features runtime-tokio --lib imap_stream::tests`，13 项通过；该临时锁文件另解析了上游测试专用依赖，不改变项目锁文件或运行依赖。根 CI 的锁定测试覆盖应用接线；底层单元测试的容量断言另行本地执行。
- 容量回归直接断言超预算请求及 checked arithmetic 拒绝后容量不增长；覆盖非 4 KiB 整数倍上限、重置剩余数据、已知/未知 incomplete、逐字节分片、包含花括号的合法响应文本和 literal 正文、恰好 1 KiB 合法响应。所有输入均为小型合成数据，没有 OOM 或压力实验。
- `bash scripts/audit-dependencies.sh`（cargo-audit 0.22.2，307 项依赖）和 `bash scripts/test-docker-context.sh` 通过。上游版本/checksum、运行依赖和 Cargo.lock 不变，审计不能替代本地 parser 补丁审阅。保留上游现有 `unsafe_pinned` 弃用警告，本次没有新增警告。
- 原始修复候选的生产 Dockerfile 本地构建 `docker build --tag rust-email-mcp:fix-24 .` 通过（镜像 manifest `sha256:937f082ab849ff030262c9a3aa834ba3187c14fdeb82d807c3a5c37d7b7bcb20`）。随后合入 main 的 issue #25 修复；合入后的本地重测遇到 WSL/Service/WSAETIMEDOUT，不能算作通过，最终候选的 `check`、`audit`、`container` 以 PR #39 检查为合并门禁。
- WSL 恢复后，合入 main `717a302e3ca955a084c3a0cca78d38dcb0fc141a` 的提交 `8b8967b4548e9219e9832ea4595cb0d3808695af` 重新通过 fmt、84 单元+1 集成和 Clippy；最终 release/audit/container 仍按最终提交 CI 核验。
- 兼容性：工具参数、结果、稳定错误码、配置及数据库不变，无迁移。实际 parser 缓冲块上限收敛到已有传输预算；移除不具协议语法状态的 literal 猜测和其 64 KiB 行扫描限制，允许预算内合法响应文本。预算为每个缓冲块上限，增长时新旧块及已交付响应可能同时存活，不构成进程 RSS 上限。
- 未覆盖：真实 IMAP、生产部署、持续负载和 musl 发布产物；本记录不代表这些验收已完成。

## 2026-10-29 get_email 完整响应正文预算（Asia/Shanghai）

- 代码提交：`a97b0f585bef946080743cc7dd8764af2419f8f9`（含 `f8257cda138b9a211ce6e4ecdcb3c610a0ed0fe6`）；基线 main `36177076aae667cd72ba3ca81556b94575ab0fc6`，修复 [#27](https://github.com/black-zero358/rust-email-mcp/issues/27)。
- 环境：Windows 独立工作树、WSL Ubuntu、固定 Rust/Cargo 1.99.1，独立 `CARGO_TARGET_DIR=/home/oeic_fe/issue-output-target`；仅合成数据与本地模拟后端。
- `cargo fmt --check`、`cargo test --locked`（81 项单元测试、1 项 IMAP 集成测试）、`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --locked --release`、`git diff --check` 全部通过。专用 `bash scripts/audit-dependencies.sh` 通过，覆盖 307 项依赖；依赖及 vendored 补丁未变。vendored async-imap 的既有 deprecated macro 警告仍存在。
- 回归先验证 150000 字符合成 MIME 经真实 `parsed_with_options` 产生的 128 KiB 正文完整封装超限，再验证新路径返回前缀与真实 `body_truncated`。另覆盖 ASCII、转义密集正文、多字节字符、最大合法 1024 序列化字节 id、text/structuredContent 相同及下一字符超限；短正文与已有截断标记保持，元数据独立超限仍拒绝。边界回归要求把 false 改为 true 所节省的字节不能造成“未省略字符却标记截断”。
- 依据：[MCP structured content](https://modelcontextprotocol.io/specification/2025-11-25/server/tools#structured-content) 建议同时保留序列化文本副本；[Rust floor_char_boundary](https://doc.rust-lang.org/std/primitive.str.html#method.floor_char_boundary) 保证字符边界，不保证 grapheme cluster 不拆分。
- 兼容性：仅修正 get_email 已承诺的前缀返回；工具参数、字段类型、配置、依赖和操作库不变，无迁移。`body_byte_limit` 保持配置上限含义；实际返回可进一步缩短。非正文元数据不裁剪，其余工具及发送状态恢复路径不变。
- 未进行真实邮箱读取/写入、生产部署、持续负载或 musl 发布验收；生产 Docker 构建由最终 PR `container` 检查补足。合并前需核对最终提交的 `check`、`audit`、`container`，本地记录不代替最终 CI。

## 2026-10-29 完整 alternative 正文选择（Asia/Shanghai，issue #25）

- 代码提交：`287074887dbcc8e970855cf900571432ac22fd6e`；基于 main `36177076aae667cd72ba3ca81556b94575ab0fc6`，独立分支 `codex/fix-25-complete-alternative`。
- 环境：Windows 隔离工作树，WSL Ubuntu、固定 Rust/Cargo 1.99.1，使用现有 Cargo 构建缓存；只有合成 MIME 与本地模拟协议，没有真实邮箱操作或部署。
- 修复前新增读取/转发回归失败：读取只返回 `Regards, billing`，预期为 `Complete fallback: invoice total 123`。修复后完整 plain 回退与无 plain 时的 HTML 主体加签名均完整保留，验证签名前后顺序及构建出的转发正文；MIME 层另覆盖 related 主部件、嵌套 alternative、空 HTML、空 plain、显式/命名 HTML 附件和候选片段回滚。
- `cargo fmt --check`、`cargo test --locked`（80 项单元测试、1 项 IMAP 完成状态集成测试）、`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --locked --release` 全部通过。vendored async-imap 的既有 deprecated 宏警告未修改。
- cargo-audit 0.22.2 的 `bash scripts/audit-dependencies.sh` 通过，覆盖 307 项依赖及 vendored 上游身份。依赖、锁文件、容器上下文没有变化；最终分支的 CI `check`、`audit`、`container` 须在合并前核对，CI 链接以关联 issue #25 的 PR Checks 为准。
- 兼容性：仅修复不完整表示被误选导致主体遗漏的行为；工具 Schema、错误码、配置、权限、依赖和操作库格式保持不变，无迁移要求。候选探测继续不解码，最终叶子共用原有资源预算；不拼接不同 alternatives。
- 边界：未验证真实邮箱兼容性、真实发送、生产部署、持续负载或 musl 发布产物；本地 release 构建是 Linux GNU 默认目标。

## 2026-10-29 工作流兼容整合（Asia/Shanghai）

- 代码提交：`4fa02133974c1fae527c454e4d2e17249aec735f`；基于 main `957c37600fdc286fc366e0d3d9fc3ccd31517576`，选择性整合保留分支并保留当前主线的协议、MIME、发送和输出边界。
- 环境：Windows 工作树，WSL Ubuntu 中 Rust 1.99.1；合成 MIME、本地 IMAP/SMTP 与注入下载后端，无真实邮箱或生产操作。
- 本地 `cargo fmt --check`、`cargo test --locked` 通过，78 项单元测试和 1 项 IMAP 完成状态集成测试；高级字段交集与同步 UTF-8 literal、统一 multipart 附件索引、正文选项共享 HTML/MIME、严格 STATUS、主体绑定/过期/下载容量、批量中断及主线回归均在覆盖范围内。
- `bash scripts/audit-dependencies.sh`（cargo-audit 0.22.2，307 项依赖）和 `bash scripts/test-docker-context.sh` 本地通过。依赖版本与 vendored 补丁均未改变；lockfile 只更新服务版本至 0.1.1。
- 本地 Clippy、release 与生产 Dockerfile 构建仍在运行；最终候选的 `check`、`audit`、`container` 必须在合并前全部成功，以最终 PR 检查及后续结果记录为准。
- 兼容性：29 个唯一工具，旧 `download_attachment` 保留内联 Base64，新工具 `prepare_attachment_download` 提供认证 GET；没有配置或数据库迁移。附件发现与 GET 都读取受限整封邮件，不宣称 BODYSTRUCTURE/单部分读取或超过 8 MiB 邮件支持。
- 边界：未验收真实 IMAP 兼容性、真实批量修改、ChatGPT 认证下载/文件消费、自然刷新、持续负载或 musl 发布产物；待验收入口见 CONTRIBUTING，不把本地合成测试当成部署验收。

## 2026-10-28 源码 CI 快照

- 提交：[`c775e922e29ff04a0dfafe3f0e501a8b926b290a`](https://github.com/black-zero358/rust-email-mcp/commit/c775e922e29ff04a0dfafe3f0e501a8b926b290a)。
- 来源：[CI run 40678288110](https://github.com/black-zero358/rust-email-mcp/actions/runs/40678288110)，push 触发，完成时间 2026-10-28 19:16:50（Asia/Shanghai）。本记录来自已完成运行日志的核验。
- 环境：GitHub Actions `ubuntu-latest`，固定 Rust 1.99.1；具体 runner 与构建环境以该运行日志为准。
- `check`：`cargo fmt --check`、`cargo test --locked`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --locked --release` 均通过；测试为 57 项单元测试、1 项集成测试。
- `audit`：cargo-audit 0.22.2，`bash scripts/audit-dependencies.sh` 通过，包含 vendored async-imap 对应上游版本的审计视图。
- `container`：Docker 上下文哨兵检查和生产 Dockerfile 构建通过。
- 边界：这次 CI 不证明真实邮箱写入、生产 OAuth 刷新、持续负载、恢复演练或 musl 发布资产通过验收；本地补丁的代码审阅也不能由依赖审计替代。

## 2026-10-28 初始部署验收历史

以下为原始验收记录。记录当时的 Rust 1.99.1、rmcp 3.8.0、Rauthy 0.37.2、SQLite 3.54.1；未保存确切提交、产物校验值或时区，不能据此推定覆盖后续修复。[v0.1.0 Release](https://github.com/black-zero358/rust-email-mcp/releases/tag/v0.1.0) 也记载了 16 项测试，但不据此反推这些现场测量对应的源码提交。

### 自动化检查

- `cargo test --locked`：16 项通过。
- `cargo fmt --check`、`cargo clippy --locked --all-targets -- -D warnings`：通过。
- Linux amd64 musl 静态 release 构建：通过。
- `cargo audit`：0 项漏洞、0 项警告；JWT 使用 AWS-LC 后端，审计包含测试依赖。
- 本地模拟 SMTP 验证接受后审计失败、响应丢失、重复操作键；合成草稿验证 BCC 和 MIME 附件保留。
- JWT 测试覆盖签名算法、issuer、audience、subject、scope、有效期与 nbf。

历史 `cargo audit` 结果仅保留为当时记录。当前审计入口统一为 `bash scripts/audit-dependencies.sh`，普通 `cargo audit` 会跳过 path 包，不能用于宣称完整覆盖当前 vendored 依赖。

### 隔离部署验收

使用独立测试身份与 OAuth 客户端，完成同用户两次登录、两份刷新令牌独立使用、提前刷新和服务重启后的刷新恢复。QQ 邮箱验证包括工具发现、账户、文件夹、单条列表、正文读取和读前后标记一致。工具数量为 20。

SMTP 故障测试使用本地模拟服务器。真实邮箱发送、移动、删除和文件夹写入由专用测试邮箱另行验收。

### 2026-10-28 ChatGPT 接入排障历史

原始保留分支的现场记录未保存源码映射；不作为当前版本的验收证据。当时已修正回调地址，并为 Rauthy 0.37.2 的公开发现文档补充公共客户端认证方式 `none`。发现文档移入 Nginx 可读取的公共目录，服务私有目录保持原权限。

登录页一次加载约 60 个静态资源，原认证路径限流使部分资源返回 429，前端显示 500。静态资源拆分到独立 location 后，引用的脚本、样式和主题请求均返回 200。

此前的限流事件还触发了 `nginx-limit-req` Fail2ban jail，封禁客户端出口 IP，造成 `ERR_CONNECTION_CLOSED`。核实规则并解除该 IP 的封禁后，客户端 HTTPS 恢复，浏览器实际显示中文登录页。检查保留了认证接口限流和 Fail2ban 防护。

待验收：用户在 ChatGPT 完成完整授权、邮件工具调用，以及访问令牌到期后的自然刷新。排障步骤见[接入排障](troubleshooting.md)。

### 内存与存储

Ubuntu Docker 单次空闲内存快照，在新服务完成登录和只读调用后采集：

| 服务 | Docker 显示内存 |
| --- | ---: |
| Rust 邮件服务 | 1.445 MiB |
| Rauthy | 33.02 MiB |
| 原 FastMCP gateway | 246.4 MiB |
| 原 Node 邮件服务 | 73.86 MiB |
| 原 Dex | 10.55 MiB |
| 原 PostgreSQL | 18.93 MiB |

新组合合计 34.465 MiB，原组合合计 349.74 MiB。原容器已连续运行约 46–47 小时，新容器刚完成验收。登录哈希、并发和大附件的峰值需持续采样。

正式切换并将邮箱授权收敛到 owner 后，邮件容器重建；空闲快照为 mail 1.273 MiB、Rauthy 26.24 MiB，合计 27.513 MiB。原四个容器已停止，数据保留供回滚。

SQLite 使用本地新数据库、WAL、synchronous=FULL；4 个线程执行共 1,000 次 claim → finish → get 循环。总耗时 4.068 秒，中位数 3.719 ms、p95 7.236 ms、最大 2.107 秒。该结果记录当前磁盘和锁竞争条件下的操作记录成本。选用 SQLite 的主要收益是省去常驻数据库服务。

### 当时尚未完成的验收

原开发记录还列有用户侧 ChatGPT 重新连接/自然刷新、专用测试邮箱的授权写入，以及持续负载和大附件内存观察。这里保留其历史未覆盖状态；后续完成时应追加对应版本和证据，不能从源码 CI 成功推定完成。原记录称验收后已移除测试邮箱授权，这不构成任何后续真实邮箱操作的授权。
