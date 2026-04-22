# 用户输入文本后的数据流图

> **Happy Path**（步骤 ❶-❿）：纯文本输入 → 纯文本回复
> **Tool Path**（步骤 A-F）：当回复包含 tool_use 且需要用户审批时的分支路径

## 全链路总览

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              用户终端 (Terminal)                              │
│                                                                             │
│  用户输入 "帮我修复这个 bug" 并按 Enter                                       │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │  input: string
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  ❶ REPL.tsx:3807  onSubmit(input, helpers)                                  │
│                                                                             │
│  PromptInput 组件的 onSubmit 回调，接收原始用户文本                            │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  ❷ handlePromptSubmit.ts:128  handlePromptSubmit({ input })                 │
│                                                                             │
│  处理粘贴文本引用展开 → processUserInput() → createUserMessage()             │
│                                                                             │
│  数据变换:  string  ──►  UserMessage {                                      │
│    type: 'user',                                                            │
│    message: { role: 'user', content: "帮我修复这个 bug" },                   │
│    uuid: randomUUID(),                                                      │
│    timestamp: new Date().toISOString()                                      │
│  }                                                                          │
│                                                                             │
│  最后调用 onQuery(newMessages)                                               │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │  newMessages: UserMessage[]
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  ❸ REPL.tsx:3457  onQuery(newMessages, abortController, ...)                │
│                                                                             │
│  ① setMessages(prev => [...prev, ...newMessages])                           │
│     ──► React 立即重渲染，用户看到自己的消息出现在终端                          │
│                                                                             │
│  ② 调用 onQueryImpl(messages, newMessages, ...)                             │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │  messages: Message[] (完整对话历史)
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  ❹ REPL.tsx:3174  onQueryImpl(messages, ...)                                │
│                                                                             │
│  并行加载上下文:                                                              │
│  ┌────────────────────────────────────────────────────────────────┐         │
│  │  Promise.all([                                                │         │
│  │    getSystemPrompt(tools, model),   // 系统提示词               │         │
│  │    getUserContext(),                // CLAUDE.md + 日期         │         │
│  │    getSystemContext(),              // git 状态                 │         │
│  │  ])                                                           │         │
│  └────────────────────────────────────────────────────────────────┘         │
│                                                                             │
│  进入主查询循环:                                                              │
│  for await (const event of query({ messages, systemPrompt,                   │
│                                     userContext, systemContext })) {         │
│    onQueryEvent(event)                                                      │
│  }                                                                          │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │  QueryParams
                                   ▼
```

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  ❺ query.ts:222  query(params)  ──►  yield* queryLoop(params)               │
│                                                                             │
│  queryLoop 一次迭代的处理流水线:                                               │
│                                                                             │
│  (a) yield { type: 'stream_request_start' }                                │
│      ──► REPL 收到后切换 spinner 为 "请求中"                                  │
│                                                                             │
│  (b) 消息预处理:                                                              │
│      getMessagesAfterCompactBoundary(messages)  // 提取压缩边界后的消息       │
│      applyToolResultBudget(messages)            // 裁剪过大的工具结果          │
│      autocompact 检查 (token 超阈值则触发压缩)                                │
│                                                                             │
│  (c) 上下文注入:                                                              │
│      appendSystemContext(systemPrompt, systemContext)                        │
│      // → systemPrompt 末尾追加 git 状态                                     │
│      prependUserContext(messages, userContext)                               │
│      // → messages 头部插入 <system-reminder> 合成消息                        │
│                                                                             │
│  注入后的 messages 结构:                                                      │
│  ┌─────────────────────────────────────────────────┐                        │
│  │ [0] UserMessage { content: <system-reminder>    │ ← 合成上下文消息        │
│  │        "CLAUDE.md 内容\nToday's date: ..." }    │   (isMeta: true)       │
│  │ [1] UserMessage { content: "帮我修复这个 bug" }  │ ← 真实用户消息         │
│  │ [2] AssistantMessage { ... }                     │ ← 历史回复             │
│  │ [3] UserMessage { ... }                          │ ← 历史消息             │
│  │ ...                                             │                        │
│  └─────────────────────────────────────────────────┘                        │
│                                                                             │
│  (d) 调用 API:                                                               │
│  for await (const msg of deps.callModel({ messages, systemPrompt,           │
│                                            tools, thinkingConfig })) {      │
│    yield msg                                                                │
│  }                                                                          │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │  API 请求参数
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  ❻ claude.ts:1035  queryModel()                                             │
│                                                                             │
│  ① normalizeMessagesForAPI(messages)  // 格式化为 API 规范                   │
│  ② buildSystemPromptBlocks(systemPrompt, enableCaching)                     │
│     // → 在 section 边界插入 cache_control breakpoint                        │
│  ③ 构建请求:                                                                 │
│     params = {                                                              │
│       model: "claude-sonnet-4-6",                                           │
│       messages: [...],                       // 格式化后的消息               │
│       system: [{ type:'text', text, cache_control }],                       │
│       tools: [...],                          // 工具定义列表                 │
│       max_tokens: 16384,                                                    │
│       thinking: { type:'enabled', budget_tokens: 10000 },                   │
│       stream: true                                                          │
│     }                                                                       │
│  ④ 发起流式请求:                                                             │
│     anthropic.beta.messages.create(params).withResponse()                   │
│                                                                             │
│  ⑤ 消费 SSE 流 (for await part of stream):                                  │
│  ┌──────────────────────────────────────────────────────────────────┐       │
│  │  SSE Event           │  处理                                     │       │
│  ├───────────────────────┼──────────────────────────────────────────┤       │
│  │  message_start        │  捕获 partialMessage, usage              │       │
│  │  content_block_start  │  初始化 content block (text/thinking)    │       │
│  │  content_block_delta  │  拼接 delta 到 block (text_delta)        │       │
│  │  content_block_stop   │  组装完整 block, yield AssistantMessage  │       │
│  │  message_delta        │  更新 usage, stop_reason                 │       │
│  │  message_stop         │  最终清理                                 │       │
│  └───────────────────────┴──────────────────────────────────────────┘       │
│                                                                             │
│  每个 content_block_stop 时 yield:                                           │
│  AssistantMessage {                                                         │
│    type: 'assistant',                                                       │
│    message: {                                                               │
│      role: 'assistant',                                                     │
│      content: [ TextBlock { text: "好的，我来帮你..." } ],                   │
│      usage: { input_tokens, output_tokens, ... },                           │
│      stop_reason: 'end_turn'                                                │
│    },                                                                       │
│    uuid: randomUUID(),                                                      │
│    timestamp: new Date().toISOString(),                                     │
│    requestId: 'req_xxx'                                                     │
│  }                                                                          │
│                                                                             │
│  同时逐帧 yield StreamEvent (Layer 1):                                       │
│  { type: 'stream_event', event: { type: 'content_block_delta', ... } }      │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                    ┌──────────────┴──────────────┐
                    │  双层消息并行回传              │
                    │  Layer 1: StreamEvent (逐帧)  │
                    │  Layer 2: AssistantMessage    │
                    └──────────────┬──────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  ⓿ queryLoop 透传 yield ──► query() 透传 yield ──► onQueryEvent(event)      │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  ❽ messages.ts:2974  handleMessageFromStream(event, onMessage, ...)         │
│                                                                             │
│  Layer 1 处理 (实时流式):                                                     │
│  ┌─────────────────────────────────────────────────────────────────┐        │
│  │  content_block_start(text)  → setStreamMode('responding')      │        │
│  │  content_block_delta(text)  → streamingText += delta           │        │
│  │                               → 终端逐字渲染回复文本             │        │
│  └─────────────────────────────────────────────────────────────────┘        │
│                                                                             │
│  Layer 2 处理 (完整消息):                                                     │
│  ┌─────────────────────────────────────────────────────────────────┐        │
│  │  AssistantMessage 到达  → onMessage(message)                   │        │
│  │    → setMessages(prev => [...prev, message])                   │        │
│  │    → React 重渲染，将完整消息固定到消息列表                       │        │
│  └─────────────────────────────────────────────────────────────────┘        │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                    ┌──────────────┴──────────────┐
                    │                              │
                    ▼                              ▼
┌────────────────────────────┐    ┌────────────────────────────────────────┐
│  ❾ UI 渲染                  │    │  ❿ 持久化                              │
│                            │    │                                        │
│  Messages.tsx 遍历          │    │  sessionStorage.ts:1409               │
│  messages[] 渲染消息行      │    │  recordTranscript(messages)            │
│                            │    │                                        │
│  每条 AssistantMessage 渲染  │    │  写入 ~/.claude/projects/             │
│  为 MessageRow 组件:        │    │       <sanitized-path>/<uuid>.jsonl   │
│  - TextBlock → 文本显示     │    │                                        │
│  - ThinkingBlock → 折叠思考  │    │  格式: 每行一个 JSON entry,           │
│                            │    │  通过 parentUuid 链接形成分支树        │
└────────────────────────────┘    └────────────────────────────────────────┘
```

## 数据结构变换总结

```
阶段                     数据类型                    关键字段
───────────────────────  ─────────────────────────  ────────────────────────────
用户输入                  string                     "帮我修复这个 bug"
                        │
createUserMessage()      UserMessage                type, message.role,
                        │                          message.content, uuid, timestamp
                        ▼
prependUserContext()     Message[]                  头部插入 <system-reminder> 合成消息
+ appendSystemContext()  │                          systemPrompt 追加 git 状态
                        ▼
normalizeMessagesForAPI  BetaMessageParam[]          格式化为 Anthropic API 规范
+ buildSystemPrompt()   │                          system 注入 cache breakpoint
                        ▼
API Request             BetaMessageStreamParams     model, messages, system, tools,
                        │                          max_tokens, stream: true
                        ▼
SSE Stream              BetaRawMessageStreamEvent   message_start → content_block_* →
                        │                          message_delta → message_stop
                        ▼
content_block_stop      AssistantMessage            type: 'assistant', message.content,
                        │                          message.usage, stop_reason, uuid
                        ▼
setMessages()           Message[] (React state)     追加到全局状态数组，触发重渲染
                        │
                        ▼
recordTranscript()      JSONL entries               每行 JSON，parentUuid 链式关联
```

## 工具审批分支 (Tool Approval Path)

当 API 返回的 AssistantMessage 包含 `tool_use` content block 时，queryLoop 进入工具执行分支：

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  ❺ queryLoop — 流式响应中的 tool_use 检测                                     │
│                                                                             │
│  流式到达 AssistantMessage {                                                 │
│    message: {                                                               │
│      content: [                                                             │
│        TextBlock { text: "让我读取那个文件" },                                │
│        ToolUseBlock {                                                       │
│          type: 'tool_use',                                                  │
│          id: 'toolu_01ABC',                                                 │
│          name: 'Read',                                                      │
│          input: { file_path: '/src/query.ts' }                              │
│        }                                                                    │
│      ],                                                                     │
│      stop_reason: 'tool_use'                                                │
│    }                                                                        │
│  }                                                                          │
│                                                                             │
│  检测到 tool_use block → needsFollowUp = true                                │
│  StreamingToolExecutor.addTool(toolBlock, assistantMessage)                 │
│  流式 yield AssistantMessage 给 REPL 渲染 (用户看到工具调用意图)                │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │  needsFollowUp = true
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  A. toolExecution.ts:493  checkPermissionsAndCallTool()                     │
│                                                                             │
│  StreamingToolExecutor.getRemainingResults()                                │
│    └── runToolUse(tool, input, toolUseContext, ...)                         │
│          └── checkPermissionsAndCallTool()                                  │
│                └── resolveHookPermissionDecision()                          │
│                      └── canUseTool(tool, input, ...)                       │
│                            │                                                │
│                            ▼                                                │
│              permissions.ts:473  hasPermissionsToUseTool()                   │
│              ┌─────────────────────────────────────────────────┐            │
│              │  权限检查流水线 (按优先级):                        │            │
│              │                                                 │            │
│              │  ① getDenyRuleForTool()    → behavior:'deny'?   │            │
│              │  ② getAskRuleForTool()     → behavior:'ask'?    │            │
│              │  ③ tool.checkPermissions() → 工具自定义检查      │            │
│              │  ④ tool.requiresUserInteraction()               │            │
│              │  ⑤ 安全路径检查 (.git/, .claude/)                │            │
│              │  ⑥ bypassPermissions 模式检测                    │            │
│              │  ⑦ toolAlwaysAllowedRule  → behavior:'allow'?   │            │
│              └────────────────┬────────────────────────────────┘            │
│                               │                                             │
│              ┌────────────────┼────────────────┐                            │
│              ▼                ▼                ▼                            │
│         behavior:        behavior:        behavior:                         │
│         'allow'           'ask'            'deny'                           │
│            │                │                │                              │
│            │                │                └──► 立即返回拒绝结果            │
│            │                │                    UserMessage {               │
│            │                │                      tool_result(is_error:true)│
│            │                │                    }                            │
│            │                ▼                                                 │
│            │    ┌───────────────────────────────────────────┐                │
│            │    │  useCanUseTool.tsx:307                    │                │
│            │    │  handleInteractivePermission(resolve)      │                │
│            │    └───────────────────┬───────────────────────┘                │
│            │                        │                                        │
│            │                        ▼                                        │
│            │    ┌───────────────────────────────────────────┐                │
│            │    │  创建 ToolUseConfirm 对象:                  │                │
│            │    │  {                                        │                │
│            │    │    tool: Read,                             │                │
│            │    │    input: { file_path: '/src/query.ts' },  │                │
│            │    │    toolUseID: 'toolu_01ABC',               │                │
│            │    │    onAllow(updatedInput, permissions),     │                │
│            │    │    onReject(feedback),                     │                │
│            │    │    onAbort(),                              │                │
│            │    │  }                                        │                │
│            │    └───────────────────┬───────────────────────┘                │
│            │                        │                                        │
│            │           setToolUseConfirmQueue(                               │
│            │             prev => [...prev, toolUseConfirm]                    │
│            │           )                                                     │
│            │                        │                                        │
│            │                        ▼                                        │
│            │    ╔═══════════════════════════════════════════╗                  │
│            │    ║  Promise 等待用户交互 (阻塞 canUseTool)    ║                  │
│            │    ╚═══════════════════════════════════════════╝                  │
│            │                                                                 │
│            └──────────────────────┐                                          │
│                                   ▼                                          │
└─────────────────────────────────────────────────────────────────────────────┘
```

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  B. REPL.tsx:5527  PermissionRequest 组件渲染                                │
│                                                                             │
│  toolUseConfirmQueue[0] 存在时渲染 PermissionRequest:                        │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────┐           │
│  │  PermissionRequest.tsx:196                                   │           │
│  │                                                              │           │
│  │  permissionComponentForTool(tool) 选择专用审批组件:            │           │
│  │    BashTool    → BashPermissionRequest                       │           │
│  │    FileEdit    → FileEditPermissionRequest (展示 diff)        │           │
│  │    FileWrite   → FileWritePermissionRequest (展示 diff)      │           │
│  │    其他        → FilesystemPermissionRequest                  │           │
│  │                                                              │           │
│  │  专用组件内部使用 PermissionPrompt 渲染选项列表:                │           │
│  │  ┌────────────────────────────────────────────────────┐      │           │
│  │  │  ❯ Yes, proceed                                     │      │           │
│  │  │    Yes, and don't ask again for Read commands       │      │           │
│  │  │    No, reject                                       │      │           │
│  │  │    No, and don't ask again for Read commands        │      │           │
│  │  └────────────────────────────────────────────────────┘      │           │
│  │                                                              │           │
│  │  用户选择 → handleSelect(value) → onSelect(value, feedback)   │           │
│  └──────────────────────────────────────────────────────────────┘           │
│                                                                             │
│  终端 UI 效果:                                                               │
│  ┌─────────────────────────────────────────────────────────────┐            │
│  │  ⏺ Claude wants to read this file:                          │            │
│  │    /src/query.ts                                             │            │
│  │                                                              │            │
│  │  ❯ Allow                                                    │            │
│  │    Allow for this session (Read)                             │            │
│  │    Deny                                                      │            │
│  │    Deny for this session (Read)                              │            │
│  └─────────────────────────────────────────────────────────────┘            │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                    ┌──────────────┴──────────────┐
                    │  用户选择                      │
                    ▼                              ▼
              "Allow"                         "Deny"
                    │                              │
                    ▼                              ▼
┌─────────────────────────────┐  ┌─────────────────────────────────────────────┐
│  C. toolUseConfirm.onAllow() │  │  toolUseConfirm.onReject()                  │
│                             │  │                                             │
│  ① ctx.handleUserAllow()    │  │  ① ctx.cancelAndAbort()                     │
│     - 持久化权限规则          │  │     - 中止工具执行                           │
│       (写入 settings.json)   │  │                                             │
│     - 记录分析事件           │  │  ② 返回 PermissionDenyDecision              │
│                             │  │     { behavior: 'deny', message: "..." }    │
│  ② resolve(                 │  │                                             │
│    PermissionAllowDecision  │  │  ③ 创建错误 tool_result:                     │
│    { behavior: 'allow',     │  │     UserMessage {                            │
│      updatedInput }         │  │       message: {                             │
│  )                          │  │         content: [tool_result {               │
│                             │  │           is_error: true,                    │
│  ③ Promise 解除阻塞         │  │           content: "Error: User denied"      │
│     canUseTool() 返回 allow │  │         }]                                   │
│                             │  │       }                                      │
└──────────────┬──────────────┘  │     }                                        │
               │                  └──────────────────┬──────────────────────────┘
               ▼                                     │
┌─────────────────────────────────────────────────────┼───────────────────────┐
│  D. toolExecution.ts:1221  工具执行 (仅 Allow 路径)  │                       │
│                                                     │                       │
│  ① 应用 updatedInput (用户可能修改了输入)              │                       │
│  ② tool.call(input, toolUseContext, ...)            │                       │
│     ──► 执行实际工具逻辑 (读文件、运行命令等)          │                       │
│  ③ result 映射为 ToolResultBlockParam:               │                       │
│     {                                               │                       │
│       type: 'tool_result',                          │                       │
│       tool_use_id: 'toolu_01ABC',                   │                       │
│       content: "1  import { feature }..."           │                       │
│     }                                               │                       │
│  ④ 包装为 UserMessage:                              │                       │
│     UserMessage {                                   │                       │
│       type: 'user',                                 │                       │
│       message: {                                    │                       │
│         role: 'user',                               │                       │
│         content: [ ToolResultBlockParam ]            │                       │
│       },                                            │                       │
│       toolUseResult: ...,                           │                       │
│       sourceToolAssistantUUID: assistantMessage.uuid │                       │
│     }                                               │                       │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │  UserMessage (tool_result)
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  E. query.ts:1762  Loop 状态更新 & 继续迭代                                   │
│                                                                             │
│  toolResult 被 yield 回 queryLoop:                                           │
│    yield update.message  ──► REPL 渲染工具结果                                │
│    toolResults.push(normalizeMessagesForAPI([msg]))                          │
│                                                                             │
│  状态合并:                                                                   │
│  state.messages = [                                                         │
│    ...messagesForQuery,       // 之前的对话历史                               │
│    ...assistantMessages,      // 含 tool_use 的 AssistantMessage             │
│    ...toolResults,            // 含 tool_result 的 UserMessage               │
│  ]                                                                          │
│                                                                             │
│  while(true) 继续 → 下一轮迭代 → 回到步骤 ❺(b)                               │
│  新的 API 请求会携带 tool_use + tool_result 对                               │
│  模型基于工具结果继续回复                                                      │
└─────────────────────────────────────────────────────────────────────────────┘

                           ═══ 完整循环 ═══

  用户输入 ──► API 请求 ──► API 返回 tool_use ──► 权限检查
                                                     │
                                          ┌──────────┼──────────┐
                                          ▼          ▼          ▼
                                       allow        deny      ask→UI
                                          │          │          │
                                          ▼          │          ▼
                                      工具执行       │     用户审批 ─┐
                                          │          │       allow │ deny
                                          ▼          │          │    │
                                    tool_result      │     工具执行  │
                                          │          │       │      │
                                          ▼          ▼       ▼      ▼
                                      新 API 请求 (携带 tool_use + tool_result)
                                          │
                                          ▼
                                    模型继续回复 ──► 可能再次触发 tool_use → 循环
```

## 时序图

### Happy Path（纯文本回复）

```
时间 ───────────────────────────────────────────────────────────────────────────►

用户        REPL.tsx      handlePrompt      query.ts        claude.ts       Anthropic API
 │             │           Submit.ts          │                │                │
 │─Enter──────►│             │                 │                │                │
 │             │──input──────►│                 │                │                │
 │             │             │─UserMessage─────►│                │                │
 │             │◄───渲染用户消息───────────────  │                │                │
 │             │             │                 │──load context──►│                │
 │             │             │                 │  systemPrompt  │                │
 │             │             │                 │  userContext   │                │
 │             │             │                 │  systemContext │                │
 │             │             │                 │                │                │
 │             │◄──stream_request_start───────  │                │                │
 │  [spinner]  │             │                 │──params────────►│                │
 │             │             │                 │                │──stream req────►│
 │             │             │                 │                │                │
 │             │             │                 │                │◄──message_start─│
 │             │             │                 │                │◄──cbs(thinking)─│
 │             │  [思考中●]   │                 │◄──stream_event─│◄──cbd×N────────│
 │             │             │                 │◄──AM(thinking)─│◄──cbs_stop─────│
 │             │             │                 │                │                │
 │             │             │                 │                │◄──cbs(text)────│
 │             │  [回复中●]   │                 │◄──stream_event─│◄──cbd×N────────│
 │             │  逐字渲染    │                 │◄──AM(text)─────│◄──cbs_stop─────│
 │             │             │                 │                │◄──msg_delta────│
 │             │             │                 │                │◄──msg_stop─────│
 │             │             │                 │                │                │
 │             │◄──AM(text)──────────────────  │                │                │
 │  [完整回复]  │             │                 │                │                │
 │             │──recordTranscript(异步)──────────────────────────────────────────►│
 │             │             │                 │──loop ends─────│                │
 │             │             │                 │  return        │                │
 │  [完成 ✓]   │             │                 │                │                │
```

### Tool Approval Path（工具审批）

```
时间 ───────────────────────────────────────────────────────────────────────────►

用户      REPL.tsx     query.ts    permissions.ts   toolExecution.ts   Anthropic API
 │           │            │              │                │                  │
 │           │            │              │                │◄──AM(tool_use)───│
 │           │◄──AM(tool)─│              │                │  stop:tool_use   │
 │  [工具名]  │            │              │                │                  │
 │           │            │──canUseTool──►│                │                  │
 │           │            │              │──hasPermissions│                  │
 │           │            │              │  ToUseTool()   │                  │
 │           │            │              │                │                  │
 │           │            │              │  deny规则?──no──►                │                  │
 │           │            │              │  ask规则?───yes──►               │                  │
 │           │            │              │  alwaysAllow?─no►               │                  │
 │           │            │              │                │                  │
 │           │            │              │  behavior:'ask'│                  │
 │           │            │              │                │                  │
 │           │            │              │──ToolUseConfirm│                  │
 │           │            │              │  pushToQueue───►                  │
 │           │◄──PermissionRequest组件───│                │                  │
 │           │            │              │                │                  │
 │           │  ┌─────────────────────────────────────────────────┐          │
 │           │  │  "Claude wants to read: /src/query.ts"          │          │
 │           │  │  ❯ Allow                                        │          │
 │           │  │    Allow for session (Read)                     │          │
 │           │  │    Deny                                         │          │
 │           │  └─────────────────────────────────────────────────┘          │
 │           │            │              │                │                  │
 │──Allow───►│            │              │                │                  │
 │           │──onAllow───│──resolve────►│                │                  │
 │           │            │              │──PermissionAllowDecision           │
 │           │            │              │                │                  │
 │           │            │◄─allow───────│                │                  │
 │           │            │              │                │                  │
 │           │            │──runToolUse───────────────────►│                  │
 │           │            │              │                │──tool.call()─────│
 │           │            │              │                │  (执行工具)       │
 │           │            │              │                │                  │
 │  [工具结果] │◄──UM(tool_result)───────│                │                  │
 │  渲染      │            │              │                │                  │
 │           │            │              │                │                  │
 │           │            │──state 更新 (合并 tool_result 到 messages)      │
 │           │            │              │                │                  │
 │           │            │────────── 新 API 请求 (携带 tool_result) ──────►│
 │           │            │              │                │                  │
 │           │            │              │                │◄─模型基于工具结果─│
 │           │            │              │                │  继续回复         │
 │           │◄──AM(text)─│              │                │                  │
 │  [完成 ✓]  │            │              │                │                  │
```
