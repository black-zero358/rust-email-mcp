# R05 多邮箱只读配置与身份设计

日期：2026-11-02。沿用 [读取方案 R05](read-capabilities-plan.md#r05-多账户读取) 和统一身份契约；本文件记录本轮实现接口、配置及离线验证安排。新增邮箱固定只读，旧写工具继续使用原邮箱及原发送记录库。

## 配置与旧调用

服务级 OIDC、监听地址、资源地址、预算和 `DATABASE_PATH` 继续使用原环境变量。`ACCOUNT_CONFIG_PATH` 可选，指向严格 JSON：版本 1、最多 128 KiB、最多 16 个当前账户（包含原邮箱）、拒绝未知字段和重复键。配置重启生效。

| 启动方式 | 邮箱配置来源 | 默认读取 | 旧写工具与发送记录 |
| --- | --- | --- | --- |
| 未设置 ACCOUNT_CONFIG_PATH | 原 EMAIL/IMAP/SMTP/目录/MAIL_MODE 环境变量 | default | 原行为；账户表保持 v1 |
| extend_legacy | 原邮箱身份仍来自旧环境变量；JSON 显式写原邮箱 ACL 与追加只读邮箱 | JSON default_account_id | 始终绑定原 default；追加全局 OIDC 主体不会自动获得原邮箱权限 |
| file_only | 所有邮箱配置来自 JSON，服务级设置仍来自环境变量 | JSON default_account_id | 必须声明固定 legacy_default 锚点；复用既有记录库时身份/路径必须匹配 |

file_only 遇到任意旧 EMAIL_ADDRESS、EMAIL_PASSWORD、IMAP_HOST/PORT、SMTP_HOST/PORT/STARTTLS、DRAFTS_MAILBOX、TRASH_MAILBOX、ARCHIVE_MAILBOX、MAIL_MODE 变量即拒绝启动，避免隐式优先级。extend_legacy 的 legacy_default 只允许显示名和显式 ACL，不能覆盖原邮箱身份。file_only 的 permission_mode 非 read 时必须声明 SMTP；read 可使用 smtp:null。

`read_conversation` 的 `scope: "standard"` 可加入每个账户单独配置的归档目录。纯环境配置和 `extend_legacy` 的原邮箱读取 `ARCHIVE_MAILBOX`，追加邮箱读取各自的 `accounts[].folders.archive`。`file_only` 的原邮箱读取 `legacy_default.folders.archive`，追加邮箱同样读取各自的 `folders.archive`。填写服务端的准确 IMAP 目录名，最多 512 个 Unicode 码点及 2048 个 UTF-8 字节，且须非空、无控制字符。省略该配置即可使用无归档目录的标准范围。若提供 JSON `folders` 对象，仍须填写 `drafts` 和 `trash`。

新 JSON 严格约束名称、秘密引用及 ACL；纯旧环境变量模式保留原解析边界，包括既有重复 OIDC_SUBJECTS，不用新 JSON 规则收紧旧启动方式。

新 JSON 的资源/issuer URL、IMAP/SMTP TLS 主机名及 SMTP TLS 参数在登记/发送记录库打开前验证，不发起网络请求。主机名显式使用运行时相同的 rustls ServerName 校验；lettre 的 [TlsParameters 实现](https://docs.rs/lettre/latest/src/lettre/transport/smtp/client/tls.rs.html)负责构造连接参数，其构造结果另行预检。

追加只读邮箱示例：

```json
{
  "version": 1,
  "mode": "extend_legacy",
  "default_account_id": "finance",
  "legacy_default": {
    "display_name": "原邮箱",
    "read_subjects": ["oidc-original-user"],
    "write_subjects": ["oidc-original-user"]
  },
  "accounts": [{
    "registration_key": "finance-mailbox",
    "id": "finance",
    "display_name": "财务邮箱",
    "address": "finance@example.test",
    "imap": {"host": "imap.example.test", "port": 993},
    "password": {"env": "FINANCE_MAIL_PASSWORD"},
    "folders": {"drafts": "Drafts", "trash": "Trash", "archive": "Archive"},
    "read_subjects": ["oidc-finance-user", "oidc-original-user"]
  }]
}
```

file_only 的 legacy_default 另外声明 address、imap、password、smtp、folders、permission_mode；例如：

```json
{
  "version": 1,
  "mode": "file_only",
  "default_account_id": "default",
  "legacy_default": {
    "display_name": "主邮箱",
    "address": "owner@example.test",
    "imap": {"host": "imap.example.test", "port": 993},
    "password": {"file": "primary.secret"},
    "smtp": null,
    "folders": {"drafts": "Drafts", "trash": "Trash", "archive": "Archive"},
    "permission_mode": "read",
    "read_subjects": ["oidc-reader"],
    "write_subjects": []
  },
  "accounts": []
}
```

秘密引用仅接受 `{ "env": "NAME" }` 或 `{ "file": "relative/path" }`，不接受明文。环境变量名最多 128 个 ASCII 大写字母/数字/下划线；秘密最多 4096 字节，非空且无 NUL/内嵌 CR/LF。Unix 文件引用位于 MAIL_SECRET_ROOT 的规范路径内，拒绝绝对路径、父目录和 symlink；文件仅所有者可读写，根目录及中间目录不能由组/其他人写入。打开文件后核对 inode/权限，再从同一文件描述符有界读取；文件末尾允许去掉一组 LF/CRLF。Windows 使用 env 引用；当前 file 引用会在配置验证时拒绝，待实现原生 ACL 校验后再开放，避免略过私密文件权限要求。

registration_key 是稳定配置键，id 是调用参数 account_id 使用的别名，display_name 是展示名称。前两项采用 `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`，区分大小写；显示名最多 128 个码点/512 UTF-8 字节，可用中文。原邮箱固定 registration_key=`legacy-default`、id=`default`。显示名或别名改变保持 UUID 和世代；原别名停止路由，调用者从 list_accounts 获取新别名。

## 权限与发送记录

每次请求按“全局 OIDC 主体集合 ∩ 该邮箱显式 ACL”授权。原邮箱 write_subjects 必须包含于其 read_subjects；追加邮箱没有写权限配置字段，运行模式固定 read。配置可列出尚未加入全局准入的主体，实际访问仍由交集决定。

- list_accounts 只返回当前主体可读取的邮箱，允许空列表。主体看不到配置默认读取邮箱时 default_account_id 为 null。
- 省略 account_id 仍选择配置的默认读取邮箱，然后授权；不自动改选其他邮箱。
- 所有旧写工具拒绝 account_id，始终选择固定 legacy default，再检查其模式和显式写 ACL。默认读取指向其他邮箱也保持此规则。
- get_operation 保留原参数，独立按 principal + operation_key 查询原 Journal。没有邮箱权限的全局已准入主体仍可检查自己的旧 unknown/accepted 记录。
- tools/list 按当前请求主体过滤；无邮箱读 ACL 时隐藏 13 个账户读取工具。直接调用已知账户读取工具仍进入统一账户授权错误；未知工具及禁用写工具使用原协议错误。
- 未知别名、无权限、账户已移除、旧 AccountKey/世代均返回 ACCOUNT_UNAVAILABLE，固定说明、不回显别名。附件 GET 对账户或句柄不可用统一 404。

Auth 继续只验证 OIDC/JWT 和全局 subject，不承担邮箱授权。账户授权先于缓存、执行队列和 IMAP 连接；不能缓存某次请求的 ACL 结果供以后使用。

## 注册库与启动事务

普通旧环境模式继续使用原 v1 注册表。显式账户 JSON 启动时，将 v1 升为 v2；先完整解析配置、解析秘密、验证 ACL/默认项，再检查注册库和原邮箱/操作库绑定，全部完成后才允许 main 调用 Journal::open（该函数会恢复 sending 状态）。

v2 保留原 legacy_default 表与其 UUID、generation=1、身份摘要和原操作库路径摘要；追加 account_records 与配置要求标记。注册库只保存 UUID、世代、稳定键、当前别名、启用状态和身份摘要，不保存密码、邮箱地址、服务器地址或原操作库路径明文。最多 1 MiB、256 个历史注册键，超出拒绝继续登记。

| 变化 | 身份处理 |
| --- | --- |
| 显示名、别名、密码轮换、IMAP 主机大小写 | 保持 UUID/世代 |
| 追加邮箱的登录邮箱、IMAP 主机或端口变化 | 同 UUID，世代递增 |
| 追加邮箱从配置移除 | 保留 inactive tombstone；当前请求和引用不可用 |
| 同 registration_key 删除后重新加入 | 同 UUID，世代递增，旧引用/缓存命名空间失效 |
| 新 registration_key | 新 UUID；不能用改显示名代替稳定配置键 |
| legacy default 身份或操作库路径变化 | 拒绝启动，原发送记录保持原字节；另做显式迁移 |

在持久锁内建立 staging SQLite，使用独立事务、FULL 同步、完整性/格式核对后原子发布。发布前故障保留原正式库；保留 v1-backup 及最近完整 previous 副本供维护恢复，发布后的目录同步异常也保留完整旧副本。临时/备份/锁文件与操作库及其 sidecar 一并做路径冲突检查。显式多账户运行期持有注册锁，禁止第二进程同时修改此注册库。迁移不打开或恢复 Journal。

已有 v2 注册库时，移除 ACCOUNT_CONFIG_PATH 会拒绝启动；恢复明确的账户配置后再启动。旧二进制回退需要恢复 v1 注册副本，操作记录库保留最新内容；未来持久缓存与注册表恢复联动另按 R11 规程处理。

## 固定跨工作者接口

```rust
config::load_runtime() -> anyhow::Result<ValidatedAccountSet>
accounts::open_catalog(ValidatedAccountSet) -> anyhow::Result<Arc<AccountCatalog>>

ValidatedAccountSet { service_config: Arc<Config>, default_read_account: String,
                      accounts: Vec<ValidatedAccountConfig>, source: AccountConfigSource }
AccountLease { key: AccountKey, alias: String, display_name: String,
               config: Arc<Config>, is_legacy: bool, cancellation: CancellationToken }

AccountCatalog::config() -> &Arc<Config>
AccountCatalog::default_read_account_id() -> &str
AccountCatalog::read_account(principal, Option<&str>) -> Result<AccountLease>
AccountCatalog::read_key(principal, &AccountKey) -> Result<AccountLease>
AccountCatalog::write_account(principal) -> Result<AccountLease>
AccountCatalog::visible_accounts(principal) -> Vec<AccountLease>
AccountCatalog::registered_accounts() -> Vec<AccountLease>

MailRuntime::new(Arc<AccountCatalog>, Arc<Journal>) -> anyhow::Result<Self>
MailRuntime::config() -> &Arc<Config>
MailRuntime::execute(name, args, principal) -> Result<Value>
MailRuntime::has_read_access(principal) -> bool
MailRuntime::legacy_mode_for(principal) -> Option<String>
MailRuntime::list_accounts(principal) -> Value
```

Config 保留解析后的原字段兼容视图：service_config 对应固定 legacy default；每个追加邮箱持有覆盖其 IMAP/凭据/目录、mode=read 的独立视图。追加邮箱不构造 SMTP sender；file_only 原邮箱 read + smtp:null 也不构造 SMTP。AccountKey 派生 Hash，继续提供现有 reference_namespace，附件/调度/缓存均用内部键，不用别名当身份。

生产配置先按重启生效。测试专用 from_test_accounts/revoke_read_for_test/remove_account_for_test 复用实际 Catalog 查询，验证每次请求重新授权和旧 token 取消；不提供模型可调用的账户管理或热重载工具。

跨目录会话游标绑定发起主体和账户世代。续页仍须授权该账户，并按原调用传回账户参数（非默认账户须显式指定）。游标保存在当前进程内存，空闲 10 分钟到期。配置重启后，客户端用种子邮件和所需目录范围重新发起查询。

## 全局调度与资源

所有 MailService 共用同一个 Scheduler，保持全局 4 个后端执行槽、20 个运行及排队请求、10 秒排队期限和 60 秒操作期限。账户常规使用 2 槽；无其他未满 2 槽且正在等待的账户时可借用到 4。已借出的执行不抢占；下一槽释放后优先满足等待账户的常规份额，避免借用者持续插队。

请求取消、等待超时或 future 丢弃均通过 RAII 清理排队项；BackendPermit Drop 释放额度并重新调度。基础份额优先于借槽，同一份额级别前台优先；未来后台任务全局最多 1 个。例如 A 已有 2 槽并等待第 3 个前台请求、B 尚无执行且有后台任务时，B 可先获得 1 槽，使后台刷新持续有进展。已经开始的发送继续由原发送生命周期持有许可，不因 MCP 调用取消而中断记录。

附件句柄 64 个、传输槽 4 个保持全局共享，避免账户数放大额度。后端 FETCH 结束后释放后端槽；二进制传输许可保留至消费或取消。每次 GET 用当前 Principal + 句柄 AccountKey 再授权，并保留 UIDVALIDITY、摘要和响应预算。

## 文件职责与离线验证

| 执行者 | 文件所有权 | 交付 |
| --- | --- | --- |
| 配置/登记 | src/config.rs、config/**、accounts.rs、accounts/**、本文件；账户错误 helper 按协调修改 error.rs | 严格 JSON、秘密引用、ACL、身份/世代、原子迁移、故障恢复测试 |
| 邮件运行时 | mail.rs、mail/** | MailRuntime、共享 Scheduler、账户读路由、固定原写路由、附件/缓存隔离 |
| 协议接线 | server.rs、server/**、main.rs、auth.rs 的必要接线 | 13 个读取工具 account_id、按主体发现、Schema、启动次序、双账户 HTTP/JWT 验收 |
| 主执行者 | 共享 README/架构/维护/AGENTS/.env/部署文档 | 集成审阅、完整验证、发布记录 |

测试继续只用本地模拟邮箱与合成邮件：旧环境启动；旧环境追加只读；file_only；默认读取换邮箱但旧写目标固定；空 ACL 与历史记录；同文件夹/UID/UIDVALIDITY 在双邮箱的正文、头缓存、附件 id 与下载不混用；无权限时后端连接计数为零；身份变化和删除重加的旧引用失败；全局 4/20 与公平借槽；配置/迁移失败原 Journal 字节不变。真实连接、登录和部署不在本阶段执行。

2026-11-02 模块验证：在 `/home/oeic_fe/.cache/rust-email-mcp-r05-accounts-check` 建立临时同源 harness，Cargo.toml/Cargo.lock 取自项目（仅 vendored 路径改为项目绝对路径），`#[path]` 直接载入实际 config/accounts/error/journal 源文件。命令：

```sh
CARGO_TARGET_DIR=/home/oeic_fe/.cache/rust-email-mcp-target \
  /home/oeic_fe/.cargo/bin/cargo +1.99.1 test --offline --locked --lib
```

通过 26 项：config 7、accounts 17、journal 2；包含发布示例严格解析、v1/v2 发布前故障、previous 副本发布失败与完整重试、有界核验已有备份、中断临时文件清理、原记录库字节不变。Linux 执行了文件权限/symlink/目录/字节上限测试；Windows 的拒绝文件引用分支提供 cfg 专用回归，待 Windows Rust 环境执行。跨模块完整 crate 与 HTTP/JWT 验证由集成轮运行。
