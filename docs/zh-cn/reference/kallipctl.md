---
title: kallipctl CLI 参考手册
description: 面向运维的实例管理工具 CLI 参考。
order: 75
---

`kallipctl` 通过守护进程的控制套接字管理本地 kallip 实例：spawn、adopt、
start/stop/restart、持久化 env、日志与 blob 存储维护；另有本机的平台登记表
（`kallipctl polis`，不依赖守护进程）。守护进程部署见
[部署文档](../deployment/nixos/minimal.md)；套接字的 `0600` 权限即认证。

## 用个人访问令牌 spawn

远端实例通过一次性注册码接入平台。`kallipctl spawn <slug> <workspace>
  --polis <origin>` 用访问令牌换一个注册码，只把注册码交给实例。两个位置参数
都必填；workspace 写相对路径时按当前目录解析：

```bash
kallipctl spawn dev /srv/kallipai/dev --polis https://kallip.example.com
```

令牌默认读自 `$XDG_CONFIG_HOME/kallipai/access-token`（变量未设时为
`~/.config`），也可用 `--token-file` 指向别处。在 Web 界面创建令牌时要
显式勾选 `archeion:tagma:write`：新令牌默认只读，缺这个串换不到注册码。
令牌文件只允许属主可读：

```bash
chmod 600 ~/.config/kallipai/access-token
```

`--polis` 是平台边缘源（edge origin）：实例把它读作 `KALLIPAI_POLIS_URL`，
换取注册码时也从它拼出 `/v1/archeion`。它不是某个服务的裸地址。取值必须是
http 或 https URL，不带查询串、片段、用户名和密码；明文 http 只对回环
地址（回环 IP、`localhost`、`*.localhost` 之下的名字）免开关，因为访问令牌
和换取到的注册码都会以明文过网；其它主机上的明文源要加 `--allow-plaintext`，
它是顶层参数，写在子命令之前。`spawn` 带 `--polis` 时、`polis add`、以及每个
会花令牌的 `polis` 子命令都会读它，因为地址规则在碰令牌之前就跑了；不带
`--polis` 的 spawn 用不到它。放行明文源时，命令会先在 stderr 上写明风险。

可信内网里的明文边缘要在子命令前加开关：

```bash
kallipctl --allow-plaintext spawn dev /srv/kallipai/dev --polis http://kallipai.lan:7100
```

`--polis` 会把 `KALLIPAI_POLIS_URL` 写进实例环境。再叠加
`-e KALLIPAI_POLIS_URL` 或 `-e KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE` 会被
直接拒绝，不做优先级合并。不带 `--polis` 时不写注册相关变量，离线也可用。

换取注册码发生在 `kallipctl` 里，不在实例里：令牌始终留在运维机器上。
每条失败信息都写明补救动作：

| 失败信息 | 含义 |
| --- | --- |
| no access token at PATH | 令牌文件不存在，创建后写入该路径 |
| token file PATH is readable by other users | `chmod 600` |
| token file PATH is empty | 令牌文件为空，把访问令牌写进去 |
| archeion rejected the access token | 无效、过期或已吊销 |
| the access token lacks archeion:tagma:write | 给令牌补上该字符串 |
| the token belongs to the admin console or a tagma | 换成个人访问令牌再换码 |
| archeion refused the request: 403 MESSAGE | 没有可据以补救的 code，按提示信息排查 |
| too many live pending enrollment codes | 在界面吊销闲置的待注册实例 |
| archeion refused to mint an enrollment code: STATUS MESSAGE | 平台回了失败状态码，后面的文字是平台自己的说明 |
| archeion at ORIGIN did not answer within 30s | 网络可达但一直不应答；30s 是 CLI 内置上限，没有参数可改 |
| cannot reach archeion at ORIGIN | 检查 URL 与网络 |
| archeion answered without an enrollment code | 返回的注册码是空串 |
| minting an enrollment code: DETAIL | 收到的是 2xx，但响应体读不懂，等于没有拿到可用的码 |
| --polis must be an http or https URL | 换取注册码只走 HTTP(S) |
| --polis must be an origin, without a query or fragment | 只写 scheme://host[:port] |
| --polis must be an origin, without a path | 只写 scheme://host[:port] |
| --polis must not carry a user name or password | 去掉地址里的用户名和密码 |
| --polis URL is not loopback and not https | 令牌会以明文过网；确认网络可信时加 --allow-plaintext |

## 平台登记表

`kallipctl polis` 记录本机要对话的平台，一个平台一个名字，并向平台查询它名下
登记了哪些 tagma。登记表是你自己的文件
`$XDG_CONFIG_HOME/kallipai/polis-registry.toml`，权限 `0600`；这里的动词都不经过
守护进程，所以守护进程停着也能用。

| 动词 | 作用 |
| --- | --- |
| `polis add NAME URL [--replace]` | 按 origin 规范化后记下地址 |
| `polis remove NAME` | 只删这条记录，不动别的 |
| `polis list` | 名字、地址、是否存了令牌；只读文件 |
| `polis token set NAME [--token-file PATH]` | 存下该平台的访问令牌 |
| `polis status NAME` | 平台的判定，以及它名下登记的 tagma |
| `polis revoke NAME TAGMA-ID` | 在平台上吊销一条登记 |
| `polis mint-code NAME` | 在本机铸一枚注册码，连有效期和它新建的待定 tagma 一起打印 |

登记表这组动词既不需要守护进程，也不需要实例在跑。`mint-code` 用的是登记表里的令牌，问的是平台，所以守护进程停着也能用；把这样的码交到实例，用 `tagma polis add --code`，落到已有凭据的条目上则是 `tagma polis set --code`。

名字沿用实例 slug 的语法：小写字母、数字和 `-`。一个平台一个名字、一个名字一个
平台：给已登记的地址再起一个名字会被拒，把名字改指新地址要加 `--replace`。

地址必须是 origin，规则与 `--polis` 相同（http 或 https，不带 path、query、
fragment 和用户名密码）；非本机回环的明文地址在这里同样要加
`--allow-plaintext`。`list` 只打印掩码后的令牌，密钥本身不会打印。
`token set` 给了 `--token-file` 就读文件，否则读 stdin（终端下隐藏回显），
写入之前先打一次列表请求：平台不认的令牌不会存。

令牌至少要 `archeion:tagma:read`。吊销登记和换注册码（`spawn --polis`）还要
`archeion:tagma:write`，因为新铸的令牌默认只读。`add`、`remove`、`list` 只读写
登记表，不联网。

`status` 每行一条记录，tagma id 在最前，可以直接复制进 `revoke`：

```text
local  http://api.example.com  reachable  token=accepted
tagma-1  enrolled  created=2026-11-01T09:12:00Z
tagma-2  pending  created=2026-11-11T17:40:00Z  expires=2026-11-11T18:10:00Z
```

平台拒收令牌时仍打印 `reachable`，下一行是平台的说明。对一个平台不持有的 id
执行 `revoke` 会打印 `TAGMA is not registered on NAME` 并以 0 退出，因为这正是
它被要求达到的结果。

## 运行中实例里的条目

`polis add` 记的是本机认识的平台；`tagma polis` 写的是某一台实例自己的
`polis.toml`：加一条、换地址、换凭据，并把它的 relay 起起来。这组动词要守护进程
告诉它实例监听在哪，实例本身也必须正在运行。

| 动词 | 作用 |
| --- | --- |
| `tagma polis list SLUG` | 该实例里的条目；若条目来自实例环境，也会说明 |
| `tagma polis add SLUG NAME PLATFORM [--code CODE] [--token-file PATH]` | 加一条条目；PLATFORM 是登记表里的平台名，没有现成注册码就地铸一枚，然后把 relay 起起来 |
| `tagma polis set SLUG NAME [PLATFORM] [--code CODE] [--token-file PATH]` | 改一条已存在的条目：地址（给平台名）、凭据，或两样都不改 |
| `tagma polis remove SLUG NAME [--forget]` | 删一条条目并停掉它的 relay；`--forget` 连凭据目录一起删，换凭据留下的残留也一并删 |

`add` 默认自己铸码、在同一次调用里兑换，所以码不会进 shell 历史，那三十分钟
的窗口也不用你盯着：

```bash
kallipctl tagma polis add alpha local edge
```

铸码之前，它先问实例手里已经有什么：名字已带凭据的，在不花任何注册码的情况下
拒掉并指向 `set`；条目来自环境的实例直接拒掉。没有凭据的那条正是注册码要修的。
地址只从登记表取，这一点约束的是 `kallipctl`：PLATFORM 给平台名，地址和铸码用的
令牌都来自登记表，明文地址照样要 `--allow-plaintext`（HTTP API 仍收裸 URL，
`polis.toml` 也能手改）。平台还没登记，就先 `kallipctl polis add NAME URL` 记一条，
再回来加条目。`add` 建的条目都挂在登记名下，那条登记还在时，`polis status|revoke|token set`
就服务得到它；把登记删掉、或用 `--replace` 改指别处，名字就从这些条目上撤走了。
已经用 `polis mint-code` 铸好的码，用 `--code` 交进去。

`set` 改的是 `add` 建出来的那条，自己从不新建：实例里没有这个名字会拒，条目既
没凭据也没码也会拒，后者是 `add` 的形状。

| 交什么 | 会发生什么 |
| --- | --- |
| 什么都不交 | 用盘上那对凭据把 relay 重新起起来；原本就在跑的，报条目未变 |
| 平台名 | 条目指向登记表里那个地址，盘上的凭据留着。若这条正是模型画像源钉住的那条（设置里的 `[profiles.source] polis`），实例会原样拒掉并说明先改或清掉那项设置 |
| `--code`，或用 `--token-file` 就地铸一枚 | 重新登记一次，新的一对顶掉盘上那对。旧凭据在平台上留下的登记仍在，命令会把释放它的 `polis revoke` 行打出来 |

换凭据是整目录换位，所以 `set` 能报出来的失败，盘上要么是旧的一对、要么是新的一对，
不会各剩一半。掉电是例外：新的一对文件已 fsync，装它们的目录没有，所以断电后可能是
整对不见，而不是写了一半。两次换位之间被杀，条目目录旁会留下 `<name>.new-*` 和
`<name>.old-*`，`list` 此时显示 `enrolled=off`；把其中任一个改回 `<name>` 就恢复出
一对可用凭据。对这条条目再换一次凭据会清掉残留，`remove --forget` 也会一并删掉。

`tagma polis list` 一条条目一行，两个运行状态用词写出来：

```text
local  https://edge.example.com  enrolled=on  tagma=t-9f2  relay=on  model-gateway=on
```

`model-gateway=off` 说明网关比这条条目起得早，这条条目要到实例下一次启动才进它的
平台表。写入先报条目头，再报实例自己的说明：

```text
local  https://edge.example.com  enrolled=on  tagma=t-9f2
relay connected; the model gateway's platform list is still the one from startup, so this entry joins it on the next start
```

铸了码的那一趟，会先把待定 tagma 和有效期打到 stderr，所以之后再出错，`polis revoke
NAME TAGMA` 要用的把手仍在。

请求打的是实例自己的回环端口，带的是 `kallipctl operator-token show SLUG` 指向的
运维令牌：启动时设了 `KALLIPAI_OPERATOR_TOKEN` 的实例以那个环境值为准、不读文件，
这里同样以环境值为准。实例没在跑就直接拒掉，并给出该执行的 `kallipctl start`：
这里不替人启动任何东西。

一次写入可能要等一会儿：排在前面的写者、平台的登记、被顶替的旧 relay 各有自己的
上限，所以每次实例调用等 120 秒（会铸码的 `add` 先后读一次、写一次）。真等到
超时、或读不到答复，它会说实例可能仍在完成，让你先看 `tagma polis list SLUG` 再
决定是否重试；不要先再铸一枚码，正在走的那枚可能已经花掉了。

条目来自实例环境（spawn 时带了 `--polis`）的实例只列不写：`add`、`set`、`remove`
都会被拒，得改 spawn 环境里的 `KALLIPAI_POLIS_URL` 和
`KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE`，再重启实例。

## Shell 补全

补全是动态的，分两种：实例 slug 随输入实时向守护进程查询；平台名读的是 polis
登记表，不需要守护进程。两者的来源不可用时都退化为空候选，不会阻塞输入：

```bash
echo "source <(COMPLETE=bash kallipctl)" >> ~/.bashrc
echo "source <(COMPLETE=zsh kallipctl)" >> ~/.zshrc
```

nix 安装（`kallipctl` 或 `workspace` 包）在构建期生成这些钩子脚本
并自动安装：bash、zsh、fish 落入各 shell 的标准 profile
路径，五种 shell 的脚本同时保留在 `share/kallipai/completions/kallipctl/`
下。每份钩子脚本内嵌其二进制的 nix store 路径，拷贝到其它主机会静默
失效，应在目标主机重新生成。

隐藏的 `generate` 动词输出静态（AOT）脚本：词表在构建期烧定、不查询
守护进程。nix 安装不为 kallipctl 使用它们；适合补全时无法运行 CLI
的封闭环境：

```bash
kallipctl generate bash    # 可选 zsh、fish、elvish、powershell
```
