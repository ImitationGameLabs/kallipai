---
title: 计划任务（cron）
description: 计划任务服务（cron）的配置。
order: 40
---

一个小型服务，按时触发计划并把每次触发注入目标 agent 的会话；另有 `kallip-cron` 命令行客户端，agent 用它管理自己的计划。客户端运行在 agent shell 内，复用 shell 注入的凭据（`KALLIP_ID` + `KALLIP_AUTH_TOKEN`）；每个操作都限定在该 agent 自己的计划上，管理 API 只监听环回。

| 变量                     | 必填      | 默认                                | 说明                                                                                                                                       |
| ------------------------ | --------- | ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `KALLIP_CRON_ADDR`       | 否        | `127.0.0.1:3010`                    | daemon 管理 API 的监听地址。**仅环回**：cron 是 tagma 侧的内部服务，daemon 拒绝非环回绑定。                                                |
| `KALLIP_CRON_DATA_DIR`   | 否        | 平台数据目录 + `kallipai/cron/`     | 服务的计划数据库所在目录。未设置且无可判定的平台数据目录时，服务快速失败而非猜测。                                                          |
| `KALLIP_CRON_TICK_MS`    | 否        | `1000`                              | 调度器 tick 间隔（毫秒）。必须 `>= 1000`（秒级精度调度器）。                                                                               |
| `KALLIP_CRON_DELIVER_MS` | 否        | `500`                               | 投递器轮询间隔（毫秒）：被触发的计划多久推送一次到 tagma。                                                                                 |
| `KALLIP_CRON_URL`        | 否        | `http://127.0.0.1:3010`             | `kallip-cron` CLI 客户端使用的 daemon URL。                                                                                                |
| `KALLIP_ID`              | 是（CLI） | _（未设）_                          | 调用 agent 的 id（tagma 自动注入 agent shell）；CLI 把它作为自作用域传递，daemon 经 tagma 对 bearer 核验。                                  |
| `KALLIP_TAGMA_URL`       | 是        | `http://127.0.0.1:3000`             | 投递与逐请求核验用的 tagma URL。自 tagma 客户端复用；不带 `KALLIP_CRON_*` 前缀。                                                           |
| `KALLIP_AUTH_TOKEN`      | 是        | _（未设）_                          | daemon 投递用的 operator 密钥（触发的提醒渲染 `[From: operator]`）；CLI 管理请求用的 agent bearer。自 tagma 客户端复用。                    |
