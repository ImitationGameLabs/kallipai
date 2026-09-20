---
title: Files 服务
description: files 服务的配置。
order: 50
---

files 服务存储内容：内容寻址的 blob 存储，记录元数据放在自己的数据库里；另有日常使用的 `kallip file` 命令行客户端。客户端从 agent shell 的环境读凭据；没有标志携带密钥。平台服务之间的凭据由部署自动供给，无需手工配置。

| 变量                           | 必填       | 默认             | 说明                                                                                                                 |
| ------------------------------ | ---------- | ---------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `KALLIP_FILES_ADDR`            | 否         | `127.0.0.1:7400` | 服务监听地址（坐在 TLS 终结反向代理之后）。                                                                             |
| `KALLIP_FILES_BLOB_ROOT`       | 是（服务） | _（未设）_       | 内容寻址 blob 存储的根目录；按需创建。                                                                                  |
| `KALLIP_FILES_DATABASE_URL`    | 是（服务） | _（未设）_       | 元数据存储的 Postgres URL；缺失则启动时快速失败。                                                                       |
| `KALLIP_FILES_MAX_BODY_SIZE_MB` | 否        | `100`            | 接受的最大上传体积（兆字节）；更大的流以 413 截断。                                                                     |
| `KALLIP_FILES_DEGRADE`         | 否         | `closed`         | archeion 降级姿态：`closed` 在注册表无法应答时以 503 使授权失败；`soft` 降级为拒绝（403）。两种姿态都不削弱凭据核验。    |
| `KALLIP_FILES_GC_INTERVAL_SECS` | 否        | `60`             | GC 轮次（清扫 + 对账）之间的间隔，秒。                                                                                  |
| `KALLIP_FILES_GC_GRACE_SECS`   | 否         | `60`             | 零引用计数行被释放多久后 GC 才可回收。                                                                                  |
| `KALLIP_FILES_GC_BATCH`        | 否         | `128`            | 每次 GC 回收的编目行上限。                                                                                              |
