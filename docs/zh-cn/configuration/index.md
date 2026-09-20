---
title: 配置
description: 平台的配置方式，从环境变量到模型 profiles 文件与 dev 栈。
order: 10
---

运行时配置以环境为先：每个组件都在启动时从 `KALLIP_*` 变量读取设置。LLM 后端还可经 profiles 配置文件调优（方法见[模型配置方法](tagma/methods.md)），NixOS 部署则经[模块 option](../deployment/nixos/index.md) 配置服务。

把 `.env.example` 复制为 `.env`，填入必填值。用 `direnv` 的话，它会经 `.envrc` 自动加载 `.env`。

Tagma 的配置参考见 [Tagma](tagma/index.md)；其余组件（cron、files 服务、本地 daemon）的环境变量在[环境变量参考](../reference/environment-variables/index.md)。

## Dev 栈形态

三个变量同时驱动 dev compose（`compose/dev/polis.nix`）与 web dev server（都从根 `.env` 经 direnv 流入）：

| 变量               | 默认           | 用途                                                                                                                                                    |
| ------------------ | -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIP_EDGE_TLS`  | `on`           | 边缘形态：`on` = Caddy 前置的 https+域名拓扑，证书出自 mkcert；`off` = 纯 http（无需证书/DNS 信任设置；见 docs/en/development/setup.md）。               |
| `KALLIP_EDGE_PORT` | `443`          | dev 边缘监听端口（非默认时作用于 compose caddy 与 web dev server）；浏览器侧的 web origin 携带它，CORS/oauth 原样从它推导。                             |
| `KALLIP_DOMAIN`    | `kallipai.com` | dev server 与 compose 拓扑推导所用的域名（两种边缘形态皆然；纯 http 快速上手显式设 `localhost`）；web 应用的 URL 在浏览器运行时推导。                    |
