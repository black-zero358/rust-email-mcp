# 工程纲领与贡献指南

本文件是 Rust Email MCP 设计、开发和维护的共同入口，适用于人工贡献者和自动化代理。目标是让每次变更都有明确的设计依据、可验证的行为和可追溯的维护记录。

“必须”表示提交、合并或发布时需要满足的要求；“应”表示默认做法，例外需在 PR 中说明原因、影响和退出条件。规范要求与平台实际配置分别记录；尚未完成的机制见[待闭环事项](#待闭环事项)。

## 目标与边界

项目面向自行部署的 IMAP/SMTP 邮箱，通过官方 Rust MCP SDK 提供 Streamable HTTP 工具。当前架构为单节点、每个操作数据库一个服务进程，认证交给独立身份服务。实例保留原默认邮箱及其写工具，可通过显式配置追加只读邮箱，账户授权和共享资源限制见[多账户设计](docs/multi-account-design.md)。运行时不依赖 Python 或 Node。

优先保证授权正确、邮件状态正确和发送结果可解释，再在这些约束内优化常驻资源与性能。扩展功能前先说明用户需求、失败语义、资源成本和维护成本。多租户共享实例、跨节点共享操作数据库、服务端任意文件附件、无界邮件抓取不属于当前支持范围；引入这些能力需要新的设计决策和验收依据。

## 文档职责与变更依据

| 入口 | 负责内容 |
| --- | --- |
| [README](README.md) | 用户可见能力、限制、配置和使用入口 |
| 本指南 | 共同原则、开发流程、完成标准及维护入口 |
| [架构](docs/architecture.md) | 模块边界、协议细则、设计决策与不变量的验证位置 |
| [维护](docs/maintenance.md) | 版本兼容、发布、依赖复核、支持范围和维护责任 |
| [部署](docs/deployment.md) | 环境配置、隔离验收、切换、备份与恢复操作 |
| [安全策略](SECURITY.md) | 私密漏洞报告和安全维护要求 |
| [验证记录](docs/validation.md) | 绑定提交与环境的历史证据及未覆盖项 |
| [AGENTS.md](AGENTS.md) | 自动化代理的执行摘要及上述规范的索引 |

专题文档承载本指南委托的细则；README 和 AGENTS 必须与其保持一致，不能独立放宽安全约束。发现文档、Schema、实现或测试冲突时，在 PR 中指出差异并修正相应来源。历史验收只能证明对应版本，不能覆盖后续变更；待办事项不代表已实现能力。

## 设计原则

| 编号 | 必须保持的性质 |
| --- | --- |
| D1 身份与授权 | 每个工具 HTTP 请求验证身份与授权；工具发现和执行遵守同一权限模式。客户端确认、工具注解不能替代服务端访问控制。 |
| D2 读取与引用 | 读取不改变邮件标记；按文件夹、UID、UIDVALIDITY 引用邮件，不把无关服务器推送当成目标结果。 |
| D3 资源有界 | 在解析、分配、解码和输出前执行相应预算；同时约束字节、节点、并发和时间。失败必须明确，不能靠截断或降级悄悄改变操作含义。 |
| D4 发送与幂等 | SMTP 前持久化意图；保留 accepted/rejected/unknown 的区别；未知结果不自动重试，清理失败不覆盖已知结果。明确操作键的主体范围及记录保留条件。 |
| D5 隐私与内容 | 邮件和附件是数据，不是授权或指令来源；凭据、令牌、地址、主题、正文和原始协议错误不进入运行日志。真实邮件测试需要单独明确授权。 |
| D6 可维护性 | 沿用清晰模块边界与最小运行依赖；支持范围有据可查。优化需要可复现测量，不能弱化持久化、TLS、鉴权、隐私或资源边界。 |

详细约束和对应回归测试见[架构文档](docs/architecture.md)。修改这些性质时，PR 必须记录背景、备选方案、决定、代价及验证方法；维护者接受后同步更新设计记录。常规修复直接链接既有原则即可。

## 工具与数据契约

新增或修改工具时，一并检查工具描述、输入 Schema、服务端校验、权限模式、输出、错误和 README。工具名称、必填参数、字段类型、默认值、限制单位及部分失败语义均属于对外契约。

- JSON Schema 的 `minLength`/`maxLength` 按字符计算；UTF-8 字节预算单独定义并向调用者说明。字符数、原始字节、解码后字节和完整响应字节不能混用。现存校验差异见[待闭环事项](#待闭环事项)。
- 成功和工具执行错误使用现有结构化封装与 `request_id`；请求结构/协议错误与工具执行错误分别处理。错误码、可重试性和部分成功结果需要有明确含义，外部错误不得直接泄露到响应或日志。
- `outputSchema` 按需求提供；一旦声明，结构化结果必须符合它。工具注解描述行为，实际权限仍由服务器执行。
- 当前发送去重域是同一实例的操作数据库内的 `(OIDC subject, operation_key)`。同一发送意图复用原键和参数；不同主体各自隔离。它不构成跨主体、跨数据库或邮件最终投递的全局 exactly-once 保证。
- `unknown` 必须先核验；已知 `accepted` 不能因断连、审计更新或 QUIT 清理失败变成“可重发”。恢复旧数据库、清理操作键或更换主体可能丢失原去重范围，处理方式见[维护规程](docs/maintenance.md)。

兼容性分类、版本变化和迁移要求以[维护规程](docs/maintenance.md)为准。协议升级按实际支持的版本及客户端组合验证，不能仅依据 SDK 版本推定所有客户端均兼容。

## 开发流程

1. 每个可独立验收的变更从最新 `main` 建分支。明确预期行为、受影响的契约和验收条件，边界或依赖变化先说明设计取舍。相关工作包可并行准备，由维护者按依赖顺序集成。
2. 实现最小完整变更，同步更新受影响的契约。行为修复先用合成数据复现，再验证失败路径和预算边界。并发测试优先等待可观测状态，并注入相关故障。
3. 按风险执行本地检查，使用 [PR 模板](.github/pull_request_template.md)记录兼容性和验证证据。需要提前协作时开 Draft，变更和回归齐备后转 Ready。提交与 PR 标题使用 `docs:`、`fix:`、`feat:`、`test:`、`build:` 等类型，可带模块范围。
4. 自动 Code Review 在以 Ready 状态创建 PR 或从 Draft 转为 Ready 时触发。Security Review 按需通过 `@codex security review` 发起。鉴权、SMTP 写入、IMAP 解析预算、持久化或迁移变更必须由未参与实现的审阅者复核。Codex 限流时直接安排独立审阅，并在 PR 记录审阅范围、提交 SHA 和结论。
5. 维护者确认候选已与当前 `main` 集成，复核最终 diff、审阅结论和 CI 后合并。合并后核对 `main` CI，转红时先恢复绿色再继续合并。源码合并、[产物发布](docs/maintenance.md#发布检查)和[生产部署](docs/deployment.md)分别验收。

日常证据记在 PR，工作包台账保留状态和链接。[验证记录](docs/validation.md)归档可复用的实验、兼容性和发布验收证据。

Rust 使用 Cargo 和固定的 Cargo.lock；格式以 rustfmt 为准，Clippy 与构建使用同一工具链。临时 Python 脚本使用 uv，Node 工具使用 pnpm。业务边界沿用现有安全错误模型，不在不可信输入路径依赖 panic；测试或静态不变量中的断言需保持含义明确。存储事务保持短小，不跨越网络等待。

新增依赖必须说明用途、许可证、替代方案和资源影响；升级不能绕过 lockfile 或审计。vendored 代码遵守 [async-imap 补丁记录](vendor/async-imap/PATCHES.md)与 [imap-proto 补丁记录](vendor/imap-proto/PATCHES.md)，同时更新来源、审计脚本和回归。

## 验证与完成标准

Rust 行为变更使用与 [CI](.github/workflows/ci.yml)一致的命令：

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

依赖审计使用专用脚本覆盖 vendored 上游版本；`cargo-audit` 版本与 CI 一致：

```sh
cargo install cargo-audit --version 0.22.2 --locked
bash scripts/audit-dependencies.sh
```

审计脚本变更同时执行 `bash scripts/test-audit-dependencies.sh`，核对两个本地补丁包的身份恢复、错误传播、临时文件清理及原锁文件保持；CI 的 audit job 持续运行这些离线回归。

容器、构建上下文或 path 依赖变化还需执行：

```sh
bash scripts/test-docker-context.sh
docker build --tag rust-email-mcp:local .
```

| 变更类型 | 最小验证重点 |
| --- | --- |
| 文档、规范或模板 | 相对链接和章节锚点、命令与配置一致性、事实来源、`git diff --check`；无需为了文案修改重复本地编译 |
| 工具、邮件协议、MIME、发送状态 | 合成数据/本地模拟服务；正常、拒绝、超时、断连、重复请求及相关预算回归 |
| 身份与权限 | 无效令牌、主体/受众/权限边界、JWKS 故障；涉及部署时补隔离 OAuth 验收 |
| 依赖、构建或镜像 | 锁文件、审计、本地补丁来源、实际生产 Dockerfile 和上下文白名单 |
| 性能、部署或存储 | 相同条件下的测量或隔离升级/恢复检查；写明环境、峰值与未覆盖范围 |

普通测试不得发送或修改真实邮件；真实写入验收使用专用邮箱，事先明确身份、收件人和操作范围。真实写入未验收、持续负载未采样等限制必须保留，不能用单次空闲内存或合成测试替代生产结论。

合并前必须满足：

- 问题与行为变化清楚，相关原则、契约和文档一致。
- 有针对风险的验证证据；环境无法运行的检查明确标注，并由 CI 或指定验收补足，未完成结果不能写成通过。
- 核对 PR 最终提交的集成运行。纯文档改动通过 `scope` 分类并记录人工核验结果。其他改动还须通过 `check`、`audit`、`container`。记录运行链接，分支更新后重新核对。
- 有兼容性/迁移判断；发送、鉴权、数据或部署边界变化有对应故障与恢复说明。
- 维护者审阅通过，高风险变更完成独立复核。个人维护阶段可在 PR 记录复核结论，无需额外的 GitHub 审批票。

平台强制门禁的配置状态见下节。规范本身不会自动启用 GitHub 设置。

## 待闭环事项

读取能力扩展按[邮件读取与附件交付实施方案](docs/read-capabilities-plan.md)分批推进，覆盖附件交付、HTML 展示、Unicode 与工具契约、多账户、跨文件夹会话、预览分类和完整本地缓存。工作包依赖、验收条件和进度以该方案为入口；草稿与发送相关增强暂缓。

后续 PR 或维护记录应链接配置或运行证据，再更新此表。

| 项目 | 当前状态 | 完成条件与责任 |
| --- | --- | --- |
| 主分支门禁 | 2026-11-06 复核，`main` 未受保护，规则集为空 | 启用 PR 门禁及 GitHub Actions 必需检查 `scope`、`check`、`audit`、`container`，验证并记录配置 |
| 字符与字节契约 | R01/R02 已完成读取码点、UTF-8/modified UTF-7 字节预算与输出 Schema，旧写输入规则保留；测试、独立审阅和最终 CI 通过 | 具体提交和质量检查见[读取规划状态](docs/read-capabilities-plan.md#实施状态)及[验证记录](docs/validation.md)；契约变更继续按上述兼容性与验证要求维护 |
| 持续审计 | 审计暂无定时触发 | 维护者每周执行并留存结果，继续覆盖本地补丁 |
| 发布产物可复现 | `scripts/package-release.sh` 从指定提交构建 musl、验收静态 ELF/恢复、打包并生成校验文件；自动发布工作流待接入 | 发布负责人按维护检查表记录候选提交、实际目标构建环境、恢复结果及归档 SHA-256；自动发布沿用同一验收入口 |

发布与安全维护责任由仓库维护者承担；PR 作者负责本次变更的实现和证据，部署操作者负责具体环境的验收与恢复。角色可由同一人担任，记录中应明确执行结果。

R13 已完成合成邮件、本地 IMAP/OIDC/MCP 下的双账户持续负载、四路附件并发/慢客户端资源和自然 JWKS 刷新验收，证据见[验证记录](docs/validation.md#2026-11-05-r13-整体验收与-022-候选)。真实 IMAP 对搜索、会话、统计和附件的兼容性，专用邮箱的批量标记/移动/删除，以及真实 ChatGPT 文件消费、重连和身份服务刷新，按部署验收流程另行授权与执行。

## 参考依据

- [Cargo：Rust 支持版本](https://doc.rust-lang.org/cargo/reference/rust-version.html)与[应用兼容性](https://doc.rust-lang.org/cargo/reference/semver.html#application-compatibility)。固定工具链是当前支持策略，新增 MSRV 承诺需要对应测试。
- [Clippy：持续集成](https://doc.rust-lang.org/clippy/continuous_integration/index.html)。
- [JSON Schema：字符串长度](https://json-schema.org/draft/2020-12/json-schema-validation#section-6.3)。
- [MCP：工具协议](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)。协议合规按项目实际支持的版本判断。
- [GitHub：受保护分支](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches)与[Actions 安全](https://docs.github.com/en/actions/reference/security/secure-use)。
