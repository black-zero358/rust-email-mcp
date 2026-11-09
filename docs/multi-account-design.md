# 多邮箱只读配置与身份设计

本文记录当前多账户配置、授权、注册库与共享调度。追加邮箱固定只读，旧写工具继续使用原邮箱及原发送记录库。配置操作见[部署指南](deployment.md#多账户配置)，交付历史见[读取能力记录](read-capabilities-plan.md)。

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
- tools/list 按当前请求主体过滤；无邮箱读 ACL 时隐藏 14 个账户读取工具。直接调用已知账户读取工具仍进入统一账户授权错误；未知工具及禁用写工具使用原协议错误。
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

已有 v2 注册库时，移除 ACCOUNT_CONFIG_PATH 会拒绝启动；恢复明确的账户配置后再启动。旧二进制回退需要恢复 v1 注册副本，操作记录库保留最新内容；持久缓存与注册表恢复联动按[缓存恢复规程](deployment.md#持久缓存备份与恢复)执行。

## 运行期接口

配置加载、账户授权与任务路由分别由 [`config`](../src/config.rs)、[`AccountCatalog`](../src/accounts/catalog.rs) 和 [`MailRuntime`](../src/mail/runtime.rs) 负责。账户租约携带持久身份、配置世代、权限及取消状态；读取和写入均经目录授权后进入调度。

Config 保留解析后的原字段兼容视图：service_config 对应固定 legacy default；每个追加邮箱持有覆盖其 IMAP/凭据/目录、mode=read 的独立视图。追加邮箱不构造 SMTP sender；file_only 原邮箱 read + smtp:null 也不构造 SMTP。AccountKey 派生 Hash，继续提供现有 reference_namespace，附件/调度/缓存均用内部键，不用别名当身份。

生产配置先按重启生效。测试专用 from_test_accounts/revoke_read_for_test/remove_account_for_test 复用实际 Catalog 查询，验证每次请求重新授权和旧 token 取消；不提供模型可调用的账户管理或热重载工具。

跨目录会话游标绑定发起主体和账户世代。续页仍须授权该账户，并按原调用传回账户参数（非默认账户须显式指定）。`cursor_storage` 标明 memory 或 persistent；范围全部纳入 metadata 配置的持久游标在原 10 分钟期限内可跨重启续查。memory 游标在重启后从种子重新查询。账户注册库或缓存备份恢复按[缓存恢复](deployment.md#持久缓存备份与恢复)执行。

## 全局调度与资源

所有 MailService 共用同一个 Scheduler，保持全局 4 个后端执行槽、20 个运行及排队请求、10 秒排队期限和 60 秒操作期限。账户常规使用 2 槽；无其他未满 2 槽且正在等待的账户时可借用到 4。已借出的执行不抢占；下一槽释放后优先满足等待账户的常规份额，避免借用者持续插队。

请求取消、等待超时或 future 丢弃均通过 RAII 清理排队项；BackendPermit Drop 释放额度并重新调度。基础份额优先于借槽，同一份额级别前台优先；未来后台任务全局最多 1 个。例如 A 已有 2 槽并等待第 3 个前台请求、B 尚无执行且有后台任务时，B 可先获得 1 槽，使后台刷新持续有进展。已经开始的发送继续由原发送生命周期持有许可，不因 MCP 调用取消而中断记录。

附件句柄 64 个、传输槽 4 个保持全局共享，避免账户数放大额度。后端 FETCH 结束后释放后端槽；二进制传输许可保留至消费或取消。每次 GET 用当前 Principal + 句柄 AccountKey 再授权，并保留 UIDVALIDITY、摘要和响应预算。

## 验证入口

账户注册与迁移见 `src/accounts/tests.rs`、`src/accounts/catalog/tests.rs`，配置见 `src/config/account_config/tests.rs`，共享调度见 `src/mail/scheduler.rs`。`src/server/output_tests.rs` 使用签名 JWT、双账户 TLS IMAP 和实际 HTTP 路由核对授权、同 UID 隔离及附件交付。

现行原则与回归对应关系见[架构验证表](architecture.md#invariants-and-verification)，R05 的原始实验见[历史记录](archive/2026-11/validation.md#2026-11-02-r05-多账户离线验证)。
