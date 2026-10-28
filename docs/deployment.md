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

邮件容器设置 45 秒停止宽限期，给正在执行的 SMTP 操作留出持久化结果的时间。进程异常退出后的 `sending` 记录在下次启动转为 `unknown`。

## 备份与恢复

备份 Hiqlite 时停止认证服务后复制整个数据目录；邮件 SQLite 使用在线 backup API，或停止邮件服务后复制完整目录。加密密钥和配置单独加密备份。记录备份时间、应用版本/commit 和数据库格式说明，定期在隔离数据目录及独立端口演练恢复，先以只读模式验证。

应用版本回滚与操作数据库回退必须分别处理。恢复旧操作库会丢失备份之后的发送记录，原 `operation_key` 查不到记录不代表邮件未发送，也不能继续保证这段时间的重复发送保护。

若必须从旧备份恢复，先暂停邮件写操作，保留可用的最新数据库及故障现场副本，核对备份后可能执行的发送与邮件服务商记录。无法确认的结果按 `unknown` 处理，不自动重试，也不通过换用新操作键绕过检查。只有在受影响操作已核验、操作记录恢复方案明确后，才恢复已获授权的写模式；当前项目没有自动重建丢失发送记录的工具。

附件扩展路由：公开 `MCP_RESOURCE/attachments/{token}` 应转发至内部 `/mcp/attachments/{token}`。示例 `/email/` 前缀代理覆盖此后缀；自定义仅匹配 MCP 根路径的代理需同步覆盖，保留 Bearer 头、认证中间件，并对含句柄路径关闭访问日志。此说明不代表实际客户端下载验收已完成。
