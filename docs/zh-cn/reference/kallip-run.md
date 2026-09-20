---
title: kallip-run CLI 参考手册
description: kallip-run 运行器的 CLI 参考。
order: 80
---

向 tagma agent 投递一条 prompt 并观察其运行：把 agent 的执行过程流式输出到 stderr，在 agent 转入 idle（调用 `break`）或进入终态错误时以语义化退出码结束。为脚本化与自动化工作流设计。

`kallip-run` 是**运行时遥测观察器**：它**不**打印 agent 的消息。消息现在是专门的 `kallip lesche send` CLI 调用，由 agent 经中继/聊天路径发给用户，不是这条流上的值。`kallip-run` 给你的是执行过程（推理、工具调用，`--verbose` 下含结果）与机器可读的退出状态。

默认情况下 prompt 发给 tagma 的**单例 root agent**（tagma 启动时主动创建）。传 `--agent <ID>` 改为定向某个具体（子）agent，需要隔离时对专职 subagent 运行很有用，因为对 root 的多次运行共享其上下文。目标 agent 在运行结束后保留。

```bash
kallip-run [OPTIONS] --prompt <PROMPT>
```

使用 `KALLIP_AUTH_TOKEN`（必填）与 `KALLIP_TAGMA_URL`（环境变量，默认 `http://127.0.0.1:3000`）。

## 选项

| 标志                | 说明                                                          |
| ------------------- | ------------------------------------------------------------- |
| `--prompt <PROMPT>` | 发给 agent 的 prompt（必填）                                  |
| `--agent <ID>`      | 按 id 定向显式 agent，而非 tagma root                         |
| `--json`            | 在 stdout 输出单个 JSON 对象（见输出）                        |
| `--verbose`         | 把 agent 的执行过程（推理、工具调用）流式输出到 stderr        |

### 退出码

| 码  | 含义                          |
| --- | ----------------------------- |
| 0   | 成功（agent 转入 idle / `break`） |
| 1   | 错误                          |
| 2   | 超过最大轮数                  |
| 3   | 已取消                        |
| 4   | 超出 Token 预算                  |
| 5   | 故障转移链耗尽                |

### 输出

输出形态由 `--json` 与 `--verbose` 驱动（没有基于 TTY 的自动检测）。tagma 已经持久化了 agent 的完整执行历史，agent 经 `kallip lesche send`（而非这条流）与用户对话，因此运行器默认只发一条完成提示。

| `--json` | `--verbose` | stdout            | stderr                                                                                |
| -------- | ----------- | ----------------- | ------------------------------------------------------------------------------------- |
|          |             | _（无）_          | 完成提示（agent id + 如何继续）                                                       |
|          | `--verbose` | _（无）_          | 执行过程（`[reasoning]`/`[assistant]`/`[tool]`/`[tool-result]`/`[retry]`）+ 提示      |
| `--json` |             | `{agentId, exit}` | 诊断信息                                                                              |
| `--json` | `--verbose` | `{agentId, exit}` | 执行过程流 + 诊断信息                                                                 |

- stdout 不打印任何消息。agent 的裸 assistant 文本只是执行过程：`--verbose` 下以 `[assistant]` 前缀流入 stderr；它不是用户消息。
- JSON 对象**绝不含 `reasoning` 或用户消息**；`--verbose --json` 把执行过程流入 stderr，对象本身不变。
- 警告与错误一律走 stderr。

`--json` 示例：

```json
{
  "agentId": "a3f1b2c4-5678-90ab-cdef-1234567890ab",
  "exit": "success"
}
```

`exit` 取值为 `success`、`error`、`max_rounds`、`cancelled`、`budget_exceeded`、`failover_chain_exhausted` 之一。tagma 不可达、agent id 未知或 `post_message` 失败时，不输出 JSON 对象：错误打印到 stderr，退出码为 `1`。

```bash
kallip-run --json --prompt "Refactor the config loader"
```

### 继续会话

目标 agent 在运行结束后保留。其 id 打印在完成提示里，可以继续同一会话：

```text
$ kallip-run --prompt "Refactor the config loader"

agent a3f1b2c4-5678-90ab-cdef-1234567890ab went idle. Continue with: kallip-run --agent a3f1b2c4-5678-90ab-cdef-1234567890ab --prompt "<prompt>"
$ kallip-run --agent a3f1b2c4-5678-90ab-cdef-1234567890ab --prompt "and add a test"
```

传 `--verbose` 可旁观 agent 的推理与工具调用：

```bash
kallip-run --verbose --prompt "Refactor the config loader"
```

经 `--agent` 的后续运行保留 agent 的完整上下文。只对仍注册着该 agent 的 tagma 有效：同一实例，或启动时从磁盘恢复它的实例。`--agent` 不校验 id 格式；未知 id 表现为 tagma 错误。

完整的环境变量参考（含 LLM provider 配置）见 [模型配置](../configuration/tagma/llm.md)。
