---
title: 模型配置
description: provider、profile 与 profile set 三层模型，以及每层单独存在的设计原因。
order: 20
---

[配置参考](../index.md)的一部分；本页讲模型配置的三层抽象。

provider 选择变量：[Tagma](../../reference/environment-variables/tagma.md)。

## 三层模型

模型配置分三层。**Provider** 是一个后端端点：协议族（`family`）、密钥（`api_key`）与可选的 `base_url`，回答“请求发给谁”。**Profile** 把一个模型绑定到一个 provider 端点，并声明该组合的能力：上下文窗口（`max_context_window`）与支持的模态（`modalities`），回答“用哪个模型、它声明了什么”。**Profile set** 是命名的 profile 组，组内构成故障转移链，回答“agent 绑定到哪一组”。

三层的关系：agent 不直接绑定单个 profile，而是绑定一个 profile set。root agent 用配置的 `default` set，每个 subagent 在 spawn 时经 `profile_set` 显式声明自己的 set。set 内第一个 profile 是活跃模型，其余构成故障转移链。

每层单独存在都有明确的设计原因。

**Provider 独立成层，因为速率限制与重试预算以端点为界。** 速率限制是端点级行为：同一个端点上的多个 profile 共享一份重试预算，而不是各算各的。把端点信息收拢进 provider，预算的归属就自然落在这一层。

**Profile 独立声明能力，因为故障转移时上下文窗口要跟随新 profile 的声明。** set 内各 profile 的窗口可以不同；切换发生时，上下文按新 profile 声明的窗口处理，预算不变式的校验也依赖这份声明值。窗口因此是 profile 的属性，不是全局常量。

**Agent 绑定的是 set 而非单个 profile，因为故障转移要在 set 内前进。** 活跃 profile 终态失败时，set 内下一个 profile 接手；恢复时活跃索引重置。绑定单个 profile 就没有这条前进路径。

配置文件、字段与运行细节（示例、default 解析、悬空 set、重试预算的细节）见[模型配置方法](methods.md)。
