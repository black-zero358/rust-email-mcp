# ChatGPT 接入排障

按连接、页面资源、OAuth、邮件工具的顺序检查。记录故障发生时间及所在步骤，有助于关联 Nginx、认证服务和邮件服务日志。

## 创建连接时设置验证失败

先核对 [README 接入表](../README.md#认证与-chatgpt-接入)，尤其是：

- 使用预注册的公共客户端，客户端 ID 与 Rauthy 一致，密钥留空，Token 端点身份验证方法为 `none`。
- Redirect URI 精确复制 ChatGPT 创建页面显示的回调 URL。
- 注册 URL 留空。若表单实际填入 `https://example.com` 等示例地址，清除该值。DCR 需要真实可用的注册端点。
- 默认 scope 为 `email:mcp`，基础 scope 为 `openid`，两者均需允许该客户端请求。
- MCP 资源 URL、令牌 audience 和服务器配置一致，issuer 包括正确的尾部斜线。

Rauthy 0.37.2 默认发现文档遗漏公共客户端支持的 `none`。按[部署指南](deployment.md)修正公开元数据后，重新获取连接设置，检查页面显示的认证方式。升级身份服务后重新检查其原始发现文档，并更新公开副本。

发现文档返回 403 时，检查 Nginx worker 对文件及各级父目录的访问权限。将公开 JSON 放入 `/var/lib/nginx/` 等可读取目录，保持密钥和服务数据目录的私有权限。

## 登录页显示 500 / Internal Error

在浏览器开发者工具的 Network 面板区分授权请求和脚本、样式请求。授权 HTML 返回 200 后，还需确认页面引用的资源加载成功。

本次故障由静态资源 429 引起：登录页同时加载数十个文件，触发了覆盖整个 `/auth/` 路径的登录限流。为以下路径配置更具体的代理 location，并与登录限流分开：

- `/auth/v1/_app/`
- `/auth/v1/assets/`
- `/auth/v1/theme/`

执行 `nginx -t` 并 reload 后，重新打开登录页检查全部资源和实际渲染。同时检查 Fail2ban 是否留下了封禁记录。若授权请求本身返回 500，则按该请求时间检查认证服务日志。

本次分别使用 `email:mcp` 与 `openid email:mcp` 请求授权页均成功；排查该页面故障时应以响应及资源加载记录定位原因。

## ERR_CONNECTION_CLOSED

先比较客户端与服务器本机访问结果，检查 DNS、TLS、Nginx 监听和防火墙。服务器本机访问成功后，继续检查客户端到服务器的网络路径。

本次故障是静态资源限流触发 Fail2ban，客户端出口 IP 被 `nginx-limit-req` jail 封禁。修复 Nginx 配置后，该封禁仍然存在。服务器管理员可检查：

```sh
sudo fail2ban-client status
sudo fail2ban-client status nginx-limit-req
```

使用实际存在的 jail 名称。将被封禁地址与故障客户端出口 IP、事件时间及限流日志核对；确认是正常页面加载触发、且限流配置已修正后，解除该地址的封禁：

```sh
sudo fail2ban-client set nginx-limit-req unbanip <已核实的客户端出口IP>
```

随后从原客户端重新访问 HTTPS 和登录页面，保留认证接口限流及 Fail2ban 防护。若仍失败，继续依据抓包、TLS 错误及防火墙记录排查。

## 登录完成后工具返回 401

检查访问令牌的 issuer、audience、有效期、`email:mcp` scope，以及用户 subject 是否在 `OIDC_SUBJECTS` 中。以服务器错误码和 request ID 关联请求，避免把原始令牌写入日志或问题报告。

从旧 FastMCP/Dex 迁移后，重新授权以获得当前身份服务签发的令牌。刷新测试使用独立用户和客户端；旧 Dex 部署中，同用户、同上游客户端的测试登录曾替换已有刷新会话。验收分别记录即时刷新、重启后刷新和 ChatGPT 自然刷新结果。

## 问题报告

提供版本、发生时间及时区、失败步骤、HTTP 状态、错误码、request ID，以及去除查询参数的请求路径。浏览器截图遮盖账户信息；分享网络记录前移除 Cookie、Authorization、授权码、state、令牌和邮件内容。

相关官方资料：[ChatGPT 身份验证](https://developers.openai.com/plugins/build/auth)、[连接 ChatGPT](https://developers.openai.com/plugins/deploy/connect-chatgpt)、[Nginx 请求限流](https://nginx.org/en/docs/http/ngx_http_limit_req_module.html)。
