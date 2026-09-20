---
title: 认证与授权
description: KallipAI 各服务的令牌类型与授权模型。
order: 20
---

## 令牌类型

所有进入 KallipAI 的请求都以 bearer 令牌认证。每种令牌的前缀一眼标明其类别，泄露的凭据因此能自我标识，密钥扫描器也容易标记它。

- **operator 令牌**（`sk-operator-…`）：tagma 启动时打印一次。授予对一个 tagma 的完全控制：管理任意 agent、批准或驳回 approval。tagma 唯一的 root agent 在启动时创建，从不经 API 创建。
- **agent 令牌**（`sk-agent-…`）：agent 创建时逐个签发，以 `KALLIP_AUTH_TOKEN` 注入该 agent 的 shell。agent 用它回调 tagma；人不会经手它。

平台只存储令牌材料的哈希，绝不存储密钥本身。

### 角色

- **Supervisor**（直接父级）：创建调用者的那个 agent。
- **Superior**（上级）：创建链上的任意祖先（supervisor、supervisor 的 supervisor，依此类推）。
- **Self**（自身）：agent 本身（身份与目标 agent 一致）。
- **Root agent**：tagma 中唯一没有创建者的 agent。由 tagma 自管（启动时从配置创建），从不经 API 创建或移除。

### 认证面

平台的每个面以不同方式认证：

- **Web 应用**：人在 web 面上注册与登录；平台签发浏览器会话，授予用户作用域的面（profiles、tagma 登记与管理）。
- **tagma 管理**：operator 令牌授权 tagma 的管理 API。
- **平台管理**（本地部署）：admin 令牌（`sk-admin-…`）由 operator 钉死，授权登记、用户管理等平台管理操作；登录时可交换为 admin web 会话。
- **agent 回调**：注入每个 agent shell 的 agent 令牌，向 tagma 认证该 agent（以及在其内运行的 CLI 命令）。
