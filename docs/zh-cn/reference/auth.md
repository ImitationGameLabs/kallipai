---
title: 认证与授权
description: KallipAI 各服务的令牌类型与授权模型。
order: 20
---

## 令牌类型

所有进入 KallipAI 的请求都以 bearer 令牌认证。每种令牌的前缀一眼标明其类别，泄露的凭据因此能自我标识，密钥扫描器也容易标记它。

- **operator 令牌**（`sk-operator-…`）：tagma 启动时以 0600 权限持久化到 `credentials/operator-token.env`（用 `kallipctl operator-token show <slug>` 取回；启动流程不打印令牌，`operator-token reset` 按操作者明确要求打印一次新生成的值）。授予对一个 tagma 的完全控制：管理任意 agent、批准或驳回 approval。tagma 唯一的 root agent 在启动时创建，从不经 API 创建。
- **agent 令牌**（`sk-agent-…`）：agent 创建时逐个签发，以 `KALLIPAI_AUTH_TOKEN` 注入该 agent 的 shell。agent 用它回调 tagma；人不会经手它。
- **tagma 凭据**（`sk-tagma-…`）：tagma 登记时签发，每个 tagma 一条记录，携带一份权限清单而非全部权力。所有者可改名、改清单或重置；重置当场作废旧哈希，新密钥只显示一次。
- **个人访问令牌**（`sk-pat-…`）：由已登录用户在 web 面签发，带名字、权限清单与有效期（30、90 或 365 天，默认 90）。吊销后记录仍留在列表里，并带失效时间。

平台只存储令牌材料的哈希，绝不存储密钥本身。

### 角色

- **Supervisor**（直接父级）：创建调用者的那个 agent。
- **Superior**（上级）：创建链上的任意祖先（supervisor、supervisor 的 supervisor，依此类推）。
- **Self**（自身）：agent 本身（身份与目标 agent 一致）。
- **Root agent**：tagma 中唯一没有创建者的 agent。由 tagma 自管（启动时从配置创建），从不经 API 创建或移除。

### 认证面

平台的每个面以不同方式认证：

- **Web 应用**：人在 web 面上注册与登录；平台签发浏览器会话，授予用户作用域的面（profiles、tagma 登记与管理）。
- **个人访问令牌**：给需要在浏览器会话之外自带凭据的人，在 web 面签发；它以签发者本人的身份，在清单里写的那几条路由上访问注册表与文件服务，其它服务一律不收。
- **tagma 凭据**（`sk-tagma-…`）：tagma 登记时签发，一台 tagma 一条活记录，带一份权限清单。清单管它在文件服务上的调用；房间流量按它的登记状态走。
- **tagma 管理**：operator 令牌授权 tagma 的管理 API。
- **平台管理**（本地部署）：admin 令牌（`sk-admin-…`）由 operator 钉死，授权登记、用户管理等平台管理操作；登录时可交换为 admin web 会话。
- **agent 回调**：注入每个 agent shell 的 agent 令牌，向 tagma 认证该 agent（以及在其内运行的 CLI 命令）。

## 权限清单

凭据携带一组权限串，范围内的每条路由都点名它需要的那一条。读这份清单的只有两个服务：注册表（`archeion:*`）与文件服务（`files:*`）。共六条串，三个资源各有读写一档：

| 串                      | 守住                                                    |
| ----------------------- | ------------------------------------------------------- |
| `archeion:tagma:read`   | 列出所有者的 tagma、读其中一个                           |
| `archeion:tagma:write`  | 给所有者的 tagma 改名或吊销                              |
| `archeion:account:read` | `GET /me`，调用者自己的身份                             |
| `files:record:read`     | 列出、下载、读记录元数据                                |
| `files:record:write`    | 上传与删除记录                                          |
| `files:delivery:write`  | 把记录投递到另一个主体的收件箱                          |

- registry 在自己的路由上查 `archeion:*`；files 服务在内容路由上查 `files:*`，且排在其按路径的 ACL 之前。路由要的那条串在清单里就通过；不在就听到缺的是哪条。
- 其它服务判的是身份与房间成员关系，不看串：keryx 与 relay 直接以 `403` 拒收个人令牌；tagma 的房间流量按它的登记状态走，与清单无关。
- `401` 说明这一趟没有可用的凭据：哈希未知、已过期、已吊销、所属身份被停用，或清单里带着本版本不认识的串（这条记录读不成一把钥匙）。`403` 说明凭据被认出来，而这条路由需要的串它没有。
- 空清单是 tagma 的所有者可以有意造出的状态：它在文件服务上的调用仍能认证，然后逐条被拒绝。tagma 无论清单里有什么都够不到注册表路由。个人令牌创建时至少要带一条串（否则注册表回 `400`）。结束一根凭据靠吊销或重置，不靠清空清单。
- 改动在下一次请求上生效；没有任何环节缓存清单。
- 重置是唯一一处「下一次请求就够」不成立的地方：registry 当场作废旧哈希，而运行中的 tagma 仍举着它读到的密钥。把新值写进它的凭据文件：relay、files 与网关调用在被拒时会重新读一次文件，下一趟请求就用新值；因旧密钥停摆的网关源，在下一次 spawn 闸门检查时就不再拒绝新 spawn（每次检查都会重读一次凭据文件），拉取本身则在下一轮拉取时恢复（两轮之间的退避最长五分钟），或在一次 `POST /profiles/refresh` 时恢复；两种情形都不需要重启。
- tagma 登记默认带三条 `files:*` 串；新建的个人令牌预勾 `archeion:tagma:read`、`archeion:account:read`、`files:record:read`。
- 不携带记录的认证身份不受清单收窄：浏览器会话与 admin 令牌都过每一条串的检查。

签发、改清单、重置凭据，passkey，邮箱地址，OAuth 关联与解绑，设备配对，登出只属于会话面：个人令牌无论带多少条串，在这些路由上都得到 `403`，因为能签发凭据或添加一种登录方式的凭据可以给自己升权。

`POST /tagmata`（换注册码）不在这张名单上：它点名 `archeion:tagma:write`，带这条串的个人令牌可以为自己的所有者在待注册上限内换码；admin 令牌过得了这条串的检查，随后停在用户检查上，因为它不属于任何用户。两个 `403` 靠错误里的 `code` 区分：令牌缺串是 `perm_missing`，主体不是用户是 `user_required`。
