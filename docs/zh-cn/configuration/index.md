---
title: 配置
description: 平台的配置方式，从环境变量到模型 profiles 文件与 dev 栈。
order: 10
---

运行时配置以环境为先：每个组件都在启动时从 `KALLIPAI_*` 变量读取设置。LLM 后端由实例的 `profiles.toml` 承担（文件形状与写法见[模型配置方法](tagma/methods.md)），NixOS 部署则经[模块 option](../deployment/nixos/index.md) 配置服务。

三份示例随仓发行：仓根 `.env.example` 属 tagma 自身，`compose/.env.example` 属栈，`compose/.env.prod.example` 给 prod 部署经 `KALLIPAI_ENV_FILE` 指名的服务 env 文件。dev 用前两份，各复制到同目录的 `.env` 并填入必填值；用 `direnv` 的话，它会经 `.envrc` 把这两份都读入。

Tagma 的配置参考见 [Tagma](tagma/index.md)；其余组件（cron、files 服务、本地 daemon）的环境变量在[环境变量参考](../reference/environment-variables/index.md)。

## Dev 栈形态

三个变量共同决定 dev 栈的边缘与 web dev server 的形态，另三个替换边缘实际服务的东西，还有一个指名浏览器输入的 origin（栈值只存 `compose/.env` 一份，经 direnv 流入 shell）：

| 变量               | 默认           | 用途                                                                                                                                                    |
| ------------------ | -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIPAI_EDGE_TLS`  | `on`           | `deno task dev` 的 HMR 协议：`on` = `wss`（前面是 TLS 边缘）；`off` = `ws`（前面是纯 http 边缘）。它不决定容器跑哪份入口。 |
| `KALLIPAI_EDGE_PORT` | `443`          | dev 边缘的监听端口。compose 把它写进边缘的站点地址（`*.<域名>:<端口>`）与 API base。浏览器侧的 origin 是 `https://app.<域名>`（不带端口），所以换端口起栈必须显式给 `KALLIPAI_APP_ORIGIN`。vite 用它作 HMR client 端口。 |
| `KALLIPAI_APP_ORIGIN` | `https://app.<域名>` | 浏览器为应用输入的 origin（协议、主机与端口），CORS、OAuth 回调与 WebAuthn 依赖方都比对它。`compose.dev.yaml` 在 https、443 的场景下从 `KALLIPAI_DOMAIN` 推导；换主机、换端口或纯 http 入口时必须显式给；写了就按写的用，脚本不比对。 |
| `KALLIPAI_DOMAIN`    | `kallipai.com` | 边缘站点、API base 与浏览器侧各值所用的域名；web 应用的 URL 在浏览器运行时推导。 |
| `KALLIPAI_EDGE_ENTRY` | `/etc/caddy/Caddyfile.vite` | dev 边缘容器跑哪份入口。`/etc/caddy/Caddyfile.plain` 用 http 提供镜像内的 bundle，且不挂证书；生产跑镜像自带的 `Caddyfile`。 |
| `KALLIPAI_EDGE_SITE` | `*.<域名>:<端口>` | 替换边缘的站点地址。https 边缘下这里若写端口，必须等于 `KALLIPAI_EDGE_PORT`：API base 与 HMR client 端口都跟那个值。不带端口的地址按 443 应答，所以 `KALLIPAI_EDGE_PORT` 不是 443 时这里必须写出端口；两种不一致脚本都会拒绝。纯 http 入口的端口就写在这个地址里，不参与这项检查，且需要给一个 `http://` 地址。 |
| `KALLIPAI_API_BASE`  | `https://api.<域名>:<端口>` | 浏览器拿到的 API 基址，同时也是边缘服务它的地址：同一个值，两者无法分叉。 |
