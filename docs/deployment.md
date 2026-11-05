# 单节点部署

以下示例使用 Linux amd64、Docker Compose、已有 HTTPS Nginx，域名为 `mail.example.com`，MCP 地址为 `https://mail.example.com/email/mcp`。替换为自己的域名。Rauthy 的固定镜像摘要对应 amd64。

部署前按 [维护与发布规范](maintenance.md) 确认目标版本、commit、产物校验和、兼容性与验收证据。源码发布与生产部署分别执行；本指南不授予真实邮件写入测试权限。

## 准备目录与配置

```sh
mkdir -p secrets/bootstrap data/auth data/mail
cp deploy/rauthy.toml.example secrets/rauthy.toml
cp .env.example secrets/mail.env
uv run deploy/hash_password.py
```

将生成的密码哈希填入 `password_argon2id`。用 `openssl rand -hex 32` 分别生成两个集群密钥，用 `openssl rand -base64 32` 生成加密密钥。修改 `pub_url`、`rp_id`、`rp_origin` 和管理员邮箱。`rp_origin` 保留 `:443`。配置中的可信代理必须与 Compose 网桥一致。

在 `secrets/mail.env` 填入邮箱参数，并设置：

```dotenv
MCP_RESOURCE=https://mail.example.com/email/mcp
OIDC_ISSUER=https://mail.example.com/auth/v1/
OIDC_AUDIENCE=https://mail.example.com/email/mcp
OIDC_SUBJECTS=REPLACE_WITH_USER_ID
BIND=0.0.0.0:8080
DATABASE_PATH=/data/operations.sqlite
MAIL_MODE=read
```

目录权限：

```sh
sudo chown -R 10001:10001 data/auth secrets/bootstrap
sudo chown 10001:10001 secrets/rauthy.toml
sudo chown -R 1000:1000 data/mail
sudo chmod 700 data/auth data/mail secrets/bootstrap
sudo chmod 600 secrets/rauthy.toml secrets/mail.env
```

附件 HTTP 下载使用专属暂存目录。`ATTACHMENT_STAGING_DIR` 可显式指定；默认位于 `DATABASE_PATH` 的父目录下，名称为 `mail-downloads`，上述 Compose 配置对应 `/data/mail-downloads`。目录由服务用户独占，权限为 0700，文件为 0600；四份下载文件合计最多 128 MiB。目录用于下载过程中的临时文件，客户端消费或取消后清理，重启时在取得目录锁后清理可识别的残留文件。备份时排除此目录。

使用 `DATABASE_PATH=:memory:` 且需要 HTTP 附件下载时，显式配置 `ATTACHMENT_STAGING_DIR`。暂存目录初始化失败时，普通邮件读取和内联附件下载继续可用；准备 HTTP 下载返回 `ATTACHMENT_STORAGE_UNAVAILABLE`。检查目录权限、可用空间和是否被另一个服务进程占用，修正后重启服务。运行中写入或清理失败同样停止相关下载，日志仅记录固定错误类别。

暂存路径可以是绝对路径或相对进程工作目录的路径，允许 `.`，拒绝空值和 `..`。父目录须已存在，服务只创建最后一级目录；已有目录需满足私密权限要求。路径中的符号链接、非目录和目录锁冲突均使 HTTP 下载不可用，服务保留原有目录权限。

## 多账户配置

保留原环境配置即可继续使用单邮箱。追加只读邮箱时，复制 [accounts.json.example](../deploy/accounts.json.example) 到私密配置目录，填写稳定登记键、调用别名、邮箱端点、密码环境变量名称和各账户的主体 ACL。`extend_legacy` 保留原邮箱环境配置；JSON 中的 `legacy_default` 显式列出原邮箱的读写主体。全局 `OIDC_SUBJECTS` 与每账户 ACL 取交集，新加入全局列表的主体按各账户 ACL 获取权限。

在 `secrets/mail.env` 设置 `ACCOUNT_CONFIG_PATH=/run/mail-config/accounts.json`，并填写示例引用的 `FINANCE_MAIL_PASSWORD`。密码值保存在环境文件，JSON 只保存引用。将 JSON 单独只读挂载给 mail 服务；例如在 `deploy/compose.accounts.yaml` 写入：

```yaml
services:
  mail:
    volumes:
      - ../secrets/accounts.json:/run/mail-config/accounts.json:ro
```

组合配置使用 `docker compose -f deploy/compose.yaml -f deploy/compose.accounts.yaml`。原 `/data` 挂载继续保留。JSON 文件只允许服务用户及管理员读取；以当前 Compose 的 UID 1000 为例，设置所有者 1000:1000、权限 0600。配置修改后重启 mail 服务生效。

文件密码引用适用于 Linux：设置 `MAIL_SECRET_ROOT=/run/mail-secrets`，将秘密目录只读挂载至该路径，JSON 使用 `{ "file": "finance.secret" }`。根目录及路径上的子目录禁止组/其他用户写入，密码文件设为 0600，所有者为服务用户；禁止符号链接、绝对路径和父目录跳转。Windows 开发环境使用环境变量引用。配置文件最多 128 KiB、当前账户最多 16 个；完整的 `file_only` 配置及校验规则见[多账户设计](multi-account-design.md)。

`default_account_id` 只控制省略账户参数时的读取目标。旧写工具始终操作原 `default` 邮箱；新增账户固定只读。更换调用别名时保留 `registration_key`，使内部账户身份延续；追加邮箱的登录身份/端点变化或删除后重新加入会递增世代并使旧附件引用失效。原邮箱身份与操作库路径保持固定。

如需在跨目录会话的标准范围内搜索归档邮件，请按账户填写准确的 IMAP 目录名。纯环境配置和 `extend_legacy` 的原邮箱使用 `ARCHIVE_MAILBOX`，JSON 定义的邮箱使用 `folders.archive`。若无需归档目录，移除相应配置项。`file_only` 须移除环境文件中的 `ARCHIVE_MAILBOX`。标准范围包含 INBOX、服务端确认的已发送目录和所配置的归档目录，并排除 All、垃圾邮件、废纸篓、草稿和不可选择的目录。配置方式见[多账户设计](multi-account-design.md#配置与旧调用)。

首次启用显式账户配置会将账户注册库升级到 v2；升级和回退先按[备份与恢复](#备份与恢复)处理。验证采用独立目录、本地模拟邮箱和合成数据，附件继续只做离线测试。

## 配置认证

将 `deploy/nginx.conf.example` 放入自己的 HTTPS server 块，执行 `nginx -t` 后 reload。公网只开放 Nginx 的 HTTPS 端口，容器端口绑定 loopback。按部署环境对登录和 API 路径配置速率限制。

登录限流只应用于认证接口。为 `/auth/v1/_app/`、`/auth/v1/assets/`、`/auth/v1/theme/` 设置更具体的代理 location，保持静态资源独立于登录限流。登录页一次会加载数十个 JavaScript/CSS 文件；上线验收需检查这些资源全部返回成功，防止资源 429 导致前端显示 500。

```sh
docker compose -f deploy/compose.yaml up -d auth
```

打开 `https://mail.example.com/auth/v1/`，用 bootstrap 管理员登录。创建专用邮箱用户，并将其 ID 填入 `OIDC_SUBJECTS`。创建 `email:mcp` scope 和 `email-mcp` 客户端：

| 设置 | 值 |
| --- | --- |
| 类型 | Public，PKCE S256 |
| Flows | authorization_code、refresh_token |
| Redirect URI | 当前 ChatGPT 设置界面显示的精确回调 URL |
| Scopes / default scopes | openid、email:mcp |
| Access token algorithm | RS256 |
| Access token lifetime | 300 秒 |
| Allowed resources / default audience | `https://mail.example.com/email/mcp` |

回调地址以 ChatGPT 当前创建页面显示的值为准，精确复制。Rauthy 0.37.2 支持公共 PKCE 客户端，但默认发现文档遗漏 `none`。创建连接前导出修正后的发现文档：

```sh
uv run deploy/prepare_discovery.py https://mail.example.com/auth/v1/.well-known/openid-configuration /opt/rust-email-mcp/oidc-discovery.json
sudo install -m 644 /opt/rust-email-mcp/oidc-discovery.json /var/lib/nginx/email-mcp-oidc.json
```

在 HTTPS Nginx server 块添加精确路由并 reload：

```nginx
location = /auth/v1/.well-known/openid-configuration {
    alias /var/lib/nginx/email-mcp-oidc.json;
    default_type application/json;
    add_header Cache-Control "no-store";
}
```

公开发现文档单独放在 Nginx 可读取的目录，保留服务目录权限。升级 Rauthy 或修改域名、端点、scope 后，暂时移除精确路由，从原服务重新导出发现文档再恢复路由。ChatGPT 的 Token 端点认证方式选择 `none`，客户端密钥与注册 URL 留空；默认 scope 为 `email:mcp`，基础 scope 为 `openid`。此连接可关闭可选的 OIDC 邮箱身份功能。

Rauthy 0.37.2 的 bootstrap client JSON 未包含资源受众字段。通过管理界面，或管理 API 的 `GET /auth/v1/clients/email-mcp` 和 `PUT /auth/v1/clients/email-mcp` 设置 `allowed_resources`、`default_aud` 数组，PUT 时保留其他客户端配置。创建短期 API Key 时仅授予 Clients read/update 权限。

启用邮件服务：

```sh
docker compose -f deploy/compose.yaml up -d --build mail
```

在 ChatGPT 中配置 OAuth MCP，客户端 ID 为 `email-mcp`。先完成只读验收，再按需要将 `MAIL_MODE` 改为 `draft`、`send` 或 `full` 并重建邮件容器。

## 验收与回滚

依次记录以下验收结果：

1. 公网健康检查、资源发现、OIDC 发现及 JWKS 可访问，发现文档中的 URL、scope 和认证方式正确。
2. 在浏览器打开授权流程，确认登录界面实际渲染；检查 JavaScript、CSS 和主题资源的突发加载均成功。
3. 检查 Nginx 限流日志与 Fail2ban 状态。修正静态资源限流后，已有封禁仍需单独核实和处理，见[接入排障](troubleshooting.md)。
4. 用隔离测试身份完成授权、换令牌、刷新及重启恢复，再检查邮件只读工具。
5. 用户在 ChatGPT 完成授权和工具调用，随后确认访问令牌到期后的自然刷新。分别记录服务端测试与 ChatGPT 实测结果。

用独立测试用户和客户端检查两次登录各自刷新、提前刷新、重启后刷新、工具发现、列表、正文和读前后标记。真实写操作通过专用测试邮箱验收，收件人与操作范围提前确认。

将新服务先部署到独立端口，使用隔离数据库或按下述方法取得的备份副本，以 `MAIL_MODE=read` 验收。不要让新旧邮件进程同时打开同一操作数据库。保留旧镜像、配置和数据；正式切换时先停止旧邮件进程，再让新版本接管最新的操作数据库并切换 Nginx。旧服务停止后再记录总内存。

回滚应用时，先确认旧程序兼容当前数据库格式，保留最新操作记录，再恢复旧镜像、端口映射与对应认证发现地址；客户端按需要重新授权。若数据库已迁移且旧程序不兼容，先暂停写入并按该版本的迁移/恢复说明处理，不能直接把旧备份覆盖到当前操作库。

回滚到尚未识别 `folders.archive` 的程序时，从账户 JSON 移除此字段或恢复该版本兼容的账户配置。归档配置本身沿用现有账户注册库和操作库格式。

邮件容器设置 45 秒停止宽限期，给正在执行的 SMTP 操作留出持久化结果的时间。进程异常退出后的 `sending` 记录在下次启动转为 `unknown`。

## 备份与恢复

R02 起同时保留 `accounts.sqlite`（或 `ACCOUNT_REGISTRY_PATH` 指定文件）与原操作数据库。注册库 v1 保存 default UUID、世代以及 IMAP 身份和操作库路径的摘要；首次升级在恢复操作记录前建立/核验注册库。Linux 新建目录为 0700、注册库和初始化锁为 0600，既有目录权限由管理员管理。固定 `.staging` 文件在初始化锁内恢复清理；备份正式注册库，保留其文件权限。

R05 显式账户配置使用 v2。从 v1 迁移时保留原 UUID、世代及绑定，并保存完整文件名追加 `.v1-backup` 的副本；替换已有正式库时保存 `.previous` 副本。全新直接建立 v2 时尚无这两份历史副本。同一次备份保存注册库、已有副本、最新操作库及对应账户配置，秘密单独加密保存。多账户运行期持有注册锁，恢复或替换注册库前停止 mail 服务。

启动失败先修正配置、秘密文件权限或路径冲突，再重试完整启动。若正式注册库损坏，停止服务并保留现场，使用配套完整副本恢复，核对相同原邮箱身份与操作库路径。回退至只支持 v1 的程序时，恢复匹配的 `.v1-backup`，使用原环境配置并保留最新操作记录库；追加账户在旧程序中暂停提供。再次升级使用保存的 v2 配置和注册库，使追加账户的 UUID、世代与删除记录延续。v2 注册库要求继续提供 `ACCOUNT_CONFIG_PATH`。

恢复演练可使用隔离容器和独立宿主目录，保持容器内相同的操作库规范路径（例如 `/data/operations.sqlite`）及同一邮箱绑定。改用其他实际邮箱或操作库路径会触发固定迁移错误；先恢复正确的配置和配套注册库，再启动服务。密码轮换及 SMTP 配置调整保持账户 UUID。回滚到旧程序时保留新注册库，供再次升级复用身份。

备份 Hiqlite 时停止认证服务后复制整个数据目录；邮件 SQLite 使用在线 backup API，或停止邮件服务后复制完整目录。加密密钥和配置单独加密备份。记录备份时间、应用版本/commit 和数据库格式说明，定期在隔离数据目录及独立端口演练恢复，先以只读模式验证。

跨目录会话结果的 `cursor_storage` 标明存储方式。memory 游标在进程重启后从种子重新查询；persistent 游标在原 10 分钟期限内可跨正常重启续查，并在线复核冻结目录。若收到 `STALE_UID`，先列出种子目录，取得当前邮件引用再查询。缓存备份恢复按下节清理旧游标。

应用版本回滚与操作数据库回退必须分别处理。恢复旧操作库会丢失备份之后的发送记录及对应去重信息；这段时间的发送需要结合邮件服务商记录核查，原 `operation_key` 缺失时也按此步骤处理。

若必须从旧备份恢复，先暂停邮件写操作，保留可用的最新数据库及故障现场副本，核对备份后可能执行的发送与邮件服务商记录。无法确认的结果按 `unknown` 处理，不自动重试，也不通过换用新操作键绕过检查。只有在受影响操作已核验、操作记录恢复方案明确后，才恢复已获授权的写模式；当前项目没有自动重建丢失发送记录的工具。

附件扩展路由：公开 `MCP_RESOURCE/attachments/{token}` 应转发至内部 `/mcp/attachments/{token}`。示例 `/email/` 前缀代理覆盖此后缀；自定义仅匹配 MCP 根路径的代理需同步覆盖，保留 Bearer 头、认证中间件，并对含句柄路径关闭访问日志。当前附件验收仅使用本地模拟服务，真实 ChatGPT 下载与读取测试后移。

## 持久缓存配置

复制[缓存配置样例](../deploy/cache.json.example)，设置 `CACHE_CONFIG_PATH`，按实际账户别名和 IMAP 名称选择范围。示例 `/data/cache.json` 中的相对 directory `cache` 保存到 `/data/cache`。配置和账户/目录校验发生在 Journal 恢复之前。省略 `CACHE_CONFIG_PATH` 或在 JSON 中显式设置 `"mode":"off"` 时不创建缓存目录。metadata 保存在线读取证据和选中范围的持久会话游标。

缓存使用服务运行用户独占的专用目录，新目录与分区权限为 0700，数据库、根清单和锁文件为 0600。选择空的新目录；已有目录须带有本服务校验通过的清单和 schema。缓存路径与操作库、账户注册库、附件暂存目录分别设置，拒绝符号链接及路径重叠。数据库、事务日志、根清单和旧分区均计入磁盘额度；较大的配置额度还应配合文件系统可用空间和磁盘监控。

缓存内容包含邮件元数据和会话关系，使用受限备份访问和磁盘加密。目录/存储故障会在 `get_cache_status` 报 unavailable，普通在线读取继续返回其实际结果。修正权限、路径或额度后重启服务，检查 ready 及所选目录。遇到损坏或未知 schema 时，保留故障副本，改用新的私有缓存目录重建在线观察；操作库和账户注册库继续使用原文件。

`write_health.quota_blocked:true` 表示当前进程遇到过额度不足；`failed_writes` 与固定 `last_error` 也记录队列满或存储失败。主库写入同时需要为回滚日志预留空间，额度安排应包含这部分余量。调整 JSON 中的全局/账户额度后重启并重新读取所需邮件，核对写入计数及覆盖变化。

## 持久缓存备份与恢复

停止邮件服务后保存完整缓存根目录、所有账户分区和缓存 JSON，保留所有者与权限；账户注册库和操作库按前述规程分别备份。全根快照保留统一的运行/恢复标记，避免混用不同时间的分区副本。

恢复缓存副本后，在普通服务启动前，使用同一组账户环境与 `CACHE_CONFIG_PATH` 执行一次：

```sh
rust-email-mcp --cache-restored
```

使用仓库 Compose 部署时，先停止 mail 服务，再沿用其环境、用户和 `/data` 卷执行。配置了账户或挂载覆盖文件的部署，继续传入平时使用的 `-f` 文件。

```sh
docker compose -f deploy/compose.yaml run --rm --no-deps mail --cache-restored
```

该命令在打开 Journal 前锁定缓存，递增恢复世代，清除持久游标并将目录标为 dirty，随后退出；不连接邮箱。暂未启用的旧分区在以后重新纳入配置时应用同一恢复世代。普通启动继续使用 `rust-email-mcp`。恢复后从种子新建会话，已有缓存范围保持待核对，后续只读存在性扫描负责清除 dirty。

正常重启保留未到期的持久游标；异常退出留下未清洁标记，目录进入待核对状态，游标仍需在线核对每个冻结目录。单次 EXAMINE 更新目录世代，范围内每个已缓存 UID 的存在性由后续扫描核对。关闭缓存或回滚到旧程序时移除 `CACHE_CONFIG_PATH`，保留缓存目录供后续恢复；操作记录继续按原发送规程保留。
