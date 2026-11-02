# 附件离线下载与读取工具

本阶段只对本地模拟邮箱和合成文件测试下载与直接读取。全部 8 组合成附件已通过真实 Rust 服务的离线联调；不连接真实 ChatGPT、不部署、不登录、不写邮件。产品保持 Rust 运行时。

```powershell
pnpm --dir scripts/attachment-client-adapter install --frozen-lockfile --ignore-scripts
pnpm --dir scripts/attachment-client-adapter test
```

`standard-client.mjs` 可直接由 Node 22+ 运行，无 npm 运行依赖；Node 24.15.0 已完成本地验收。它通过自己的 Bearer 会话使用现有 R04 prepare/GET 接口，支持 JSON 与增量 SSE（找到对应响应后立即取消流），限制同 origin 与 32 MiB 文件，并核对 `X-Content-SHA256`、`Content-Digest`、可用的 exact size 和调用者提供的预期摘要。落盘使用传输隔离目录、UTF-8 长度受限的安全名称与临时文件，取消或准备失败时清理并释放 HTTP 响应流。

所有连接和凭据仅通过进程环境提供：

```powershell
# 由安全的会话/凭据管理器设置；不要把令牌写进命令行、URL 或报告。
# ADAPTER_MCP_URL: https://isolated.example.test/mcp
# ADAPTER_BEARER_TOKEN: 独立客户端已授权会话的 access token
# ADAPTER_REQUEST_JSON: {"mailbox":"INBOX","uid":7,"uid_validity":9,"attachment_index":1}
# ADAPTER_DESTINATION: 调用者拥有的目标目录
# ADAPTER_EXPECTED_SHA256: 可选，合成 fixture 预期 SHA-256
# ADAPTER_REPORT: 可选，私有结果 JSON 路径；包含交付给本地消费者的 path
pnpm --dir scripts/attachment-client-adapter exec node standard-client.mjs
```

CLI 成功后返回 `local_file_ready`，文件可交给本地解析器直接读取。标准输出仅状态/长度/摘要；文件位置通过显式私有报告或模块 API 返回。客户端不自动刷新登录。

`ui-candidate.mjs` 是后续接口研究的 mock 原型，本阶段不实施。`read_attachment_delivery_chunk` 是候选工具名，当前产品没有这个工具；相关测试仅检查模拟字节和官方接口形状。真实连接测试后移，原型不自动发送消息。

开发测试的唯一直接依赖是 MIT 的 `@modelcontextprotocol/ext-apps` 2.0.4，用官方生成 schema 验证资源消息/上下文形状。传递依赖由独立 pnpm-lock.yaml 固定；只安装到本目录忽略的 node_modules，不进入 Cargo/镜像。标准客户端本身仅使用 Node 内置模块；schema 测试采用 SDK 作为独立验证器。

离线验证结果及后续接口研究见 [设计文档](../../docs/attachment-client-adapter-design.md)。
