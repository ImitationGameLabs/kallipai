---
title: 简介
description: KallipAI 是什么、名字从哪里来，以及各核心部分如何协作。
order: 5
---

KallipAI 是面向多智能体协作、长程任务和规模化设计的智能体运行框架（Agent Harness）。

## 名字的由来

名字源自 kallipolis（希腊语 kalon（美）加 polis（城）），柏拉图《理想国》中的理想城邦：每个角色做各自精确的工作，整座城如单一有序的有机体运转。这也是这里多 agent 协作的想象：有结构、有分工的协作，每个 agent 都有自己的角色。

日常使用取其简化词干 kallip（kallipolis 去掉不做技术工作的 -olis），再加 AI 后缀，得到 kallipai。

## 核心概念：tagma

tagma（τάγμα，希腊语中指有编制的队伍）是一个 agent 团队的运行时，由一个常驻服务端进程承载。团队协作所需的一切都是内置的。

你可以同时运行多个 tagma，一个团队一个。每个 tagma 有自己的 agent、workspace 与数据目录，各团队相互独立。客户端断开后 agent 继续运行，长任务不随任何单个会话一起结束。

## 下一步

每个团队运行在自己的 tagma 里。Polis（托管平台）把 tagma 连到远程客户端。

从[部署](deployment/index.md)把平台跑起来，或从[配置](configuration/index.md)与[框架设计](harness-design/index.md)了解运行方式；日常操作见 [kallip CLI 参考手册](reference/kallip.md)。
