# 编码助手设计整理

本文整理了我们关于 Git 强推风险，以及如何实现一个类似 Claude Code / OpenCode 的编码助手的讨论，方便后续作为项目设计文档继续细化。

## 1. `git push -f` 把“空仓库”推到远端会发生什么

### 1.1 结论

`git push -f` 会强制更新远端分支引用。
如果把本地“空内容提交”强推到远端分支，远端分支原本指向的提交历史会被新提交替换。

对正常协作来说，这通常等同于“远端原有内容被覆盖了”。

### 1.2 是否会永久丢失

不一定立刻永久丢失，但应该按“可能丢失”来处理： 

- 如果旧提交仍被其他分支、Tag、PR/MR 或备份引用，通常还能找回。
- 某些托管平台可能短期保留后台恢复能力，但这通常不对普通用户直接开放。
- 如果旧对象后续被垃圾回收，恢复难度会非常高，基本可以视为永久丢失。

### 1.3 实操建议

- 强推前先备份远端分支。
- 优先使用 `git push --force-with-lease`，比 `git push -f` 更安全。
- 强推前确认目标分支是否正确，例如 `main` 或 `master`。

## 2. 编码助手应具备的核心模块

如果要实现一个类似 Claude Code / OpenCode 的编码助手，建议不要只从“功能点”出发，而要按系统分层来设计。

### 2.1 五个最关键模块

#### 1. 会话与编排层

负责整个 Agent Loop：

- 接收用户输入
- 构造上下文
- 决定下一步动作
- 调用工具
- 处理工具结果
- 继续推理或结束响应

可设计为状态机，例如：

- `idle`
- `planning`
- `acting`
- `observing`
- `responding`

#### 2. 模型接入层

封装不同 LLM 提供商，统一暴露接口，处理：

- 消息格式适配
- Tool Calling / Function Calling
- 流式输出
- 限流、超时、重试
- 成本统计
- 模型切换

推荐抽象统一接口，如：

- `generate()`
- `stream()`
- `call_tools()`

#### 3. 工具执行层

编码助手的核心能力之一是“能操作环境”。至少需要支持：

- Shell / Terminal 执行
- 文件读写
- 文件搜索 / 文本搜索
- Patch / Diff 应用
- Git 状态查看
- 测试运行
- 可选的 HTTP / 浏览器能力

#### 4. 代码库感知层

让助手理解项目本身，而不是只依赖用户的一句话。常见能力包括：

- 扫描目录树
- 识别语言与框架
- 定位关键配置文件
- 建立符号索引
- 查找定义 / 引用
- 识别测试入口与构建命令

#### 5. 上下文管理层

这是 Agent 系统成败的关键之一。核心职责：

- 选择当前任务相关文件
- 压缩历史对话
- 摘要工具执行结果
- 做上下文裁剪
- 管理 Token 预算
- 拼接系统 Prompt

### 2.2 安全与控制相关模块

#### 权限与沙箱层

负责控制助手可以做什么，避免变成不受控的 Shell Bot。至少要考虑：

- 只读 / 可写工作区 / 完全访问
- 是否允许联网
- 哪些命令需要审批
- 是否允许删除文件或改写 Git 历史
- 是否允许访问工作区外路径

#### 变更管理层

让改动可审查、可回滚，而不是简单覆盖写文件。主要能力：

- 生成 Diff
- Patch 形式改动
- 按文件或按 Hunk 预览
- 回滚本轮改动
- 记录 Agent 修改过的文件

### 2.3 产品体验相关模块

#### CLI / TUI 交互层

终端助手体验高度依赖这一层，常见职责包括：

- 输入框
- 流式输出
- 工具调用过程展示
- 审批提示
- Diff 预览
- 错误展示
- 中断 / 继续执行
- 会话恢复

#### 记忆与状态持久化

建议至少保存：

- 会话历史
- 当前任务状态
- 用户偏好
- 项目级配置
- 最近读取 / 修改文件

后续再扩展长期记忆：

- 用户习惯
- Repo 级长期记忆
- 常用命令偏好
- 项目约定

#### 观测与评估层

如果没有可观测性，很难持续优化产品。应记录：

- Prompt
- 模型响应
- 工具调用链
- Token / 成本 / 延迟
- 失败原因
- 用户是否接受改动
- 任务完成率

### 2.4 可选增强模块

- 规划器（Planner）
- 自动测试与验证器
- Git 深度集成
- 插件系统 / MCP 接入

## 3. MVP 阶段建议实现的模块

如果目标是先做一个最小可用版本，建议先做下面 7 个模块：

- `LLM 接入`
- `会话编排`
- `文件读写`
- `shell 执行`
- `代码搜索`
- `上下文管理`
- `CLI 界面`

并尽快补上两个关键能力：

- `diff/patch 修改`
- `权限审批`

这样就已经可以完成大部分基础编码任务。

## 4. 推荐的系统分层

建议采用如下分层：

```text
cli/
  - input, output, streaming, approval ui

agent/
  - loop
  - planner
  - tool router
  - state machine

context/
  - prompt builder
  - history summarizer
  - file selector
  - token budgeter

tools/
  - shell
  - fs_read
  - fs_write
  - search
  - patch
  - git
  - test

runtime/
  - sandbox
  - permissions
  - execution policy
  - retries

models/
  - provider adapters
  - message normalization
  - tool-call normalization

repo/
  - indexing
  - symbol lookup
  - project detection

storage/
  - sessions
  - memory
  - config

telemetry/
  - logs
  - traces
  - metrics
  - evals
```

## 5. 容易低估的五个难点

### 1. 上下文裁剪

不是喂给模型越多信息越好，而是要让上下文尽可能相关。

### 2. 工具调用后的状态更新

文件修改、命令执行、测试失败之后，都要及时反映到后续 Prompt 中。

### 3. Shell 安全

模型可能会提出危险命令，因此必须有审批与策略控制。

### 4. Patch 精度

整文件覆盖很脆弱，尤其在用户已有本地改动时，容易误伤。

### 5. 失败恢复

需要处理：

- 测试失败
- 命令挂住
- 输出过长
- 文件冲突
- 上下文过大
- 工具返回异常

## 6. 开发顺序建议

推荐按以下顺序推进：

1. `CLI + 单模型聊天`
2. `文件读取 + 搜索`
3. `shell 执行`
4. `patch 写入`
5. `上下文选择`
6. `审批与沙箱`
7. `测试验证`
8. `记忆、插件、并行 Agent`

## 7. 先按“单文件模块”实现时的目录结构

为了快速推进 MVP，可以先采用“每个模块单文件”的结构：

```text
funcode/
├─ Cargo.toml
├─ Cargo.lock
├─ README.md
├─ .gitignore
├─ examples/
│  └─ sample_project/
├─ data/
│  ├─ sessions/
│  ├─ memories/
│  └─ logs/
├─ prompts/
│  ├─ system.txt
│  ├─ planner.txt
│  └─ summarize.txt
└─ src/
   ├─ main.rs
   ├─ app.rs
   ├─ cli.rs
   ├─ config.rs
   ├─ agent.rs
   ├─ planner.rs
   ├─ context.rs
   ├─ session.rs
   ├─ model.rs
   ├─ tools.rs
   ├─ shell.rs
   ├─ fs.rs
   ├─ patch.rs
   ├─ repo.rs
   ├─ git.rs
   ├─ sandbox.rs
   ├─ approval.rs
   ├─ memory.rs
   ├─ telemetry.rs
   ├─ types.rs
   ├─ errors.rs
   └─ utils.rs
```

如果想先更极简，也可以压缩成：

```text
src/
├─ main.rs
├─ app.rs
├─ agent.rs
├─ model.rs
├─ tools.rs
├─ repo.rs
├─ context.rs
├─ runtime.rs
├─ storage.rs
└─ types.rs
```

## 8. 模块设计图

### 8.1 结构图

```text
+------------------+
|       CLI        |
| input / output   |
| stream / approve |
+---------+--------+
          |
          v
+------------------+
|       App        |
| boot / wiring    |
| lifecycle        |
+---------+--------+
          |
          v
+------------------+
|      Agent       |
| loop / state     |
| tool routing     |
| response build   |
+----+--------+----+
     |        |
     |        |
     v        v
+---------+  +------------------+
| Context |  |      Planner     |
| prompt  |  | task breakdown   |
| history |  | next action      |
| budget  |  | verify strategy  |
+----+----+  +------------------+
     |
     v
+------------------+
|      Model       |
| llm adapter      |
| streaming        |
| tool calling     |
+----+-------------+
     |
     v
+------------------------------+
|            Tools             |
| shell / fs / patch / git     |
+---+------------+-------------+
    |            |
    v            v
+--------+   +------------------+
| Repo   |   | Runtime Control  |
| index  |   | sandbox approval |
| search |   | policy timeout   |
+--------+   +------------------+
    |
    v
+------------------+
| Storage / Memory |
| session / config |
| memory / logs    |
+------------------+
```

### 8.2 执行时序图

```text
User
  |
  v
CLI ---> App ---> Agent
                  |
                  +--> Context: 收集相关文件、历史、配置
                  |
                  +--> Planner: 是否需要拆任务
                  |
                  +--> Model: 生成下一步动作
                              |
                              +--> 普通回答 -> CLI 输出
                              |
                              +--> ToolCall -> Tools 执行
                                                |
                                                +--> shell/fs/patch/git
                                                +--> sandbox/approval 检查
                                                |
                                                +--> 结果回传 Agent
                  |
                  +--> Agent 判断是否继续循环
                  |
                  +--> 最终答复 -> CLI
```

## 9. 单文件阶段每个文件的职责

### `main.rs`

- 程序入口
- 初始化日志、配置、参数
- 调用 `app::run()`

### `app.rs`

- 顶层装配器
- 构建 `Agent`、`ModelClient`、`ToolRegistry`、`SessionStore`
- 管理生命周期

### `cli.rs`

- 处理终端输入输出
- 展示流式回复
- 展示审批与工具执行过程

### `config.rs`

- 加载配置文件与环境变量
- 管理模型参数、超时、日志级别、默认策略

### `agent.rs`

- 核心 Agent Loop
- 状态机管理
- 串联上下文、模型与工具

### `planner.rs`

- 复杂任务拆分
- 产出简单执行计划
- 第一版可以很轻量

### `context.rs`

- 选择相关文件
- 压缩历史与工具结果
- 做 Token 预算与 Prompt 拼装

### `session.rs`

- 保存会话历史、工具调用记录、最近修改文件
- 支持恢复会话

### `model.rs`

- LLM 适配层
- 封装不同厂商接口
- 处理流式输出与 Tool Calling

### `tools.rs`

- 工具注册与路由
- 定义 `Tool` Trait
- 统一工具结果结构

### `shell.rs`

- Shell 命令执行
- 处理超时、退出码、输出截断

### `fs.rs`

- 文件读取、写入、列目录、搜索
- 第一版可封装 `rg` 的能力

### `patch.rs`

- 精细化代码修改
- 支持 Patch、局部替换、重写文件
- 生成前后 Diff

### `repo.rs`

- 扫描代码库
- 检测项目类型
- 找关键文件
- 提供初级代码感知能力

### `git.rs`

- Git 状态、分支、Diff、最近提交查询
- 后续再扩展提交与冲突处理

### `sandbox.rs`

- 判断某项操作是否允许
- 处理只读、工作区可写、联网权限等策略

### `approval.rs`

- 审批请求建模
- 与 CLI 配合完成用户确认流程

### `memory.rs`

- 持久化用户偏好、项目偏好、任务摘要
- 第一版可先用 JSON

### `telemetry.rs`

- 记录日志、指标、链路与失败信息

### `types.rs`

- 存放公共数据结构
- 如 `Message`、`ToolCall`、`ToolResult`、`Session`、`AppConfig`

### `errors.rs`

- 统一错误类型

### `utils.rs`

- 存放纯工具函数
- 如文本截断、路径清洗、时间格式化等

## 10. 推荐先定义的核心 Trait

即使先按单文件实现，也建议先把接口立住：

```rust
pub trait ModelClient {
    fn complete(&self, req: ModelRequest) -> Result<ModelResponse, AppError>;
}

pub trait Tool {
    fn name(&self) -> &'static str;
    fn execute(&self, input: ToolInput) -> Result<ToolResult, AppError>;
}

pub trait MemoryStore {
    fn load_session(&self, id: &str) -> Result<Session, AppError>;
    fn save_session(&self, session: &Session) -> Result<(), AppError>;
}

pub trait ApprovalHandler {
    fn request(&self, req: ApprovalRequest) -> Result<ApprovalDecision, AppError>;
}
```

这样后续替换模型、存储或审批机制时，不需要大改上层。

## 11. 建议优先实现的最小文件集合

第一阶段建议优先落地这 8 个文件：

```text
src/
├─ main.rs
├─ app.rs
├─ agent.rs
├─ model.rs
├─ tools.rs
├─ fs.rs
├─ shell.rs
└─ types.rs
```

之后逐步补：

1. `context.rs`
2. `session.rs`
3. `sandbox.rs`
4. `approval.rs`
5. `patch.rs`
6. `repo.rs`
7. `git.rs`
8. `memory.rs`

## 12. 开发阶段建议

### 阶段 1：能对话

- `main.rs`
- `cli.rs`
- `model.rs`
- `types.rs`

### 阶段 2：能读代码

- `fs.rs`
- `repo.rs`
- `context.rs`
- `tools.rs`

### 阶段 3：能改代码

- `patch.rs`
- `shell.rs`
- `sandbox.rs`
- `approval.rs`

### 阶段 4：像真正助手

- `session.rs`
- `memory.rs`
- `planner.rs`
- `git.rs`
- `telemetry.rs`

## 13. 一句话总结

这个编码助手的核心，不是“聊天模型 + 终端”，而是以下能力的组合：

- Agent 编排
- 工具系统
- 代码库理解
- 上下文管理
- 安全控制
- 可审查改动
- 终端交互体验

先按单文件模块把边界搭起来，是一个非常适合 Rust 项目早期推进的方案。后续等边界稳定，再逐步拆目录、拆子模块，会比一开始过度设计更稳。
