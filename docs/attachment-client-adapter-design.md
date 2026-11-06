# R04 附件离线下载与直接读取

验证日期：2026-11-02。本阶段只用本地模拟邮箱和合成文件，完成下载与内容读取。全部 8 组合成附件已通过 Rust 服务与标准客户端的联调，7 组本地测试通过。

本阶段完成条件是：文件完整下载、名称正确、同名文件分别保留、长度与 SHA-256 一致，并能直接读取合成表格和 PDF 的已知内容。外部连接测试后移，本阶段不连接真实 ChatGPT、不登录、不部署。

下载测试使用 [标准客户端](../scripts/attachment-client-adapter/README.md)。表格验证金额 194、数量 6，两份同名 CSV 分别为 11/29，PDF 验证段落为 CEDAR-729。详细联调证据见下方“离线验证结果”；接口研究仅作后续参考。

## 后续研究附录：已核对的接口（本阶段不实施）

| 层 | 官方接口与证据 | 当前判定 |
| --- | --- | --- |
| 宿主文件注册 | Plugins reference 的 `window.openai.uploadFile(file, { library?: boolean })` 返回 `{fileId}` | 参数为 File，文档用途为用户选择的文件；程序构造 File 的接收行为进入目标宿主实验 |
| 后续消息 | `window.openai.sendFollowUpMessage({prompt,scrollToBottom?})`；官方 examples 的 `src/types.ts` 同形状 | 文档与示例类型无通用 fileIds 参数 |
| Widget state | `setWidgetState({modelContent,privateContent,imageIds})` | 官方明确 imageIds 用于后续图像上下文；通用 XLSX/PDF/CSV 附件路径待核验 |
| UI 工具调用 | `App.callServerTool({name,arguments})` | 由宿主转发到原 MCP server；UI 不接触连接器内部令牌 |
| 私有工具输出 | 工具结果 `_meta` | 官方定位为组件可见、模型隐藏；适合受限 chunk 数据，需目标宿主保留原 envelope |
| 标准资源 | `resource_link` / `resources/read` | 可供标准客户端取字节；目标 ChatGPT 的分析环境接收需独立验收 |

以下接口只保留研究结果，当前不安排真实连接测试。

## 后续研究附录：候选实现

本地原型首先实现可运行的标准客户端：使用自己的 Bearer 会话调用现有 `prepare_attachment_download`，限制同 origin、拒绝重定向，最多读取 32 MiB，边读边计算 SHA-256，写入受控临时文件，再原子提升为按传输随机 id 隔离的文件。该结果记为 `local_file_ready`，文件消费者由运行客户端的宿主提供。

UI 方向保留两阶段候选，先检测 `app.getHostCapabilities()?.serverTools`。拟新增的 app-only chunk 工具只在此设计中命名，当前产品没有这些工具：

```js
app.callServerTool({
  name: "read_attachment_delivery_chunk",
  arguments: { transfer_id, offset, max_bytes: 65536 },
}, { signal });
// result.structuredContent: 小型状态/元数据
// result._meta.attachment_chunk: 长度受限的 Base64 与完整绑定信息
```

服务端先完成一次完整 MIME 解码、tagged OK、字节数和 SHA-256 核验，再从不可变的临时快照发分块；每次请求重新检查 principal、AccountKey/配置世代、附件引用和有效期。UI 不读取、不保存连接器令牌。服务端工具使用 `_meta.ui.visibility:["app"]`，所有分块仍受实际完整 JSON-RPC 响应 2 MiB 上限。暂定每块 64 KiB、最多 512 块；传输 32 MiB、4 路交付、64 句柄、600 秒租约，临时文件计入独立配额。

UI 逐块核对 transfer/account/reference/generation/offset/总长和 digest，将字节填入一次预分配的 `Uint8Array`。完成后用 Web Crypto 检查完整 SHA-256，再构造浏览器 `File`。宿主 `uploadFile` 若存在，可以作为明确的隔离实验步骤；成功返回仅记 `host_file_registered`。程序生成 File 的接收和上传后文件进入通用分析环境是两个独立目标实验。

ext-apps v2.0.4 的标准接口提供另一条上下文候选：

```js
const caps = app.getHostCapabilities();
// 仅宿主明确声明对应 modality 时使用：
if (caps?.updateModelContext?.resourceLink) {
  await app.updateModelContext({ content: [{
    type: "resource_link", uri: attachment_resource_uri,
    name: safe_filename, mimeType, size: decoded_size,
  }] });
}
```

该接口接受标准资源引用；接收确认记为 `context_acknowledged`。由目标宿主将资源下载到分析环境，并实际计算合成文件内容后，才记 `analysis_ready`。`App.sendMessage` 同样接受 `ContentBlock[]`，调用需要明确用户动作；本轮原型不自动发送聊天消息。`ui/download-file` 用于用户设备下载，单独呈现为手动路径。

宿主 `imageIds` 路径只用于官方已描述的图像，不把 XLSX/CSV/PDF id 填进它。OpenAI 兼容 `sendFollowUpMessage` 的公开签名仍是 prompt/scrollToBottom；不添加未公布的 fileIds 属性。任一必要能力缺失时返回 `client_delivery_required`、已完成阶段和准确下一步。

## 后续研究附录：生命周期与失败语义

- 绑定：服务端主体来自每次已验证 MCP 请求，参数中的账户/附件/transfer id 只作引用。用户切换账户、配置世代变化或撤权后停止交付；旧引用失效。
- 验证：声明类型和实际字节分别记录；文件名经清理，保存名按传输隔离；总长、逐块 offset、块大小和最终 SHA-256 全部通过后才能注册/交给消费者。
- 取消：AbortSignal 取消未完成工具/HTTP 读取，释放 UI 缓冲区和客户端临时文件；服务器租约在取消/过期后回收。宿主 uploadFile 没有公开取消/删除参数，上传开始后的未知结果保留待核对状态，避免自动重复注册。
- 重试：只读块最多有限次重试同 offset；引用、总长、digest 任一变化就停止。过期需重新准备原引用并完整重取。宿主上传结果未知时不自动重试。
- 撤销：服务器句柄撤销作用于后续读取；已交给宿主的副本按宿主文件生命周期处理。目标验收需记录其删除/撤销入口；原型不假设存在通用 deleteFile API。
- 日志：只保存固定阶段、状态、合成 digest 和计数；无认证头、完整 URL、句柄、真实文件内容或地址。原型不引入匿名或签名 URL。

## 固定来源

- [OpenAI Plugins reference / File APIs](https://developers.openai.com/plugins/reference#file-apis) 与 [ChatGPT UI / Make images visible](https://developers.openai.com/plugins/build/chatgpt-ui)：文件辅助接口、私有 `_meta`、imageIds。
- [官方 examples 的类型](https://github.com/openai/openai-apps-sdk-examples/blob/main/src/types.ts)：sendFollowUpMessage 的公开样例签名，仅用于交叉核对。
- [ext-apps v2.0.4 spec.types.ts](https://github.com/modelcontextprotocol/ext-apps/blob/v2.0.4/src/spec.types.ts)：McpUiMessageRequest、McpUiUpdateModelContextRequest、McpUiHostCapabilities、McpUiSupportedContentBlockModalities。
- [ext-apps v2.0.4 app.ts](https://github.com/modelcontextprotocol/ext-apps/blob/v2.0.4/src/app.ts)：callServerTool、readServerResource、sendMessage、updateModelContext 的实际实现。
- [ext-apps 迁移说明](https://github.com/modelcontextprotocol/ext-apps/blob/v2.0.4/docs/migrate_from_openai_apps.md#file-handling)：标准桥尚无 uploadFile/getFileDownloadUrl 对应方法。

公开源码下载副本放在原型目录忽略的 `generated/research/`，供后续接口研究参考。

## 离线验证结果

[可运行原型](../scripts/attachment-client-adapter/README.md) 分为标准客户端和 UI mock 候选。标准客户端使用当前 R04 的完整 descriptor：prepare 的 `checksum` 可为 null，Base64 的 decoded_size 可为 estimate，QP 可为 unknown；下载后读取 `X-Content-SHA256` 和 `Content-Digest`，与客户端边读边计算的 SHA-256 一起核对。只有 size_accuracy 为 exact 才把准备阶段长度作为精确比较值。成功结果保留实际 decoded_size、sha256 和核验来源。

本地 Node 24.15.0 / pnpm 12.15.0 的 7 组受控测试通过，覆盖：

- 标准 MCP JSON、持续开启的 SSE 及时返回，认证 GET，CSV 合计 194，同名文件不覆盖。
- HTTP 两种摘要与实际文件比较；摘要错误、长度不符、到期、跨 origin、重定向、404、缺失摘要全部退出。
- 32 MiB 完整边界、声明超限预拒绝、无 Content-Length 的实际流超限、下载中取消；失败文件清理。
- GET 返回后磁盘准备失败也取消 body；本地服务确认连接释放，既有文件保留。
- UI mock 的 3 个私有分块、账户/世代/offset/编码/总长/摘要核对、超限、取消与到期。
- 后续接口研究中的程序生成 File 通过 mock 接口读取，返回模拟引用；这项 mock 测试与本阶段的下载验收分别记录。
- ext-apps 2.0.4 的实际生成 schema 接受 `ui/message` 和 `ui/update-model-context` 的 resource_link 形状；宿主未声明 resourceLink capability 时返回 client_delivery_required。

[验证证据与公开资料摘要](../scripts/attachment-client-adapter/evidence/local-prototype-2026-11-02.json) 记录命令、版本、测试计数及公开下载文件 SHA-256。上述 7 组测试的 HTTP 侧使用本地合成 fixture 服务。

真实 Rust 服务也已完成跨运行时验收：[独立联调记录](../scripts/attachment-client-adapter/evidence/rust-integration-2026-11-02.json)，测试入口 [attachments.rs](https://github.com/black-zero358/rust-email-mcp/blob/42873c0f2ea9f8ff284fa044c4111d7cb97f8dc9/src/server/output_tests/attachments.rs) 的 `standalone_node_adapter_consumes_real_rust_mcp_and_authorized_get`。它启动隔离 TLS IMAP 与真实 JWT MCP/GET router，经 Node CLI 逐个准备、认证下载并落盘全部 8 组 R00 fixture，再由 Rust 从私有报告路径读回文件，独立核对原始长度与 SHA-256；XLSX/PDF/两份同名 CSV/524,289 B CSV 均通过。结果为 1 passed、0 failed，5.42 秒。

该测试为显式 ignored 开发验收，不给默认 Cargo 测试添加 Node 依赖。成功执行命令：

```powershell
wsl -d Ubuntu -- bash -lc 'cd "/mnt/d/UserData/Documents/ChatGPT/server MCP/rust-email-mcp" && ADAPTER_NODE_BINARY="/mnt/d/UserData/Documents/ChatGPT/server MCP/rust-email-mcp/scripts/attachment-client-adapter/generated/node-v24.15.0-linux-x64/bin/node" CARGO_TARGET_DIR=/home/oeic_fe/.cache/rust-email-mcp-target /home/oeic_fe/.cargo/bin/cargo +1.99.1 test --locked --lib server::output_tests::attachments::standalone_node_adapter_consumes_real_rust_mcp_and_authorized_get -- --ignored --exact'
```

Linux Node 24.15.0 只解压在忽略的 generated 目录，官方 SHASUMS256 校验通过；也可由 ADAPTER_NODE_BINARY 指定已有兼容 Node。临时 fixture 和下载文件由测试生命周期清理，输出仅保留合成断言摘要。

当前产品实现选择标准客户端适配。UI chunk、临时快照服务、产品资源接口保持设计候选；未来复用 R11 blob/租约机制后再实现。原型不向现有产品发送不存在的 chunk 工具调用。

## 后续工作（另行安排）

真实 ChatGPT 连接测试已后移。本阶段不申请测试连接或接口，也不启动外部验证。后续按实际需要检查客户端能否直接读取或下载文件，并继续使用同一组合成附件核对字节与内容。
