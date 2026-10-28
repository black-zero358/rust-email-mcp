# 单节点部署

以下示例使用 Linux amd64、Docker Compose、已有 HTTPS Nginx，域名为 `mail.example.com`，MCP 地址为 `https://mail.example.com/email/mcp`。替换为自己的域名。Rauthy 的固定镜像摘要对应 amd64。

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
| Redirect URI | `https://chatgpt.com/connector_platform_oauth_redirect` |
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

用独立测试用户和客户端检查两次登录各自刷新、提前刷新、重启后刷新、工具发现、列表、正文和读前后标记。真实写操作通过专用测试邮箱验收，收件人与操作范围提前确认。

将新服务先部署到独立端口；验收后切换 Nginx。保留旧镜像、配置和数据，旧服务停止后再记录总内存。回滚恢复旧端口映射与旧认证发现地址，客户端重新授权。

邮件容器设置 45 秒停止宽限期，给正在执行的 SMTP 操作留出持久化结果的时间。进程异常退出后的 `sending` 记录在下次启动转为 `unknown`。

备份 Hiqlite 时停止认证服务后复制整个数据目录；邮件 SQLite 使用在线 backup API，或停止邮件服务后复制完整目录。加密密钥和配置单独加密备份。定期在独立端口演练恢复。
