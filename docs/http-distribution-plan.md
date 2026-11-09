# HTTP 本地分发与部署全流程方案

日期：2026-11-06。状态：待实施设计。代码核对基线：`b0773122855696e7d962939e8ae0a9ab4ca18dd0`。

用户已确定：保持简洁，只支持 HTTP；目标是让别人完成简单安装后，复制一段 MCP 配置即可使用。本文中的 `setup`、`serve --local`、`service`、`client-config`、`doctor`、`token rotate` 为拟新增命令；包名、命名空间和发布版本须在发布准备阶段确定。

## 1. 推荐方案与范围

采用一个 Rust 程序、一个 Streamable HTTP 实现、两种部署配置：

| 配置 | 本地个人使用 | 远程服务器使用 |
| --- | --- | --- |
| 地址 | 默认 `http://127.0.0.1:18590/mcp` | 现有 HTTPS 域名和反向代理 |
| 认证 | 随机生成的本机 Bearer 凭据 | 现有 OAuth/OIDC |
| 启动 | 一次初始化，按需登记用户级后台任务 | 现有 Docker Compose |
| 安装 | PyPI/uv 或 npm/pnpm，任选一个 | Compose 或发布二进制 |
| 客户端 | URL + Authorization 请求头 | URL + OAuth 登录 |
| 默认权限 | `read` | 保持现有配置 |
| 默认缓存 | off；用户显式选择 full 与同步范围 | 保持现有配置 |

两个配置复用账户、IMAP/SMTP、工具、附件、缓存和操作数据库实现。保持现有无参数启动与 `--cache-restored` 兼容；通过显式 `serve --local` 进入本地配置。无需增加 stdio、协议桥接、Web 管理界面或客户端专用插件。

MCP 的 Streamable HTTP 可以运行在本地；规范明确要求校验 Origin，并建议本地仅绑定回环地址。[传输规范](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)

“复制配置”负责客户端连接；安装、邮箱初始化和服务启动由本程序的 CLI 完成。Codex 和 VS Code 官方文档均给出 localhost HTTP 配置。[Codex](https://learn.chatgpt.com/docs/extend/mcp)、[VS Code](https://code.visualstudio.com/docs/agents/reference/mcp-configuration)

## 2. 当前代码与差距

| 已有基础 | 对应实现 | 本次工作 |
| --- | --- | --- |
| Streamable HTTP 与附件路由 | `src/server.rs` | 复用路由，按部署配置选择认证及 Host/Origin 策略 |
| 默认回环监听 | `src/config.rs` | 保留端口；本地配置强制回环，远程配置继续独立校验 |
| OAuth 认证和主体授权 | `src/auth.rs`、`src/config/account_config.rs` | 加入本地凭据验证，复用 `Principal` 和账户 ACL |
| 配置要求 HTTPS resource/issuer | `src/config.rs`、`src/config/account_config.rs` | 将校验按部署类型分支，覆盖两处校验 |
| 当前入口仅支持普通运行/缓存恢复 | `src/main.rs` | 先解析 CLI，再加载相应配置，使 help/version/setup 可独立使用 |
| Windows 文件能力待补齐 | `src/cache/disk.rs`、`src/mail/attachments/staging.rs` | 两处当前显式拒绝非 Unix 存储；实现 Windows ACL、文件身份、原子替换及锁 |
| 运行见证权限校验以 Unix 为主 | `src/accounts/cache_run.rs` | 补齐 Windows 所有者、权限、文件身份校验 |
| 账户注册库依赖目录同步 | `src/accounts.rs`、`src/accounts/catalog.rs` | 核验 Windows 创建、替换及目录持久化路径，覆盖缓存关闭的首次启动 |
| 优雅退出 | `src/server/shutdown.rs` | 保留绝对截止时间；为 Windows 后台进程接入可控退出信号 |
| 提交绑定的 Linux amd64 musl 发布 | `.github/workflows/release.yml`、`scripts/package-release.sh` | 扩展平台、包格式及下载后验收，保留现有归档渠道 |

Windows 工作包含运行能力适配和验收，其中账户注册库创建属于首次启动路径。多平台发行范围按实际通过的系统、架构、最低 OS 版本记录。

## 3. 用户全流程

### 3.1 安装

用户选择一种安装渠道。以下 `X.Y.Z` 是拟发布版本占位符，npm 的 `@scope` 是待登记命名空间。

```sh
# PyPI 渠道：推荐的文档默认入口
uv tool install rust-email-mcp==X.Y.Z

# npm 渠道：已有 Node/pnpm 的用户使用
pnpm add -g @scope/rust-email-mcp@X.Y.Z
```

安装过程只安装程序。首次安装教程同时提供 uv/pnpm 官方安装链接及 PATH 处理说明；启动后端为预编译 Rust 程序，用户无需 Rust 编译器。

后台任务引用持久安装中的原生可执行文件绝对路径，避开 shell PATH 和 npm shim。`uvx`、`pnpm dlx` 用于临时运行 `--help`、`--version` 或前台试用；长期服务使用固定版本安装。uv 的 tool 安装环境与临时执行缓存有明确不同的生命周期。[uv tools](https://docs.astral.sh/uv/concepts/tools/)、[pnpm add](https://pnpm.io/cli/add)

### 3.2 初始化并启动

```sh
rust-email-mcp setup --background --client codex
```

拟定行为按顺序执行：

1. 检查平台、持久安装位置、用户目录和现有配置；已有配置进入维护路径，保留主体、数据库路径和凭据。
2. 收集邮箱地址、IMAP 主机/端口及授权码，秘密输入不回显；提供经校验的常见服务商预设和自定义入口。默认 TLS、`MAIL_MODE=read`。
3. 仅在能力需要时收集 SMTP 设置：保存草稿走 IMAP，发送模式要求 SMTP。为此将当前强制 SMTP 配置调整为按能力校验。
4. 创建独立本地配置、稳定主体、随机访问凭据和私有数据目录，原子提交配置。授权码与 MCP 访问凭据分开保存。
5. 执行一次只读 IMAP 连通性检查。错误时保留可修复配置并给出明确阶段；邮箱临时离线时允许用户选择先完成安装，报告中标明邮箱待连接。
6. 登记当前用户的后台任务，启动服务，等待本地 HTTP 初始化与工具发现成功；不以进程存在作为完成条件。
7. 输出 URL 和所选客户端的配置。包含凭据的配置仅在明确的初始化/导出操作中显示，普通日志、doctor 和 status 隐去秘密。

配置 MCP 客户端后，先调用 `list_accounts`、`list_mailboxes`、一次受限 `search_emails` 与 `get_email`。附件用合成文件或用户选定邮件验证下载；发送和整理能力由用户后续显式开启。

### 3.3 前台运行与日常管理

```sh
rust-email-mcp setup
rust-email-mcp serve --local

rust-email-mcp service status
rust-email-mcp service stop
rust-email-mcp service start
rust-email-mcp service restart
rust-email-mcp doctor
rust-email-mcp client-config --client vscode
```

`setup` 默认只完成配置；`--background` 明确请求登记并启动后台任务。不支持后台管理的环境使用前台模式，远程长期运行沿用 Compose。

`doctor` 区分配置、权限、安装路径、端口、进程、HTTP/MCP、认证、IMAP、缓存覆盖等状态。网络探测通过显式 `doctor --mail` 执行，默认检查本地状态。服务恢复网络后继续接受请求，后台同步按现有退避策略运行。

### 3.4 客户端配置

以下为目标配置，`REPLACE_WITH_LOCAL_TOKEN` 由初始化输出的真实本地令牌替换。

Codex：

```toml
[mcp_servers.email]
url = "http://127.0.0.1:18590/mcp"
http_headers = { Authorization = "Bearer REPLACE_WITH_LOCAL_TOKEN" }
```

VS Code 用户配置 `mcp.json`：

```json
{
  "servers": {
    "email": {
      "type": "http",
      "url": "http://127.0.0.1:18590/mcp",
      "headers": { "Authorization": "Bearer REPLACE_WITH_LOCAL_TOKEN" }
    }
  }
}
```

Claude Code 用户配置格式：

```json
{
  "mcpServers": {
    "email": {
      "type": "http",
      "url": "http://127.0.0.1:18590/mcp",
      "headers": { "Authorization": "Bearer REPLACE_WITH_LOCAL_TOKEN" }
    }
  }
}
```

配置输出适配客户端的字段格式；只生成文本，由用户粘贴到个人配置。含凭据的片段避免保存到共享项目仓库。支持环境变量或安全输入的客户端同时提供对应导出选项，默认不依赖 GUI 进程继承 shell 环境。[Codex 配置](https://learn.chatgpt.com/docs/extend/mcp)、[VS Code 配置](https://code.visualstudio.com/docs/agents/reference/mcp-configuration)、[Claude Code MCP](https://code.claude.com/docs/en/mcp)

本机 URL 面向在同一网络环境运行的 MCP 客户端。WSL、容器、SSH 远程开发各自的回环地址单独处理，首版教程采用同一宿主机原生运行。云端连接使用远程 HTTPS 部署。

## 4. HTTP 与认证设计

### 4.1 本地模式

- 明确选择 `local-token`；绑定地址只允许回环 IP，默认固定为 `127.0.0.1:18590`。IPv6 支持需同时核验 `::1` 的监听和 URL 格式。
- 生成至少 32 字节加密安全随机数作为实例凭据，不复用邮箱密码；凭据比较使用固定长度摘要与常量时间比较。
- 保存随机生成且持久不变的本地主体 ID。令牌轮换只更换凭据，保持主体、账户 UUID、注册库和操作库路径，使原有发送操作键仍可查询。
- HTTP 请求先验证 Host、Origin 和凭据，再访问队列、账户、游标、缓存或邮箱；`/mcp` 与附件 GET 使用相同认证主体。
- 默认拒绝所有带浏览器 Origin 的请求；调试 Inspector 等场景通过精确的 Origin 白名单显式开放。无 Origin 的原生客户端仍要求有效凭据。禁止通配 CORS。
- 本地 Host 白名单按实际监听地址和端口生成，忽略未经信任的转发头。客户端和本地诊断绕过外部代理，测试系统代理开启时的回环访问。
- `/healthz` 保持最小进程健康信息；含配置、账户或版本诊断的详细状态由本地 CLI 或认证路径读取。
- 本地令牌仅用于本实例；URL、附件句柄、日志及进程参数中均不放访问令牌。不将凭据跟随 HTTP 重定向发往其他 origin。
- 首版个人实例的多个客户端共用同一本地主体，提供相同账户权限。若需客户端分别撤权，后续增加同一主体下的多凭据表；此项不阻塞首版。

本地预置 Bearer 凭据属于本项目的显式认证配置，依赖客户端的自定义请求头能力；远程模式采用 MCP OAuth 授权流程。本地模式不返回虚构 issuer，也不触发外部 JWKS 查询。无效凭据返回 401 和明确的 Bearer challenge；工具权限不足返回既有授权错误。兼容性测试覆盖客户端在 401 后的发现行为。[MCP 授权规范](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization)

### 4.2 远程模式

保持现有 HTTPS resource、OIDC issuer/audience/subject/scope 校验、发现文档、逐请求鉴权和附件主体绑定。`local-token` 配置拒绝非回环监听；切换远程模式必须提供完整远程配置。

本地与远程的认证分支最终返回相同 `Principal`，随后进入原账户 ACL 和邮件逻辑。旧环境变量部署、已有多账户 JSON 和默认写邮箱行为继续回归。

### 4.3 端口与实例

一个本地配置目录对应一个服务进程和一个稳定端口。重复启动取得同一实例锁后返回“已运行”，不建立第二个 Journal；其他程序占用端口时提示选择新端口并重新导出配置。禁止杀掉未知进程或静默换端口。

首次安装允许选择空闲端口并持久化；后续启动严格使用已保存端口。修改端口同步更新 resource 与附件下载 URL；已签发短期附件句柄按既有生命周期失效。

## 5. 配置、秘密与数据

本地模式采用一个带 schema 版本的主配置文件，保存地址、认证类型、稳定主体、账户配置引用及绝对数据路径；复用现有账户 JSON 和缓存 JSON，不另造一套邮箱模型。秘密保存在独立受限文件，配置只保存引用。旧环境变量路径完整保留。

本地配置加载顺序为默认值、所选配置文件、明确支持的 CLI 覆盖；避免把 shell 中残留的远程 OIDC 或数据库环境变量隐式混入本地配置。`doctor` 显示各非秘密字段的生效来源。

| 平台 | 配置/秘密 | 持久数据与运行记录 |
| --- | --- | --- |
| Linux | `$XDG_CONFIG_HOME/rust-email-mcp`，缺省 `~/.config/rust-email-mcp` | `$XDG_DATA_HOME/rust-email-mcp`；日志使用 XDG state；锁等瞬态对象按可用的 XDG runtime 管理 |
| macOS | `~/Library/Application Support/rust-email-mcp/config` | 同一应用支持目录下的 data；日志使用 `~/Library/Logs/rust-email-mcp` |
| Windows | `%LOCALAPPDATA%\rust-email-mcp\config` | 同目录下 data、logs、run，使用当前用户 SID 的受限 DACL |

Linux 遵循 XDG 的配置、数据、状态及瞬态文件划分。[XDG 规范](https://specifications.freedesktop.org/basedir/latest/)

操作库、注册库、运行见证和具有恢复语义的邮件缓存都放在持久目录；包升级、uv 缓存清理及卸载不删除这些数据。附件暂存独立放置并从备份中排除。

Unix 目录 0700、秘密和数据库文件 0600；Windows 显式设置并验证 DACL/所有者，拒绝不安全继承和重解析路径。文件锁、打开前后身份、硬链接、原子替换和写入持久化均通过平台实现保持相同语义。[Windows 文件安全](https://learn.microsoft.com/en-us/windows/win32/fileio/file-security-and-access-rights)

首版使用私有文件存储秘密，密钥链集成后移。提供秘密文件权限错误的具体修复指引；主配置、秘密、实例 ID、注册库与操作库一并纳入加密备份。

本地首次安装创建新实例。已有生产实例继续使用原部署；导入已有账户或数据时保留原主体及路径绑定，并另做迁移设计，避免将旧数据库直接复制到新路径后启动。

## 6. 后台运行与退出

后台管理由操作系统负责，程序保持单进程前台服务模型。`service install` 使用当前用户、当前配置的绝对路径以及原生二进制绝对路径；不把邮箱密码写入任务命令行。

| 平台 | 方案 | 关键设置 |
| --- | --- | --- |
| Linux | systemd user service | `Type=simple`，有上限的失败重启，停止宽限覆盖程序退出预算；退出登录后继续运行需显式配置 linger |
| macOS | 用户 LaunchAgent | 用户登录后运行，使用绝对 ProgramArguments，配置节流、日志及退出宽限 |
| Windows | 当前用户登录任务 | `InteractiveToken`、登录触发、单实例、无管理员提权、隐藏启动；取消默认执行时长限制，明确电池和休眠恢复策略 |

这些选型依据系统原生机制；Linux 的 Restart/TimeoutStopSec、macOS LaunchAgent 和 Windows 登录任务各有独立生命周期。[systemd](https://github.com/systemd/systemd/blob/main/man/systemd.service.xml)、[Apple launchd 指南](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)、[Windows 登录类型](https://learn.microsoft.com/en-us/windows/win32/taskschd/principal-logontype)

Windows 使用 `InteractiveToken`，保留网络访问能力；计划任务执行时限设为 `PT0S`。系统默认 72 小时上限会终止长期运行任务，须在登记时显式调整。[任务时限](https://learn.microsoft.com/en-us/windows/win32/taskschd/tasksettings-executiontimelimit)

`service stop` 首先请求程序优雅退出。Unix 接入现有 SIGTERM；Windows 增加当前用户专属、绑定实例的本地命名管道，验证调用方 SID 并把退出请求转为现有 cancellation token。管道仅支持进程状态/退出控制，不暴露邮件工具。此控制用于进程管理，MCP 通信仍全部走 HTTP。日常停止不直接使用强制结束任务；超时返回失败，强制结束作为单独故障操作。

可控停止保持一个绝对截止时间，关闭准入、处理进行中的发送、排空工作及更新缓存清洁状态。能够设置停止宽限的后台管理器采用当前 Compose 的 165 秒，并随应用实际退出预算校验；Windows CLI 等待自身退出完成后才报告停止成功。

Windows 注销、关机和 Task Scheduler 直接终止单独按系统终止场景验收，使用运行见证和 dirty 恢复保护下一次启动。与显式 stop 的优雅排空分开记录；任务结束时清洁凭据只在应用完整完成退出流程后写入。已开始的发送按持久化意图恢复，未知结果继续沿原键核查。

个人后台任务随用户会话运行。睡眠、注销、关机期间暂停同步；需要全天运行时采用现有服务器部署。

## 7. npm 与 PyPI 打包

### 7.1 共用产物与版本

一个 Git tag 绑定一个源码 SHA。Cargo、wheel、npm 包使用同一发布版本；预发布版本在 PEP 440 与 npm SemVer 间显式映射。`--version` 输出版本、源码 SHA 和 target。

每个 target 指定唯一二进制生产者，其他渠道只封装：

| target 类别 | 唯一生产者 | 分发去向 |
| --- | --- | --- |
| 首发 Windows MSVC、macOS、Linux glibc wheel targets | 在精确导出源码和独立 Cargo target 上执行一次 maturin `bin` 构建 | wheel 内 executable 为规范产物，同一文件提取至 npm 平台包和原生归档 |
| 现有 Linux amd64 musl | 当前提交绑定 `package-release.sh` | 首版继续提供 GitHub 原生归档；暂不增加同 target 的 wheel/npm 包 |

CI 对最终 wheel、npm 包和原生归档分别解包，检查 executable SHA-256、文件名称和执行权限。不同 target 分别记录摘要，glibc 与 musl 分别建模。后续增加 musl wheel/npm 时先将其唯一生产者迁移为 maturin，并把现有脚本的恢复验收复用于该产物，消除重复编译入口。

若启用平台代码签名，在唯一构建完成后对规范 executable 签名，再封装各渠道。wheel 需用标准工具重新打包并更新 RECORD；最终摘要在签名和封装完成后计算。所有渠道消费同一签名后 executable。签名身份、最低系统及签名校验纳入该平台发布记录。[wheel 文件与 RECORD 规范](https://packaging.python.org/en/latest/specifications/binary-distribution-format/)

构建流水线改造继续保留源码 SHA 导出、独立 Cargo target、vendor allowlist 和恢复验收。

### 7.2 PyPI

- 使用 maturin `bindings = "bin"`，把现有 Rust binary 放进平台 wheel，保持无 Python 绑定的实现。[maturin bin](https://www.maturin.rs/bindings.html)
- 每个平台发布对应 wheel；Linux glibc 使用经过 audit 的 manylinux wheel。后续启用 musl wheel 时使用 musllinux 标签并执行上一节的生产者迁移。最低系统/ABI 由实际构建和原生烟测确定。[maturin 分发](https://www.maturin.rs/distribution.html)
- 首版只发布已验收 wheels，源码构建继续走 Git/Cargo；未支持平台返回明确安装错误，避免终端用户意外进入 Rust 编译。
- uv/Python 环境承担安装，邮件服务由 wheel 内 Rust 程序执行。

### 7.3 npm

- 主包只含小型 JS 启动器和平台映射；按 OS、CPU、必要时 libc 选择精确同版本的平台包。
- 平台包包含原生程序、许可证和构建清单；使用 `os`、`cpu`、`libc` 与 `optionalDependencies` 筛选。[npm package.json](https://docs.npmjs.com/cli/v11/configuring-npm/package-json/)
- 安装脚本不联网下载 GitHub 二进制、不编译、不启动服务。二进制直接随 registry 包分发。
- 启动器使用参数数组创建子进程，避免 shell 拼接；转发退出码和必要信号。后台任务登记时直接定位原生程序。
- optional dependencies 被禁用、平台包缺失或平台不支持时，输出具体修复信息并退出；不临时下载替代物。
- 包内不含邮箱数据、秘密、开发缓存和测试夹具；使用文件 allowlist 与安装归档检查。

### 7.4 平台矩阵

第一轮必须验收 Windows x64、Linux glibc x64、macOS arm64；继续保留现有 Linux musl x64 归档。第二轮扩展 Linux arm64、macOS x64。Windows arm64 待对应原生测试环境就绪后加入。

每个声称支持的平台均覆盖基础读取、HTTP 附件、缓存、后台启动与优雅退出。发布说明列出实际 OS/CPU、最低 OS/ABI、测试客户端及安装渠道。

## 8. 发布流水线

1. PR 阶段验证源码、依赖、Docker 兼容、平台存储及客户端路径；包相关改动增加 wheel/npm 安装烟测。
2. 在 main 的准确提交上通过完整 CI，再推送版本 tag；沿用当前发布门禁。
3. 从 tag 的精确源码导出构建，固定 Rust、锁文件、打包工具和构建镜像/runner 信息；每个 target 使用隔离构建目录。
4. 平台 job 构建原生程序和包，执行合成 IMAP/OIDC/MCP 测试、缓存恢复和安装后启动测试；无 Rust 编译器的干净环境验证安装。
5. 汇总 manifest：版本、源码 SHA、target、最低 OS/ABI、二进制摘要、包摘要、schema、测试报告链接。
6. 发布 job 仅下载已通过的产物，校验摘要后上传；使用受保护 GitHub Environment 和短期 OIDC Trusted Publishing。构建 job 无 registry 发布权限。
7. npm 先发布所有平台包，再发布引用这些版本的主包；候选使用 `next`。PyPI 先通过 TestPyPI 验证完整上传/安装流程，再发布正式版本。测试环境仅使用专用测试索引，避免混用来源。
8. 从真实 registry 全新下载安装，验证 executable 摘要、版本、初始化和 HTTP 调用；完成后将 npm `latest` 指向新版本，完善 GitHub Release。
9. 部分渠道失败时保留发布状态清单，只补传缺失且摘要一致的文件；已发布版本内容修正使用新版本。异常版本 npm 回退 dist-tag/标记 deprecated，PyPI 按需 yank。

PyPI 与 npm 均提供 Trusted Publishing。PyPI 推荐 job 级 `id-token: write`；npm 的工具最低版本及支持 runner 在实施时按官方要求固定。项目用 pnpm 管理 Node 依赖；registry OIDC 上传可使用官方 npm CLI。[PyPI Trusted Publishing](https://docs.pypi.org/trusted-publishers/using-a-publisher/)、[npm Trusted Publishing](https://docs.npmjs.com/trusted-publishers/)

首次发布准备包括包名/namespace 所有权、发布者账号、MFA、可信工作流身份及环境配置。公共发布在实现与验收完成后单独执行。

## 9. 升级、恢复与卸载

升级采用显式操作，服务不自行下载或替换运行中的程序：

1. 查看目标版本兼容说明，记录当前版本、安装渠道及绝对路径。
2. `service stop`，确认完成退出并释放文件锁；备份配置、秘密、操作库、注册库、运行见证和需要保留的缓存，记录 schema。
3. 通过原安装渠道安装指定新版本。固定版本安装后，升级也明确指定版本，避免版本约束导致停留在旧版。
4. 执行 `service install --replace`，更新后台任务的原生程序绝对路径，保留配置和数据路径。
5. 启动，检查 HTTP 初始化、工具发现、一次只读查询及缓存状态。

```sh
rust-email-mcp service stop
# 完成上述备份后，选择原渠道之一：
uv tool install rust-email-mcp==X.Y.Z
# 或 pnpm add -g @scope/rust-email-mcp@X.Y.Z
rust-email-mcp service install --replace
rust-email-mcp service start
rust-email-mcp doctor
```

程序回滚保留最新操作库；只有数据库格式兼容时使用旧程序。缓存快照恢复继续执行现有 `--cache-restored` 流程；配置主体和路径保持原绑定。新 CLI 同时提供本地配置下的恢复入口，复用同一个恢复实现。发送结果为 `unknown` 时沿原键核查，恢复流程不自动重发。

轮换 MCP 凭据时执行拟新增 `token rotate`，更新客户端配置并重新加载服务；保持本地主体不变，旧凭据即时撤销或按显式重启切换点撤销，验收采用确定的一种行为。首版采用重启切换，正常排空既有请求。

卸载先执行 `service uninstall` 停止并删除后台任务，再使用 `uv tool uninstall` 或 `pnpm remove -g` 删除程序。默认保留用户数据。彻底删除由独立 `purge` 操作列出准确目录后确认，检查解析后的目标路径并拒绝越界。

## 10. 验收清单

| 类别 | 必测场景 | 通过条件 |
| --- | --- | --- |
| 安装 | 干净机器、无 Rust、含空格/中文路径、npm optional 被禁用 | 支持平台可运行；缺依赖错误有具体操作指引 |
| 初始化 | 新装、重复 setup、旧配置存在、IMAP 离线、秘密输入 | 不覆盖身份/数据；不记录秘密；生成有效配置 |
| HTTP | initialize、tools/list、tools/call、并发、取消、重连、系统代理 | SDK 真实协议链路通过；预算及取消语义保留 |
| 本地认证 | 缺/错 token、恶意 Host/Origin、非回环监听、token 轮换 | 在访问账户与后端前拒绝；轮换后原发送记录可查 |
| 远程兼容 | 原 env、多账户 JSON、OAuth 发现/刷新、附件 GET | 旧部署路径按原配置通过 |
| 多客户端 | Codex、VS Code、Claude Code；各两会话 | HTTP header 生效，复用单个服务与数据库；记录客户端版本 |
| 附件 | Base64、小/大 HTTP 下载、错误主体、取消、摘要校验 | 字节一致，目录私有，暂存清理符合原规则 |
| Windows 存储 | DACL、reparse point、锁、句柄占用、注册库创建/替换/目录持久化、异常退出 | 缓存 off 的首次启动、附件与 full 缓存实际运行；崩溃恢复及不安全目录拒绝通过 |
| 后台生命周期 | 登录启动、重复启动、休眠恢复、显式 stop；另测注销/关机/任务终止 | 无重复 Journal；可控退出遵守绝对期限；系统终止后 dirty 与发送意图恢复正确 |
| 版本变更 | 原生二进制从两个渠道安装、升级、路径变更、回滚 | 对应 target 摘要一致，稳定主体与数据路径保持 |
| 邮件语义 | 只读标记不变、账户 ACL、合成发送去重、unknown 恢复 | 保留现有生产入口回归 |
| 分发验收 | TestPyPI、npm next、正式 registry 下载 | 安装后实际完成 HTTP 调用；构建清单可追溯 |

开发与 CI 使用合成邮件及隔离身份。真实邮箱只读验证和真实发送/整理分别安排；生产部署沿既有授权流程执行。测试规模继续满足 `uv run scripts/test-inventory.py` 的项目约束，优先扩展代表性生产入口回归。

附件验收分为服务端认证下载与客户端文件消费两项：客户端文件读取必须实际携带 Authorization 并成功处理文件。未内建该消费路径的客户端，文档给出认证下载辅助命令与本地文件读取步骤；无需扩展 MCP 传输种类。

## 11. 实施顺序与交付物

| 工作包 | 范围 | 交付与完成条件 |
| --- | --- | --- |
| H0 设计定稿 | 本文；认证/路径/退出契约 | 确认 CLI、个人实例边界和首发平台；更新架构验证映射 |
| H1 本地 HTTP | CLI、配置、稳定主体、token、Host/Origin | Linux 上前台初始化和三个客户端合成调用通过，远程回归通过 |
| H2 平台存储 | Windows 权限/文件身份、锁、注册库创建/替换及目录持久化 | 缓存 off 首次启动、附件、缓存、运行见证与崩溃恢复验收通过；macOS 同语义回归 |
| H3 使用流程 | setup、配置导出、doctor、用户后台任务 | 首发平台安装后执行一条 setup，复制配置即可读取合成邮件 |
| H4 包与流水线 | maturin wheels、npm 平台包、版本清单、可信发布 | 每个平台从候选 registry 安装验证，跨渠道摘要一致 |
| H5 发布与运维 | 升级/恢复/卸载、帮助文档、完整客户端验收 | 完整 main CI、精确 tag、下载复验与版本化兼容矩阵 |

每个工作包独立分支/PR，按依赖合并。H1 的认证和 H2 的持久化完成独立审阅。实施状态、证据、遗留项放进各包 PR；本方案只维护设计决定和验收要求。

首版新增文档限制为：README 的本地安装入口、本地部署指南、客户端配置示例、发布维护步骤及故障处理。设计目标是安装与连接简单，程序内部保持已有权限、恢复和资源约束。

## 12. 资料使用说明

以上链接于 2026-11-06 核对。协议以明确版本的 MCP 2025-11-25 传输/授权文档为引用基线；实际实现继续匹配仓库锁定 SDK 的协议能力，并由客户端协商测试验证。客户端与包管理器资料使用各项目官方文档。

官方资料支持本地 HTTP、客户端配置字段、二进制打包、包管理生命周期、系统后台运行和可信发布机制。本文的本地 token、CLI、目录结构、平台顺序及发布门禁为结合本仓库约束提出的项目设计。
