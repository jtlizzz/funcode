# funcode

`funcode` 是一个面向终端场景的 Rust 编码助手实验项目，目标是逐步实现一个类似 Claude Code / OpenCode 的本地 Agent：

- 能理解当前代码仓库
- 能读取、搜索、修改文件
- 能执行命令并观察结果
- 能在受控权限下完成编码任务
- 能以 CLI/TUI 形式与用户协作

当前仓库还处于早期骨架阶段，设计文档已经整理完成，代码实现将围绕这些模块逐步落地。

## 项目目标

这个项目希望构建一个真正可用的“终端里的编码助手”，而不只是一个简单的聊天壳。核心方向包括：

- `Agent 编排`：负责多轮推理、工具调用和任务收敛
- `模型接入`：统一对接不同 LLM 提供商
- `工具系统`：支持 shell、文件系统、patch、git 等能力
- `代码库感知`：理解项目结构、关键文件和上下文
- `上下文管理`：在有限 token 中挑选最有用的信息
- `安全控制`：支持沙箱、审批、权限策略
- `终端体验`：支持流式输出、审批交互、会话恢复

## 当前状态

目前仓库包含：

- 一个基础 Rust 工程
- 一份较完整的设计整理文档：`docs/coding-assistant-design.md`

后续会按“先单文件模块、后逐步拆分”的思路推进实现。

## 设计文档

建议先阅读下面这份文档：

- `docs/coding-assistant-design.md`

文档中已经整理了：

- `git push -f` 覆盖远端的风险说明
- 编码助手的模块划分
- MVP 建议
- 推荐目录结构
- 系统设计图
- 单文件阶段每个模块的职责
- 核心 trait 草图
- 分阶段开发建议

## 推荐的开发路径

建议按以下阶段推进：

1. `阶段 1：能对话`
   - CLI 输入输出
   - 单模型调用
   - 基础消息结构
2. `阶段 2：能读代码`
   - 文件读取
   - 文件搜索
   - 代码库扫描
   - 上下文拼装
3. `阶段 3：能改代码`
   - patch 写入
   - shell 执行
   - 沙箱与审批
4. `阶段 4：像真正助手`
   - 会话持久化
   - 记忆系统
   - Planner
   - Git 集成
   - Telemetry

## 计划中的模块结构

第一版会优先采用“每个模块一个文件”的方式实现，预计围绕以下文件展开：

```text
src/
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

## 快速开始

当前项目还是早期阶段，可以先直接运行默认 Rust 程序：

```bash
cargo run
```

如果后续添加了更多模块，推荐在开发时使用：

```bash
cargo check
cargo test
```

## 近期建议

接下来最自然的推进方向有三个：

1. 先把 `README` 对应的目录骨架和空模块文件建出来
2. 先实现 `types.rs`、`model.rs`、`agent.rs` 的最小可运行骨架
3. 先做一个可交互的 CLI 原型，把“用户输入 -> 模型输出”链路跑通

## 技术栈

目前默认技术方向：

- `Rust`
- `CLI-first` 的交互方式
- 本地工具执行 + 受控权限模型
- 后续按需要接入具体 LLM Provider

## 项目说明

这个仓库目前更偏向“架构设计与原型搭建”阶段，因此：

- 文档会先于大规模实现
- 模块边界会先稳定下来
- 等 MVP 跑通后，再决定如何拆分为更细的目录结构

如果你准备继续开发，建议先从 `docs/coding-assistant-design.md` 出发，再开始补 `src/` 下的模块骨架。
