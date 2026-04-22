# 工具调用消息的双层架构与消费关系

## 总览

```
┌─────────────────────────────────────────────────────────────────────┐
│                     Anthropic API (SSE Stream)                      │
│                                                                     │
│  message_start → content_block_start → content_block_delta ×N       │
│                → content_block_stop → message_delta → message_stop  │
└──────────────────────────────┬──────────────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────────────┐
│                     claude.ts (API Layer)                           │
│                                                                     │
│  每个 SSE part:                                                     │
│    ├─ yield { type: 'stream_event', event: part }  ← Layer 1: 原始 │
│    │                                                                │
│    └─ content_block_stop 时:                                        │
│       yield AssistantMessage { content: [完整 block] } ← Layer 2    │
└──────────────────────────────┬──────────────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────────────┐
│                     query.ts (Query Loop)                           │
│                                                                     │
│  yield { type: 'stream_request_start' }           ← 控制信号       │
│  yield StreamEvent (透传)                          ← Layer 1 透传   │
│  yield AssistantMessage (透传)                     ← Layer 2 透传   │
│  yield UserMessage (tool_result)                   ← Layer 2 新增   │
│  yield AttachmentMessage / ProgressMessage         ← Layer 2 新增   │
└───────┬───────────────────────┬────────────────────┬───────────────┘
        │                       │                    │
        ▼                       ▼                    ▼
  ┌──────────┐          ┌──────────────┐     ┌──────────────┐
  │   REPL   │          │ QueryEngine  │     │  Hook Agent  │
  │ (本地UI)  │          │ (SDK/无头)    │     │ (execAgentHook)│
  └──────────┘          └──────────────┘     └──────────────┘
        │                       │                    │
        │                       ▼                    │
        │              ┌──────────────┐              │
        │              │  print.ts    │              │
        │              │  ACP Bridge  │              │
        │              │  Remote CCR  │              │
        │              └──────────────┘              │
        │                                            │
        ▼                                            ▼
  ┌──────────────────────────────────────────────────────┐
  │              消费方如何使用两层消息                      │
  └──────────────────────────────────────────────────────┘
```


## Layer 1: StreamEvent — 逐帧原始 SSE 事件

```
yield { type: 'stream_event', event: { type: 'message_start', message: {...} } }
yield { type: 'stream_event', event: { type: 'content_block_start', index: 0, content_block: { type: 'thinking' } } }
yield { type: 'stream_event', event: { type: 'content_block_delta', index: 0, delta: { type: 'thinking_delta', thinking: 'Let me' } } }
yield { type: 'stream_event', event: { type: 'content_block_delta', index: 0, delta: { type: 'thinking_delta', thinking: ' analyze...' } } }
...
yield { type: 'stream_event', event: { type: 'content_block_stop', index: 0 } }
yield { type: 'stream_event', event: { type: 'content_block_start', index: 1, content_block: { type: 'text', text: '' } } }
yield { type: 'stream_event', event: { type: 'content_block_delta', index: 1, delta: { type: 'text_delta', text: 'I\'ll read' } } }
yield { type: 'stream_event', event: { type: 'content_block_delta', index: 1, delta: { type: 'text_delta', text: ' that file.' } } }
...
yield { type: 'stream_event', event: { type: 'content_block_stop', index: 1 } }
yield { type: 'stream_event', event: { type: 'content_block_start', index: 2, content_block: { type: 'tool_use', id: 'toolu_xxx', name: 'Read', input: '' } } }
yield { type: 'stream_event', event: { type: 'content_block_delta', index: 2, delta: { type: 'input_json_delta', partial_json: '{"file_' } } }
yield { type: 'stream_event', event: { type: 'content_block_delta', index: 2, delta: { type: 'input_json_delta', partial_json: 'path":"/' } } }
yield { type: 'stream_event', event: { type: 'content_block_delta', index: 2, delta: { type: 'input_json_delta', partial_json: 'test.ts"}' } } }
yield { type: 'stream_event', event: { type: 'content_block_stop', index: 2 } }
yield { type: 'stream_event', event: { type: 'message_delta', delta: { stop_reason: 'tool_use' }, usage: {...} } }
yield { type: 'stream_event', event: { type: 'message_stop' } }
```

**用途：实时 UI 状态更新** → spinner 动画、流式文本预览、工具调用进度


## Layer 2: Assembled Messages — 完整的对话消息

```
┌─ content_block_stop(index:0) ──────────────────────────────────────┐
│ yield AssistantMessage {                                            │
│   type: 'assistant',                                                │
│   message: { content: [ ThinkingBlock { thinking: "完整思考内容" } ] } │
│ }                                                                   │
└─────────────────────────────────────────────────────────────────────┘

┌─ content_block_stop(index:1) ──────────────────────────────────────┐
│ yield AssistantMessage {                                            │
│   type: 'assistant',                                                │
│   message: { content: [ TextBlock { text: "I'll read that file." } ] }│
│ }                                                                   │
└─────────────────────────────────────────────────────────────────────┘

┌─ content_block_stop(index:2) ──────────────────────────────────────┐
│ yield AssistantMessage {                                            │
│   type: 'assistant',                                                │
│   message: { content: [ ToolUseBlock {                              │
│     type: 'tool_use',                                               │
│     id: 'toolu_xxx',                                                │
│     name: 'Read',                                                   │
│     input: { file_path: '/test.ts' }   ← 完整 JSON，可直接解析      │
│   } ] }                                                             │
│ }                                                                   │
└─────────────────────────────────────────────────────────────────────┘

  ── 工具执行后 ──

┌─ runToolUse() 完成 ────────────────────────────────────────────────┐
│ yield UserMessage {                                                 │
│   type: 'user',                                                     │
│   message: { content: [ ToolResultBlockParam {                      │
│     type: 'tool_result',                                            │
│     tool_use_id: 'toolu_xxx',                                       │
│     content: '1  import { feature } from ...',   ← 文件内容         │
│   } ] },                                                            │
│   toolUseResult: ReadOutput { type: 'text', file: {...} },          │
│   sourceToolAssistantUUID: '...',                                    │
│ }                                                                   │
└─────────────────────────────────────────────────────────────────────┘
```

**用途：对话历史、工具调度、持久化、SDK 输出**


## 消费方路由详图

```
query() generator yields
│
├─ { type: 'stream_request_start' }
│   ├── REPL ─────────── → setStreamMode('requesting')    // spinner 切换
│   ├── QueryEngine ──── → break (吞掉)
│   └── execAgentHook ── → continue (跳过)
│
├─ { type: 'stream_event', event }
│   │
│   ├── REPL (handleMessageFromStream)
│   │   ├── message_start ─────────── → 记录 TTFT 延迟
│   │   ├── content_block_start
│   │   │   ├── type='thinking' ────── → setStreamMode('thinking')
│   │   │   ├── type='text' ───────── → setStreamMode('responding')
│   │   │   └── type='tool_use' ────── → setStreamMode('tool-use')
│   │   │                              → streamingToolUses=[{id,name}]
│   │   ├── content_block_delta
│   │   │   ├── thinking_delta ────── → streamingThinking += ...
│   │   │   ├── text_delta ───────── → streamingText += ...
│   │   │   └── input_json_delta ─── → 渐进显示工具输入
│   │   ├── message_delta ─────────── → 捕获 stop_reason
│   │   └── message_stop ─────────── → 清理 streamingToolUses
│   │
│   ├── QueryEngine
│   │   ├── message_delta ─────────── → 内部记录 usage / stop_reason
│   │   ├── 其他 ─────────────────── → 仅 includePartialMessages 时透传
│   │   └── 最终 ─────────────────── → 不在默认 SDK 输出中出现
│   │
│   ├── ACP Bridge
│   │   ├── content_block_start ──── → agent_message_chunk / tool_call
│   │   ├── content_block_delta ──── → agent_message_chunk / tool_call_update
│   │   └── (转为 ACP SessionUpdate 通知发给客户端)
│   │
│   └── execAgentHook
│       └── 仅更新 spinner，然后 continue 跳过
│
├─ { type: 'assistant' }  ← AssistantMessage
│   ├── REPL ─────────── → onMessage(msg) → 追加到 messages[]
│   │                      → Messages.tsx 渲染完整消息
│   ├── QueryEngine ──── → normalizeMessage() → yield SDKMessage
│   ├── ACP Bridge ───── → assistantMessageToAcpNotifications()
│   └── useLogMessages ─ → recordTranscript() → 持久化到磁盘
│
├─ { type: 'user' }  ← UserMessage (tool_result)
│   ├── REPL ─────────── → onMessage(msg) → 追加到 messages[]
│   │                      → Messages.tsx 渲染工具结果
│   ├── QueryEngine ──── → normalizeMessage() → yield SDKMessage
│   ├── Bridge ───────── → toSDKMessages() → 转发给 Remote Control
│   └── useLogMessages ─ → recordTranscript() → 持久化到磁盘
│
├─ { type: 'progress' }
│   ├── REPL ─────────── → onMessage(msg) → 进度条更新
│   └── QueryEngine ──── → yield (透传给 SDK)
│
├─ { type: 'attachment' }
│   ├── REPL ─────────── → onMessage(msg) → hook 结果、memory 显示
│   ├── QueryEngine ──── → 内部消费 (structured_output 等)
│   └── Bridge ───────── → 不转发
│
└─ { type: 'system' }
    ├── REPL ─────────── → onMessage(msg) → compact 边界、警告等
    ├── QueryEngine ──── → 部分透传 (compact boundary)
    └── Bridge ───────── → 仅转发 local_command 类型
```


## 消费方对比表

```
┌─────────────────┬───────────────────┬───────────────────┬──────────────────────┐
│                 │  Layer 1          │  Layer 2          │  写入磁盘            │
│                 │  StreamEvent      │  Assembled Msg    │  (Transcript)        │
├─────────────────┼───────────────────┼───────────────────┼──────────────────────┤
│ REPL (本地终端)  │ ✅ spinner 动画   │ ✅ 消息列表渲染   │ ✅ useLogMessages    │
│                 │ ✅ 流式文本预览    │ ✅ 工具结果展示   │                      │
│                 │ ✅ 工具名/参数渐进 │                   │                      │
├─────────────────┼───────────────────┼───────────────────┼──────────────────────┤
│ QueryEngine     │ ⚙️ 仅内部用       │ ✅ normalize 后   │ ❌                   │
│ (SDK/无头模式)   │   (usage/stop)    │   yield SDKMessage│                      │
├─────────────────┼───────────────────┼───────────────────┼──────────────────────┤
│ ACP Bridge      │ ✅ 实时流式通知    │ ✅ 完成消息通知   │ ❌                   │
│ (Agent 协议)    │   agent_msg_chunk │   usage_update    │                      │
├─────────────────┼───────────────────┼───────────────────┼──────────────────────┤
│ Bridge/RCS      │ ❌                │ ✅ user/assistant │ ❌                   │
│ (Remote Control)│                   │   仅对话类消息    │                      │
├─────────────────┼───────────────────┼─────────────────────┼────────────────────┤
│ print.ts        │ ❌ 过滤掉         │ ✅ 输出到 stdout  │ ❌                   │
│ (CLI pipe 模式) │                   │   或 bridge 转发  │                      │
├─────────────────┼───────────────────┼───────────────────┼──────────────────────┤
│ execAgentHook   │ ⚙️ 仅 spinner     │ ✅ 计算结果       │ ❌                   │
│                 │   后跳过          │                   │                      │
└─────────────────┴───────────────────┴───────────────────┴──────────────────────┘
```


## 时序：一次工具调用的完整 yield 序列及消费

```
时间 ──────────────────────────────────────────────────────────────────►

query() yields:     ┌─L1─┐ ┌─L1─┐ ┌─L1─┐       ┌─L2─┐ ┌─L1─┐ ┌─L1─┐
                    │s_e │ │s_e │ │s_e │  ...  │ AM │ │s_e │ │s_e │
                    │m_s │ │cbs │ │cbd │       │thk │ │cbs │ │cbd │
                    └────┘ └────┘ └────┘       └────┘ └────┘ └────┘
                                                              ...
                                          ┌─L2─┐ ┌─L2─┐             ┌─L2─┐
                                          │ AM │ │ AM │  tool exec  │ UM │
                                          │txt │ │tool│  ────────►  │res │
                                          └────┘ └────┘             └────┘

REPL spinner:       [请求中] [思考中 ●●● ] [思考完] [回复中 ●●] [工具中 🔧] [完成 ✓]
                    ▲       ▲            ▲        ▲          ▲          ▲
                    │       │            │        │          │          │
                  stream  content_    content_  content_   content_   tool result
                  _start  block_      block_   block_     block_     UserMessage
                          start(think) stop     start(txt) stop(tool)

REPL messages[]:    [                                 ] [AM_think] [AM_text] [AM_tool] [UM_result]
                    ↑ 只有 Layer 2 消息追加到对话历史 ↑

ACP notifications: [agent_thought_chunk] [agent_msg_chunk] [tool_call] [tool_call_update] [result]
                    ↑ Layer 1 + Layer 2 统一转为 ACP 通知 ↑

Bridge (RCS):      ──────────────────────────────── [AM_text] [AM_tool] [UM_result] ──────►
                    ↑ 仅转发 Layer 2 的 user/assistant 消息 ↑
```
