---
title: Tagma
description: tagma 服务端、其 agent 与 LLM 后端的配置面。
order: 10
---

tagma 的配置住在环境变量与模型配置文件里，分页呈现，另有单行速查索引：

- [环境变量](environment-variables.md)：所有变量一表纵览，每变量一行，链接到所属小节。
- [模型配置](llm.md)：provider、profile 与 profile set 三层模型，以及每层单独存在的设计原因。
- [模型配置方法](methods.md)：用网页界面、`kallip profile-set` 命令或手动编辑 `profiles.toml` 配置模型。
- [Agent core 与 shell](agent.md)：agent 运行时调优，以及注入 agent shell 会话的变量。
- [Tagma 服务](service.md)：监听地址、数据与 skills 根、日志。
