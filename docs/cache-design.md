# 持久化缓存设计

本文记录持久元数据、完整内容与游标的配置、存储和恢复契约。实施顺序及验收条件以[读取能力方案](read-capabilities-plan.md#r10-至-r12-完整本地缓存)为准，带源码提交的验证结果进入 validation。

## 分阶段交付

R10 提供持久元数据、部件索引、中文检索基础及可跨进程重启的会话游标。R11 先交付内部文件存储与 dirty 基础，再接入原始邮件和派生内容、显式离线读取与写操作失效，分批契约见[正文与附件缓存设计](cache-content-design.md)。R12 接入完整历史回填、增量同步、删除核对和管理进度。R13 执行双账户、多客户端、故障恢复及持续负载验收。

默认关闭持久化。metadata 和 full 都只缓存管理员明确选择的账户和文件夹。full 保存已观察的完整内容，开放显式本地读取；默认在线调用仍检查当前权限、文件夹世代、目标存在性和命令终结，运行期缓存写入失败保留在线读取的实际结果。

## 配置示例

将[配置样例](../deploy/cache.json.example)保存到服务可读路径，并设置 `CACHE_CONFIG_PATH`。账户 ID 使用实际配置别名，文件夹使用准确的 IMAP 名称：

```json
{
  "version": 1,
  "mode": "metadata",
  "directory": "cache",
  "max_bytes": 268435456,
  "accounts": [
    {"account_id": "default", "mailboxes": ["INBOX"], "max_bytes": 134217728}
  ]
}
```

`directory` 相对配置 JSON 所在目录解析；例如 `/data/cache.json` 中的 `cache` 对应 `/data/cache`。全局和每账户额度均为实际磁盘字节，每账户不超过全局，每项至少 1 MiB。每账户最多 64 个目录；INBOX 大小写规范为 INBOX，其余名称精确匹配，重复项、未知账户及未知 JSON 字段在启动前拒绝。配置文件最多 256 KiB。

省略 `CACHE_CONFIG_PATH`，或使用 `{"version":1,"mode":"off"}` 关闭持久缓存。off 只接受 version 和 mode，不创建内容缓存。将样例的 mode 改为 full 即启用完整内容保存和显式离线读取。账户、目录或额度配置修改通过重启生效。所有模式启动时均在账户注册库旁更新运行见证，相关目录需可写；full 初始化失败会结束启动，管理员修复后重试。

full 可增加 `sync` 以启用管理员选定范围的后台回填与周期核对。以下示例使用全局及单账户 10 GiB 额度；实际额度按邮箱内容和数据库余量安排：

```json
{
  "version": 1,
  "mode": "full",
  "directory": "cache",
  "max_bytes": 10737418240,
  "accounts": [
    {"account_id": "default", "mailboxes": ["INBOX"], "max_bytes": 10737418240}
  ],
  "sync": {
    "enabled": true,
    "interval_seconds": 60,
    "reconcile_interval_seconds": 900,
    "protocol": "rev1"
  }
}
```

省略 `sync` 保持后台暂停，显式提供时必须设置布尔 `enabled`。两个间隔默认分别为 60 和 900 秒，允许 1 至 86400 秒；metadata/off 不接受该配置。启停和间隔修改通过重启生效；暂停保留已保存内容、检查点及缺口，已有显式离线读取继续按授权与 dirty 状态处理。模型通过 `get_cache_status` 查询进度，管理配置由管理员维护。同步设计与验收范围见[全量回填与增量同步](cache-sync-design.md)。

`sync.protocol` 接受 `rev1` 或 `auto`，省略时为 `rev1`。`auto` 在后台认证连接上探测能力并使用只读 `EXAMINE (CONDSTORE)`，按已知 UID 页获取 FLAGS 变化；能力缺失或扩展失败时回到 rev1。所有模式继续进行周期存在性核对。切换协议通过正常重启生效，前台读取工具保持原调用方式。

## 查询状态

`get_cache_status` 接受可选 `account_id`，省略时选择默认读取账户，先检查当前主体权限并仅查询本地状态。`mode` 为 off、metadata 或 full，`status` 为 off、ready 或 unavailable。账户未纳入配置时也返回 off；off 或 unavailable 时 storage 为 null。ready 时 `storage` 给出本账户 `max_bytes` / `used_bytes`、全局上限 `global_max_bytes`、启动未清洁标记 `startup_unclean`，以及各目录的 UIDVALIDITY、水位、最近检查时间、dirty、messages、complete_headers、indexed_headers 和 verified_parts。

这些计数描述已经观察并写入的当前目录世代。full 的 `content_pending` 显示正在排队或执行的前台内容保存，目录 `layers` 分别统计 raw、full_body 和 attachments 的 materialized、materialized_bytes、derivable_from_raw、missing、omitted 和 unsupported。部分头保留完整性标记，只有完整头和完整正文进入对应索引。状态读取先等待元数据本地屏障，最多 2 秒；内容保存进度另由 content_pending 和各层物化状态反映。缓存不可用时返回 unavailable，管理员检查目录、额度和恢复状态。

full 的 `sync` 返回启用策略、两个间隔和所选目录的同步状态。目录 `progress` 分别保存 `history`、`tail`、`reconciliation` 的扫描 ID、冻结 `high_uid`、修订和位置。`history_coverage` 对应首次冻结的历史范围，`current_coverage` 对应已观察水位；两者分别报告发现完成、存在性与标记核对、内容层以及 `work` 和按原因聚合的 `gaps`。目录状态查询失败时 `progress:null`，`error` 给出固定 quota 或 storage 类别；账户仍为 ready、目录已知失效证据待持久化时返回 verification_pending，成功时 error 为 null。跨 UIDVALIDITY 的旧账户快照会先返回 unavailable，sync.folders 为 null。

目录 `runtime.last_error` 记录后台最近失败类别 network、protocol、storage、quota 或 null，`runtime.quota_paused` 显示当前进程的配额暂停。它们与当前状态查询的目录 error、前台缓存证据队列的 write_health 分别报告。

`sync.protocol` 返回配置策略，目录 `runtime.protocol` 返回当前使用的 `rev1` 或 `condstore`。`runtime.protocol_fallback` 为 null，或固定类别 `capability_unavailable`、`extension_failed`、`nomodseq`、`modseq_rollback`。目录 `condstore` 为 null 或已持久化的 FLAGS 水位：`uid_validity`、覆盖 `high_uid`、`committed_modseq`、当前轮次的 `target_modseq`/`known_after` 和 `checked_at_ms`。两项 MODSEQ 使用十进制字符串，客户端按整数文本处理；每页保存位置，只有全部页面完成才推进 committed。NOMODSEQ、失效世代或待持久化的撤销会隐藏旧水位；该状态与完整内容覆盖分别展示。

完成判定按具体数据层读取：`listing_facts_complete` 要求 FLAGS、INTERNALDATE 和 SIZE 齐备；头部完整性、精确 Message-ID 索引、清单和正文索引各有独立计数与完成字段。raw/full_body/attachments 的 `available_complete` 允许已确认可从原件派生的内容，`materialization_complete` 还要求实际文件齐备。两者都受完整发现与存在性核对约束，保留 missing、omitted 和 unsupported 缺口。`flags_checked_at_ms` 是范围内最近一次单封标记观察，完整范围核对时间取 `reconciliation.completed_at_ms`。后台暂停时继续显示已提交进度。

full 的显式工具矩阵及字节限制见[内容缓存设计](cache-content-design.md#显式离线契约)。离线结果包含 `read_source:"cache"` 或 `search_source:"cache"`，以及 `cache.stale:true`、`remote_generation_unverified:true`、各范围最后观察时间和 coverage。FLAGS、INTERNALDATE、SIZE 保留独立观察时间，缺失字段以未知状态报告。`get_email_status`、`get_mailbox_statistics` 和 `diagnose_connectivity` 保持在线；`get_cache_status` 查询本地状态。中文全文搜索可返回 `next_cache_cursor`，下一页使用相同查询和筛选条件；数据修订变化时重新开始搜索。

`write_health` 始终返回 `failed_writes`、`last_error` 和 `quota_blocked`。失败计数以当前进程中归类为 quota、queue_full 或 storage 的失败证据任务为单位，一批记录计一次；固定最后错误类别为 quota、queue_full、storage 或 null。quota_blocked 在当前进程内保留，重启后重新评估。普通额度不足时仍展示已有 storage 和覆盖计数，管理员可据此增加额度或调整范围；后续成功状态读取或小额写入保留历史失败记录。

## 存储与预算

采用受根实例锁保护的专用目录，每个 AccountKey 命名空间保存一份 `cache.sqlite`。目录名由内部账户 UUID 与配置世代的固定摘要生成，服务端文件名和模型参数均不能作为路径。分库使 SQLite 主库、索引、回滚日志及临时文件可以按账户实测记账；全局预算另外统计全部分区，包括旧世代和待清理数据。操作库与账户注册库保持独立。

这一决定将规划中的逻辑 cache.sqlite 按账户分区，保留统一缓存接口和全实例配额。根目录、分区及数据库路径拒绝符号链接和与操作库、注册库、附件暂存目录的重叠。每次元数据与索引更新使用同库短事务；不跨 IMAP 请求持锁。R10 优先使用 DELETE 回滚日志，并在事务前预留主库增长及日志上界，`max_page_count` 只承担主库边界。

缓存元数据键包含账户世代、规范文件夹名、UIDVALIDITY、UID。R10 保留完整头原始值，Message-ID 参与头部文本索引；完整头标志和部件清单版本单独保存。正文及其完整性、派生状态由 R11 接入，只有完整解析的正文进入正文索引。SQLite FTS5 trigram 用实际锁定的 bundled 库验证；短于三个码点的查询使用带检查点的有界候选扫描，返回覆盖范围。

## 账户配置与接线

通过 `CACHE_CONFIG_PATH` 引用严格版本化 JSON，声明模式、私有缓存根目录、全局字节上限，以及账户别名、明确的目录列表和每账户字节上限。省略配置时不打开缓存文件。账户解析、目录与额度校验、路径冲突检查在 Journal 恢复前完成；账户别名随后绑定当前 AccountKey。

读取入口先执行当前 principal 的账户 ACL，再进入缓存或执行队列。成功 EXAMINE 所观察的 UIDVALIDITY 立即使旧目录世代不可用；候选头和部件证据只在对应命令 tagged OK 后登记。缓存对象接收内部类型化证据，不接收 MCP 请求对象或邮箱凭据。只读状态查询报告配置范围、层级覆盖、配额、故障及更新时间。

在线读取将元数据写入单个有界 FIFO 工作队列，最多 16 项和 8 MiB，执行中的任务继续占用额度。普通证据排队失败保留已经取得的在线结果并记录 queue_full，已有缓存继续可读。世代观察未成功登记或存储失效时，将受影响的持久范围标为不可用；新观察的世代先在内存中阻断旧数据，再有序持久化。关闭服务时等待已接收工作结束，符合干净退出条件后才清除运行标记。

## 会话检查点与重放

会话编解码由 conversation 模块负责，存储模块只处理绑定元数据与有界字节流。记录包含版本、主体摘要、AccountKey 命名空间、固定目录及水位、关联图、候选检查点、已交付位置、active token、最近已发布输入的 replay 快照和响应裁减标记。正文不进入游标。

编码使用固定整数宽度和显式计数。解码先检查剩余编码长度、计数乘法及 owned capacity，再分配；核对所有索引、引用、UID 区间、图状态、重放位置、版本和摘要，拒绝多余尾字节。全实例仍保持 64 项、8 MiB 状态预算，包含执行中租约及恢复状态。

完整 JSON-RPC 响应和实际 next token 通过预算后，以一个短 SQLite CAS 事务原子替换检查点、active 和最近 replay。事务成功后才确认交付。崩溃发生在提交与回包之间时，原输入 token 可重放已提交页；取消或写入失败保留未交付数据。执行中的阻塞文件工作继续拥有租约和额度，结束前不允许另一请求接管。

有效期记录为绝对到期时间，重启不延长原 600 秒窗口。无进展成功页轮换 token 并保留重放，但不刷新到期时间；未来或异常倒拨时间按失效处理。恢复的 live 会话在线复核全部冻结目录，任一 UIDVALIDITY 改变使其失效；cache 会话复核本地冻结范围、索引修订、dirty 修订和恢复世代。游标来源保持绑定。

跨目录结果的 `cursor_storage` 显示实际模式。整个冻结范围均在管理员缓存配置中时使用 persistent；关闭缓存或范围包含未选目录时使用 memory。已配置的持久范围发生存储故障时返回固定游标错误，已提交记录供故障恢复后重试；已确认的目录世代变化使旧 token 失效。持久提交仍使用本页总期限，并为提交预留至多 1 秒。响应预算检查计入 cursor_storage 及下一 token。

## 恢复与后续一致性

正常重启和异常退出保留可在线复核的会话检查点。普通重启也保留已提交的同步检查点和工作缺口。管理员执行备份恢复时，通过 `rust-email-mcp --cache-restored` 清除旧游标、同步检查点与同步任务，并将缓存范围标为待核对；后台重新发现远端范围并复用校验通过的已有内容。操作步骤见[部署规程](deployment.md#持久缓存备份与恢复)；恢复操作库遵循原发送记录规程。数据库损坏、未知 schema 或迁移失败提供可重建的缓存故障状态，避免更改发送操作结果。

R11 的离线内容通过独立运行见证、未清洁运行标记和所有既有写路径的 dirty scope 登记保护。完整只读核验以修订 CAS 清除 dirty，已知 tombstone、失效世代及撤权立即阻止交付。完整 raw、正文与附件分别报告 materialized、derivable_from_raw、missing、omitted 或 unsupported，索引报告已完整保存的覆盖；R12 全量扫描与失败队列收敛后更新对应历史范围的完成标志。

## 验证入口

存储测试使用隔离目录和合成 SQLite 数据，覆盖迁移、损坏、锁与路径、主库及日志配额、中文索引、双账户同 UID、世代切换和清理。游标测试覆盖恶意长度/计数/索引、事务前后故障、取消、重启、重放、TTL 和授权绑定。签名 HTTP 回归覆盖默认关闭、显式开启、当前 ACL、目录世代、状态输出及缓存故障后的在线结果。所有附件继续使用本地合成数据验收。
