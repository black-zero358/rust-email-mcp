# 隔离附件交付探针

本目录只处理确定性的合成数据，不读取邮件、邮箱凭据或生产配置。Python 是开发验收工具；产品运行时继续使用 Rust。

在仓库根目录执行：

```powershell
uv run --project scripts/attachment-delivery-probe --locked scripts/attachment-delivery-probe/probe.py selftest --report scripts/attachment-delivery-probe/generated/selftest.json
uv run --project scripts/attachment-delivery-probe --locked scripts/attachment-delivery-probe/probe.py generate --report scripts/attachment-delivery-probe/generated/manifest.json
```

`selftest` 在本进程内生成一次性凭据，启动随机端口 loopback 服务，完成 MCP 工具发现、Bearer GET、`resource_link` / `resources/read`、Base64 边界、文件落盘、SHA-256 与内容解析，随后关闭服务。失败输出仅含异常类型，调试时在本地断点查看；避免将含 URL 或请求头的异常直接写入日志。

`generated/` 包含八组原文件及按 fixture id 隔离的下载副本。同名附件保留原名，使用不同 fixture id 目录避免覆盖。CSV/XLSX/PDF 生成器均使用 Python 标准库；独立解析器负责确认生成文件可被常规工具打开，版本及下载摘要在 uv.lock。

| 开发依赖 | 固定版本/许可证 | 用途与替代选择 |
| --- | --- | --- |
| openpyxl | 3.1.5 / MIT | 独立打开 XLSX 并按工作表取数；标准库 ZIP/XML 仅能核对封装与 XML，选择 openpyxl 补实际文件解析 |
| et-xmlfile | 2.0.0 / MIT | openpyxl 的传递依赖，随独立 uv.lock 固定 |
| pypdf | 6.21.4 / BSD-3-Clause | 独立打开 PDF 并提取验证段落；标准库逐字节匹配只核对生成格式，选择 pypdf 补常规阅读器解析 |

这些包只进入此独立 uv 环境，不加入 Cargo、镜像或线上依赖。fixture 无公式、宏、外部链接或第三方文档内容。

分进程测试时，通过安全的进程环境提供变量，命令行只包含变量名称：

```powershell
# 由调用者的凭据管理器注入 PROBE_BEARER_TOKEN，可另注入 PROBE_OTHER_BEARER_TOKEN。
uv run --project scripts/attachment-delivery-probe --locked scripts/attachment-delivery-probe/probe.py serve

# 另一终端继承同一 PROBE_BEARER_TOKEN；PROBE_BASE_URL 设为 http://127.0.0.1:18767。
uv run --project scripts/attachment-delivery-probe --locked scripts/attachment-delivery-probe/probe.py client --report scripts/attachment-delivery-probe/generated/client.json
```

服务仅监听 `127.0.0.1`。远程客户端实验需单独配置隔离 HTTPS 反向代理或平台支持的安全隧道，并用 `serve --public-base-url https://probe.example.test` 设置返回的非保密 origin。探针不提供 OAuth 登录、发现或刷新；使用独立的实验凭据，目标宿主若要求 OAuth 则由隔离认证组件实现。报告不记录凭据、下载 URL、句柄或真实数据；访问日志关闭。客户端拒绝重定向且只向配置 origin 发送凭据。

HTTP MCP 使用 2025-06-18/2025-11-25 的 JSON 响应子集，支持 initialize、initialized 通知、ping、tools/list、tools/call、resources/list、resources/read。GET `/mcp` 返回 405，POST 通知返回 202，无会话及 SSE。JSON 请求 64 KiB、完整响应 2 MiB、序列化 id 1024 字节；句柄 600 秒、64 个。该探针用于功能验收，慢客户端/并发/内存及产品 OIDC 验收沿用项目单独流程。

详细结论、能力矩阵、适配归属与目标客户端验收步骤见 [attachment-delivery-probe.md](../../docs/attachment-delivery-probe.md)。
