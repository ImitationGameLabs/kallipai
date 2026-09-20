---
title: Agent shell 会话
description: tagma 注入每个 agent shell 会话的变量。
order: 20
---

## 注入 agent shell 会话的变量 {#variables-injected-into-agent-shell-sessions}

tagma 把这些注入每个 agent 的 shell 环境，agent shell 里运行的 CLI 命令因此能与 tagma 通信。它们不经 operator 设置；tagma 自动提供。

| 变量                         | 注入点                              | 说明                                                                                                                                                                                                                                                                                                                                                  |
| ---------------------------- | ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `KALLIP_TAGMA_URL`           | Tagma 进程（`main.rs`）             | 启动时经 `set_var` 从 `KALLIP_ADVERTISE_URL` 复制。被子进程继承。CLI 客户端读取以连接。                                                                                                                                                                                                                           |
| `KALLIP_AUTH_TOKEN`          | 逐 agent shell（`routes/agent.rs`） | 生成的 256 位 `sk-agent-…` 认证令牌。注入 shell 会话，agent 得以回调 tagma；tagma 只存储并比较其哈希。CLI 必需。                                                                                                                               |
| `KALLIP_ID`                  | 逐 agent shell（`routes/agent.rs`） | 当前 agent 的 UUID。agent shell 内可用。CLI 的 `skill` 与 `subagent` 子命令读取（用于标识执行操作的 supervisor），也是 `activity` 与 `lesche send` 的自目标。                                      |
| `KALLIP_SUPERVISOR_AGENT_ID` | 逐 agent shell（`routes/agent.rs`） | 该 agent 的 supervisor id（直接 `created_by` 委托者）。仅为 subagent 注入：**root agent 未设**（缺席，而非空），root 身份因此可由环境缺席检出。暴露 id 让 agent 能寻址 supervisor（如 `kallip message <id>`）；CLI 以位置参数取 id，不读此变量。 |
| `KALLIP_ROOT_AGENT_ID`       | 逐 agent shell（`routes/agent.rs`） | tagma root agent id（root 自己则等于自身）。注入每个 agent 的 shell。暴露 id 让 agent 能上报 root（如 `kallip message <id>`）；CLI 以位置参数取 id，不读此变量。                                       |

`KALLIP_TAGMA_URL` 同时由 tagma 在启动时从 `KALLIP_ADVERTISE_URL` 自行设置；两者之别见 [Tagma 服务](../../configuration/tagma/service.md)。
