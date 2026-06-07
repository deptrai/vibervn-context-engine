# vibervn-context-engine

[English](README.md) · [Tiếng Việt](README-vi.md) · **中文**

## 安装与运行

通过 npx 直接运行最新发布版 —— 无需手动下载，npx 会自动获取适合你平台的预编译
二进制文件。`@latest` 标签会强制 npx 拉取最新发布版本，而不是复用过期的缓存：

```bash
npx vibervn-context-engine@latest
```

该命令会在 6699 端口启动 HTTP 服务器（Web 界面位于 http://127.0.0.1:6699，
MCP 端点位于 `/mcp`）。所有 CLI 参数都会转发给二进制文件：

```bash
npx vibervn-context-engine@latest --port 8080 --bind 0.0.0.0
```

或者全局安装以获得持久的 `vibervn-context-engine` 命令：

```bash
npm install -g vibervn-context-engine@latest
vibervn-context-engine --port 6699
```

支持的平台：Linux x64/arm64、macOS arm64、Windows x64。

## 功能特性

| 功能 | 描述 |
|------|------|
| 语义代码搜索 | 通过嵌入向量按语义查找代码，而非字面文本匹配 |
| 多语言解析 | 使用 Tree-sitter 为 Python、JavaScript、TypeScript、Rust、Go、Java、C 和 C++ 提取符号 |
| 调用图扩展 | 解析调用方/被调方边，并在查询时对匹配符号进行 BFS 扩展 |
| 增量索引 | 仅重新索引已变更的文件（mtime + 监听器），通过逐文件提交标记实现崩溃安全 |
| 实时文件监听 | `notify`（带去抖）在文件变更时自动触发重新索引 |
| Voyage AI 嵌入 | 带磁盘缓存的 HTTP 嵌入客户端，避免重复 API 调用 |
| LLM 重排序 | 使用 LLM（OpenAI / Google）对候选片段重新排序；可选，可禁用 |
| 内嵌 SurrealDB | 存储片段、符号和边；每个仓库一个数据存储 |
| HTTP API + Web 界面 | 配置管理、索引浏览器和查询测试控制台 |
| MCP 服务器 | 通过可流式 HTTP 暴露单个 `codebase-retrieval` 工具 |
| SSE 进度流 | 将实时索引进度事件流式传输到界面 |
| 大型仓库扩展 | 内存有界且无 O(n²) 路径 —— 为 Linux/Chromium 规模的代码库而构建 |

## 支持的语言

Tree-sitter 符号提取（函数、类、方法和调用边）按语言分别实现。文件扩展名在
`detect_language`（`src/parsing/mod.rs`）中映射。

| 语言 | 扩展名 | 语法 |
|------|--------|------|
| Python | `.py` | `tree-sitter-python` |
| JavaScript | `.js`、`.jsx`、`.mjs`、`.cjs` | `tree-sitter-javascript` |
| TypeScript | `.ts` | `tree-sitter-typescript` |
| TSX | `.tsx` | `tree-sitter-javascript` |
| Rust | `.rs` | `tree-sitter-rust` |
| Go | `.go` | `tree-sitter-go` |
| Java | `.java` | `tree-sitter-java` |
| C | `.c` | `tree-sitter-c` |
| C++ | `.cpp`、`.cc`、`.cxx`、`.h`、`.hpp`、`.hxx`、`.hh` | `tree-sitter-cpp` |

其他扩展名的文件仍会被分块并嵌入以供语义搜索，但不会从中提取符号或调用边。

## 工作原理

```mermaid
flowchart TD
    Boot([服务器启动]) --> Engine[IndexEngine 启动]
    Engine --> Watchers[为每个仓库生成文件监听器]
    Engine --> LoadVec[从 SurrealDB 加载向量索引]

    Watchers -->|文件变更 / 手动 / MCP| Trigger[索引触发]
    Trigger --> Detect[遍历仓库 + 检测变更文件]
    Detect --> Parse[解析: tree-sitter 符号、片段、原始边]
    Parse --> Embed[嵌入片段: Voyage AI + 磁盘缓存]
    Embed --> Store[将片段/符号存入 SurrealDB]
    Store --> Phase2[阶段 2: 将原始边解析为调用图]
    Phase2 --> VecIndex[(内存中的向量索引)]
    LoadVec --> VecIndex

    subgraph Clients
        WebUI[Web 界面]
        MCP[MCP 工具: codebase-retrieval]
    end

    Clients --> Q1[嵌入查询]
    Q1 --> Q2[向量搜索: top-k 余弦]
    Q2 -.读取.-> VecIndex
    Q2 --> Q3[图扩展: BFS 调用方/被调方]
    Q3 --> Q4[合并 + 去重相邻行范围]
    Q4 --> Q5[LLM 重排序]
    Q5 --> Q6[格式化: path#Lstart-end + 带行号的源码]
    Q6 --> Result([结果])
```
