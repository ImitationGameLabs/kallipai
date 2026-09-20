---
order: 5
title: NixOS
description: 用随附的 NixOS 模块部署 KallipAI 主机。
---

本指南走完在 NixOS 主机上部署 KallipAI 平台形态的全过程：daemon 作为系统服务运行，四个 polis 服务（archeion：身份控制面；lesche：中继数据面；files：内容传输；instances：web 实例代理）位于主机反向代理之后，外加 web 应用。容器形态（arion compose）是与 NixOS 模块形态互补的另一条路径；本指南是 NixOS 模块形态的一步步启动过程。模块本体在 `nix/nixos-modules.nix`，本指南概括的一切以其中的 option 描述为权威参考。

指南分三章，按首次部署的阅读顺序排列：

- [基础部署](minimal.md)：前置条件、模块导入、一份基础配置，以及首次启动与登录。
- [HTTPS 配置](https.md)：ACME 无法签发时的两条升级路径。
- [服务模型与运维](operations.md)：内部令牌、启动身份，以及部署底层的 real-root 防护。

完整的 option 面（逐服务调优、`adminTokenFile` 与 `notifyTokenFile`）见 `nix/nixos-modules.nix` 中的 option 声明。web 站点根辅助函数在 `nix/lib.nix`。
