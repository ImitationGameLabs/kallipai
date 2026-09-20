---
title: 模型配置方法
description: 用网页界面、`kallip profile-set` 命令或手动编辑 `profiles.toml` 配置模型。
order: 21
---

[配置参考](../index.md)的一部分；本页讲模型配置的三种方法。三层模型（provider、profile、profile set 各是什么，为何各自成层）见[模型配置](llm.md)。

## 网页界面配置

管理区的 Profiles 页是日常配置入口。它维护端点池（协议族、密钥、`base_url`），把 profile 卡片在命名 set 与暂存区之间拖拽组织，声明各 profile 的上下文窗口与模态，转移 default set 标记，并可对 set 或单个 profile 发起连通测试。

保存后写入实例的 `profiles.toml` 并立即生效。删除仍有 agent 绑定的 set 需要确认。字段的完整含义见下方 `profiles.toml` 手动编辑一节。

## `kallip profile-set` 命令

四个子命令覆盖检视、改绑、转移标记与删除：

- `kallip profile-set list`：列出已配置的 profile set、各自的 profile 数与 default 标记。
- `kallip profile-set bind <ID> <SET>`：把 agent 改绑到一个命名 set。活跃的 agent 在下次唤醒时换用新的故障转移链；停泊中的 agent 在恢复时解析。
- `kallip profile-set default <SET>`：把 default set 标记转移给一个既有 set。
- `kallip profile-set remove <SET>`：删除一个 set。default set 与 root 的 set 被拒绝；其余被引用的 set 会列出绑定它的 agent，需要 `--force`（这些 agent 被中断，然后保留悬空记录直到改绑）。

`bind` 的 set 名必须精确匹配，未知名会列出可用的 set。

## `profiles.toml` 手动编辑

### 文件位置

profiles 配置文件位于实例配置根的 `profiles/profiles.toml`：`<config home>/kallipai/tagmata/<slug>/`。配置根推导不出（无 `KALLIP_TAGMA_SLUG`、无 config home）时，读取降级为隐式 env profile 并记警告；无配置根的写入则报错。没有配置文件（经 Harbor 与 `kallip-run` 做基准/脚本的默认；Harbor 是外部 agent 基准测试框架）时用单一隐式 profile：其 `max_context_window` 从 `KALLIP_CONTEXT_WINDOW_TOKENS`（默认 `128000`）推导。

有配置文件时，tagma 加载多个 provider/model 组合，每个 profile 自行声明 `max_context_window`。每个 profile 还可声明 `modalities`；省略默认纯文本，set 的生效模态是其成员的交集。

### 示例

`profiles.toml` 示例：

```toml
default = "primary"

[endpoints.deepseek-primary]
family = "deepseek"
api_key = "${KALLIP_LLM_DEEPSEEK_API_KEY}" # env-var indirection keeps secrets out of the file

[endpoints.openrouter]
family = "openai-compatible"
api_key = "${OPENROUTER_API_KEY}"
base_url = "https://openrouter.ai/api/v1"

[endpoints.official-openai]
family = "openai-responses"
api_key = "${OPENAI_API_KEY}" # no base_url: talks to the official endpoint

[sets.primary]
description = "full-capability work"
  [[sets.primary.profiles]]
  id = "deepseek-v4-pro"
  endpoint = "deepseek-primary"
  model = "deepseek-v4-pro"
  max_context_window = 500000

[sets.fast]
description = "cheap delegation"
  [[sets.fast.profiles]]
  id = "deepseek-v4-flash"
  endpoint = "deepseek-primary"
  model = "deepseek-v4-flash"
  max_context_window = 128000
```

### 字段

- `family` 取 `deepseek`、`openai-compatible`、`openai-responses`、`anthropic` 之一。
- `base_url` 对 `openai-compatible` 必填，其余可选：其他族省略时回落各自官方端点。
- `store`（可选，默认 `true`）保持服务端会话存储开启，`openai-responses` 因此每轮从存储链继续；`store = false`（或任何非 Responses 族）每轮重放完整会话。
- `effort`（可选）请求推理力度：`low`、`medium`、`high`、`xhigh` 或 `max`。provider 在自身限度内尊重它（DeepSeek 官方把 `medium` 与 `xhigh` 降为 `high`，见其 [API 参考](https://api-docs.deepseek.com/api/create-chat-completion)）。
- `api_key` / `base_url` 里的 `${VAR}` 从进程环境展开。
- 配置文件应为 `chmod 600`（组/其他可读时 tagma 会警告，因其可能含 API 密钥）。

### Set 选择

set 按名字寻址：root agent 绑定配置的 `default` set，每个 subagent spawn 经 `profile_set` 显式声明自己的 set（未知名字被拒）。绑定记录在 agent 记录上，set 在文件中的顺序因此不承载含义。重命名或删除 set 会让已绑定的 agent 悬空；恢复容忍占位，但 prompt 投递以 `409` 拒绝，直到该 set 以该名字回归。

`default` 标记在加载时解析：显式 `default = "<name>"` 必须指向存在的 set（匹配不到任何 set 的名字是配置错误）；恰有一个 set 且无标记时，该 set 即为 default，tagma 把标记写回文件、插在首个表头之前，注释、键序与 `${VAR}` 拼写因此原样保留；多个 set 而无标记是配置错误（显式点名一个）。手写的空标记（`default = ""`）读回为无标记，走单 set 自动标记路径而非报错。在这个角落里文件保留其显式空标记；自动标记写回被跳过，解析出的 default 仅存于内存。空的 `sets` 表以无 profile 态启动。

被选 set 的第一个 profile 是活跃模型；其余 profile 构成 set 内故障转移链。活跃 profile 终态失败（HTTP 401/403/404，或暂时性重试耗尽）时，runner 前进到 set 中下一个 profile 并重试同一轮；请求级失败（400/422）则该轮报错。活跃 profile 索引在 agent 生命周期内保持，恢复时重置为 0。

前进时，上下文窗口跟随新 profile 声明的 `max_context_window`（set 内窗口可以不同；一个 set 放不同窗口的模型是支持的）。若携带的上下文现在超过新的（可能更小的）窗口，runner 在重试前先压缩，该轮因此能在切换后存活。窗口会违反预算不变式的候选在前进**之前**被跳过（agent 因此绝不会向小窗口模型发送超限请求）；无可行候选时，链条报告为无可行窗口错误：每个候选都与预算冲突（调 `KALLIP_SUMMARY_MAX_TOKENS` / `KALLIP_PINNED_BUDGET_RATIO` 或加大窗口）。**活跃** profile 的窗口（非故障转移候选）在 spawn 时校验：违反预算不变式的窗口直接拒绝 spawn（fail-fast），而非悄悄回落。

重试预算是**逐端点**的，不是逐 profile：速率限制以端点为界，共享一个端点的两个 profile 共享一份预算。一个 profile 的暂时性重试在 `retry_timeout`（时限由 `KALLIP_RETRY_TIMEOUT_SECS` 配置，默认 300）之内**跨轮**累计。这是刻意的速率限制反压（持续失败的端点拿到更少重试，逼出故障转移或轮错误），也与故障转移前 agent 级活跃 profile 的行为一致。索引只向前走，被故障转移掉的端点其累计预算不会再咬回来。

边界情形：记录的 set 名匹配不到任何活 set 的 agent 会悬空：记录保留名字，恢复容忍占位，但 prompt 投递以 `409` 拒绝，直到该 set 以该名字回归。
