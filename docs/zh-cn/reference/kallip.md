---
title: kallip CLI 参考手册
description: 无头 kallip 客户端的 CLI 参考。
order: 70
---

这是 agent 用来与其他 agent 协作、管理自己的 subagent 与运行时事务的 CLI。

所有子命令都使用 `KALLIPAI_AUTH_TOKEN`（必填）与 `KALLIPAI_TAGMA_URL`（环境变量，默认 `http://127.0.0.1:3000`）。

## 子命令

### `message`：向 agent 发送消息

```bash
kallip message <ID>
```

把消息送进 agent 的输入队列。消息文本从完整 stdin 读取（支持多行：管道、heredoc、`< file` 都可以）；没有文本参数，shell 展开因此无法破坏消息。tagma 立即受理（202 Accepted）并异步处理。轮询 `status` 观察结果。成功时 CLI 打印一行受理文本的 JSON 回显（失败时什么都不打印，失败的发送绝不伪装成已送达）：

```json
{"kallipai.message.sent":{"to":"<id>","text":"<message>","queue_depth":0}}
```

`queue_depth` 统计排在这条消息之前的条数（0 = 立即处理）；tagma 也可能附带一条 `"warning"` 注记（例如消息为下班状态的 agent 暂存时）。回显键与 lesche 标记刻意不同，本地客户端因此不会把 agent 间发送渲染成用户聊天行。

建议用引号包裹的 heredoc（`<<'EOF'`，定界符带引号），shell 就完全不做展开：反引号与 `$` 保持字面量，多行文本无需转义。程序化生成的文本也可以走管道（`echo 'text' | kallip message <ID>`）。空 stdin 发送空消息，成功回显会让这一点立刻可见。

```bash
# Backticks and $ stay literal inside a quoted heredoc.
$ kallip message "$AGENT_ID" <<'EOF'
Run `cargo test` and report $CARGO_TARGET_DIR.
EOF

# Or pipe it:
$ echo 'List all TODO comments in src/' | kallip message "$AGENT_ID"
```

### `status`：查看 agent 上下文用量

```bash
kallip status <ID>
```

打印该 agent 的上下文 Token 用量与近期重试历史。

不带参数时改为列出本 tagma 上的全部 agent：每个 agent 一块，异常在前。

单 agent 视图由若干空行分隔的分组组成——时钟、生命周期状态、上下文与预算、重试记录、上游事实、profile 集，以及该 agent 的 shell 正在跑的后台任务。后台任务组属于**宿主侧**信息：tagma 只把它发给 operator 与 root agent，对其他调用者整个字段不出现，所以「字段存在之前的旧服务端响应」与「无权限调用者的响应」逐字节相同。每个任务块给出 id、state、命令、工作目录、已运行时长与已捕获输出体积；列表达到上限时报 `20+`，那是下限而不是确切数量。

### `subagent`：管理直接 subagent

```bash
kallip subagent <subcommand> [args]
```

管理**当前 agent 的直接 subagent**。执行操作的 supervisor 取自 `KALLIPAI_ID` 环境变量，这些命令只在 agent 上下文里有意义，未设置时报错。`subagent` 是唯一的管理入口；spawn、list、remove、interrupt、改标签全部经此。

| 子命令                       | 用途                                               |
| ------------------------- | ------------------------------------------------ |
| `subagent spawn`          | 创建直接 subagent（必填标志见下文说明）。                        |
| `subagent list`           | 列出当前 agent 的直接 subagent。                         |
| `subagent remove <ID>`    | 移除直接 subagent。                                   |
| `subagent interrupt <ID>` | 中断直接 subagent 当前操作。                              |
| `subagent metadata <ID>`  | 更新直接 subagent 的 role/description。                |
| `subagent workspace relocate <ID> <PATH>` | 把子代理的 workspace 迁到已备好的目录。 |
| `subagent workspace mv <ID> <PATH>` | 先停泊代理，再搬 workspace 目录本身。 |

作用域说明（服务端强制）：

- `subagent metadata` 限于**直接 supervisor**（`require_direct_supervisor`）；祖父级不能给孙辈改标签。
- `subagent remove` / `subagent interrupt` 授权**任意祖先**（`require_superior`），所以这里“直接 subagent”的表述只是 CLI 便利，不是服务端限制。
- `subagent spawn` 要求 `--workspace-root`、`--profile-set` 与 `--permission-class`；未知的 profile set 名称在 spawn 时被拒。`--role` 对每次 subagent 创建都要求非空。
- `subagent spawn --permission-class {normal,guest}` 显式**降级** subagent 的 FS 访问等级，使其低于父级自身等级（如 `normal` 父级创建只读 `guest` 评审者）。高于父级等级的取值被 tagma 以 `403` 拒绝。该标志必填。被授予的等级可由 `kallip`/`GET /agents/{id}/permissions` 查看。
- `subagent workspace`（两个动词）保留给 **operator 或 root 代理**；忙碌中，或仍有活跃子代理的代理会被拒绝。细节与前缀缓存说明见下方小节。

```bash
$ kallip subagent list
researcher  idle  ws=/projects/frontend
$ kallip subagent spawn --role reviewer --workspace-root /projects/reviews --profile-set primary --permission-class guest --description "reviews PRs" < /dev/null
b4c2d3e5-...
```

spawn 从 stdin 读取可选的初始 prompt：上面的 `< /dev/null` 表示“无 prompt”，id 被捕获时也避免 spawn 吞掉外层脚本的 stdin。要传 prompt 就用管道或 heredoc。

#### `subagent workspace`：迁移子代理的 workspace

`kallip subagent workspace relocate <ID> <PATH>` 一条命令完成迁移：停泊（deactivate）、改写 workspace 记录、恢复（restore）。目标目录必须已经放好该代理的文件（空目标会被拒绝）；该动词本身不搬文件。

`kallip subagent workspace mv <ID> <PATH>` 先停泊代理，再把记录中的 workspace 目录原样搬到新路径（普通 rename：源目录消失；跨文件系统的搬移会被拒绝），然后改写记录并恢复。

两个动词都幂等。每一步先探测当前状态，重跑会从上次停下的位置续行。在停泊这一步被拒（忙碌中，或仍有活跃子代理），代理保持原态；停泊之后任何一步失败，代理都停在冷停泊态，不会半恢复。恢复时代理的 system prompt 会按新路径重建，前缀缓存随之失效；这是切换 workspace 后的预期行为，不是故障。

### `profile-set`：管理命名 profile set

运行时检视并调整 `profiles.toml` 的命名 profile set（见[模型配置方法](../configuration/tagma/methods.md)）：

```bash
$ kallip profile-set list
default set: primary
primary: 2 profile(s), 3 agent(s), modalities: text
cheap: 1 profile(s), 0 agent(s), modalities: text
$ kallip profile-set bind reviewer cheap
Bound reviewer to profile set cheap.
```

`list` 打印 default 标记、各 set 的 profile 与 agent 计数，以及每个 set 的生效模态（成员声明的交集）。`bind` 在 agent 下次唤醒时生效（停泊的 agent 在下次恢复时取新绑定）。

### `approval`：管理 approval

列出、检视、回应 approval 的子命令（执行前需要 supervisor 批准的工具动作）。

#### `approval list`：列出 approval

```bash
kallip approval list [--offset <N>] [--limit <N>] [--requested-by <ID>] [--status <STATUS>] [--all] [--reverse]
```

列出已认证身份可见的全部 agent 的 approval。默认显示已提交待批的动作；用 `--all` 看所有状态，或用 `--status` 过滤特定状态（committed、approved、denied、redeemed、cancelled）。

```bash
$ kallip approval list --limit 5 --status committed
```

#### `approval get`：查看 approval 详情

```bash
kallip approval get <APPROVAL_ID>
```

显示单个 approval 的完整详情。

```bash
$ kallip approval get "ap_a1b2c3d4..."
```

#### `approval approve`：批准已提交的动作

```bash
kallip approval approve <APPROVAL_ID>
```

批准一个已提交的 approval。agent 会收到通知并可执行该动作。

```bash
$ kallip approval approve "ap_a1b2c3d4..."
```

#### `approval deny`：驳回已提交的动作

```bash
kallip approval deny <APPROVAL_ID> [REASON]
```

驳回一个已提交的 approval，可附原因。

```bash
$ kallip approval deny "ap_a1b2c3d4..." "too risky"
```

### `file`：对接 files 服务的内容传输

在 files 服务（`kallipai-files`；HTTP 参考见 `docs/en/reference/files-api.md`）上传、下载、投递、列举记录。执行主体是 bearer 令牌所指的 tagma（即 spawn 环境里的那个）；`--space self` 是它自己的区域，`shared` 是该 space 的共享区域。

```bash
$ kallip file put <PATH> --file <FILE> [--json]   # PATH may be relative for a tagma: it lands in its own region (e.g. images/x.png)
$ kallip file get <ID> [--out <FILE>]
$ kallip file send <ID> (--to-tagma <TAGMA> | --to-user <USER>) [--json]
$ kallip file ls --space self|shared [--prefix <PREFIX>] [--limit <N>] [--json]
```

凭据走环境变量，不走标志：`KALLIPAI_POLIS_URL`（平台边缘 origin，CLI 从它推导 `<origin>/v1/files`；必填，未设置即报错（凭据绝不发往假定的部署））与 `KALLIPAI_FILES_TOKEN`（tagma 的长效 bearer）。在 CLI 运行处供给它们；agent shell 继承 spawn 时点 tagma 环境的实况，而启动清扫会最先移除 `KALLIPAI_FILES_TOKEN`，所以被供给的令牌绝不会到达 agent shell（服务端文件读取以 tagma 注册的登记凭据认证）。`--json` 把成功响应打成 JSON；`get` 缓冲内容（受服务最大 body 尺寸约束）写到 stdout（或 `--out`），内容从不被 JSON 包裹。

### `image`：把图片读入会话

把一张图片摄取进本 agent 的实时上下文（tagma 强制所属 set 的模态并记录该轮）。三种目标形态：

- 本地路径：字节落进 tagma 自己的内容寻址附件存储（以内容哈希为键），该轮记录之；命令打印 blob id 与 turn id。
- `--id`：files 记录 id，由服务该记录的那个平台面的凭据取回一次并存为本地正本；命令同路径形态打印 blob id 与 turn id。
- `--blob`：已存储的附件 blob，按内容地址重新摄取，零字节传输。

可解析且无路径分隔符的 UUID 读作记录 id；`--id` 与 `--path` 钉死解释方式，`--blob` 从不猜测。媒体类型来自 `--media-type` 或文件扩展名（默认 `image/png`；svg 作为非位图被拒，除非 `--media-type` 覆盖）。路径形态的图片必须放得进 tagma 的请求体上限，大图先降采样。

```bash
$ kallip image read <PATH-or-ID> [--id] [--path] [--blob] [--media-type <TYPE>] [--caption <TEXT>]
```

### `activity`：上报本 agent 的当前活动

```bash
kallip activity <ACTIVITY>   # 传空字符串即清除
```

一句短语描述本 agent 此刻在做什么（例如 “reading docs/x.md”）。仅限自身。

### `agent`：只读团队目录

```bash
kallip agent list
```

列出本 tagma 上的全部 agent：role、状态、since、id 与 workspace。仅用于发现；管理动作在 `subagent`。

### `budget`：管理 tagma 级 Token 预算

全部 agent 共享同一个预算：

```bash
kallip budget get                   # tagma 级预算状态
kallip budget increase <AMOUNT>     # 支持 K、M、G 后缀，如 100M
kallip budget decrease <AMOUNT>
kallip budget set <AMOUNT>          # 0 即暂停所有 agent
kallip budget unlimited             # 停止强制执行，消耗仍被记录
```

### `inbox`：管理本 agent 的消息收件箱

```bash
kallip inbox list [--status unread|read|done] [--limit <N>] [--relative-time]
kallip inbox read <MSG_ID>        # 同时把消息标记为已读
kallip inbox summary              # 总数与未读数
kallip inbox done <MSG_ID>
kallip inbox clear [--all]        # 默认只清已处理，--all 清全部
```

`--id` 可指定其他 agent 的收件箱，默认取 `KALLIPAI_ID`。列表按最新在前排列。

### `lesche`：经聊天中继投递消息

`send` 默认发到与用户的一对一会话，`--room` 发到已加入的群聊（room），`--tagma` 发到对端 tagma 的直连会话：

```bash
kallip lesche send [--room <ROOM>] [--tagma <TAGMA>] [--quote <SEQ>] [--mention <PARTICIPANT-ID>]...
kallip lesche rooms
kallip lesche read (--room <ROOM> | --tagma <TAGMA> | --session <SESSION> | --quote <ID>) [--after <N>] [--before <N>] [--limit <N>]
kallip lesche sessions
```

`rooms` 列出本 tagma 已加入的群聊；`read` 读更晚的一页（`--after`）、读更早的一页（`--before`），或用 `--quote` 只取某条回复指的那一行（`--before` 仅限群聊；`--quote` 可搭群聊、用会话 id 寻址的直连读（`--session`，入站消息头里的 `direct <session>` 就是它），或不带任何目标——裸 `--quote <ID>` 读的是一对一会话里的那一行）；`send --quote <SEQ>` 把消息指向那一行（群聊或直连）；`sessions` 一次列出全部可寻址面（用户一对一、已加入群聊、直连会话）。附件必须放在对端可读的 workspace 里。`send --mention <ID>`（可重复）点名该群聊的成员：线上只传 id（显示名由名册在渲染时解析），该旗标仅群聊可用，成员不在该群聊名册里则拒发而不是静默丢弃。成员资格由 tagma 判定——在 agent 发送路径上按该群聊名册判定；CLI 本身只拒绝把该旗标用在非群聊发送上，浏览器端只校验 id 的形状与数量上限，那条路径上的 id 由 lesche 原样转发。用户端把消息按 Markdown 渲染。引用了另一条消息的投递，会在 `[From: ...]` 头下面写出这条引用：谁写的，以及一行摘要（正文至多 256 个字形簇，截断时另加一个 `…`）；能取回整行的 `kallip lesche read` 指令只在还有东西可取时才跟着出现——摘要被截断、那一行根本取不回，或那一行没有正文。历史里已经取不到那一行时写明取不回，指令照样给出；消息本身总是照常送达。这条引用只给读得到它所指那一行的读者：今天在一对一上就是 root agent（那份历史是 root 自己的，别的读者执行那条指令只会拿到 403），在群聊与直连面上就是收消息的那个 agent；头部的 `| quote=<SEQ>` 段也只随引用一起出现。那一行没有正文时，引用行写成 `replying to <作者>: (no text on that row)`，取回整行的指令照样跟着——除了正文，那一行还存了什么，渲染引用的一方看不到。
这层判定有两处限制，都是取舍。其一，判定用的是轮询最新取回的那份名册，成员变动要等下一次轮询（约一分钟）才反映——这段时间里刚被移出群聊的人仍可能被点名放行。其二，rooms 列表答不上来这个群聊的名册时——拉取失败，或拉回来了但答案里没有这个群聊——本条发送的点名不经判定就照常发出：信封 POST 遇 503 会退避重试，rooms GET 没有这样的重试。

### `policy`：检视 agent 权限

```bash
kallip policy show <ID>
kallip policy exec-get <ID>
kallip policy exec-set <ID> <COMMAND> <allow|ask|deny> [--reason <REASON>]
```

`show` 打印 agent 的完整权限与生效的 classify preset。`exec-get` 与 `exec-set` 读写按命令的 `bash_exec` 覆盖项；`exec-set` 仅上级可用，`--reason` 在决定收紧为 ask 或 deny 时呈现给该 agent。

### `skill`：发现与检视 skills

```bash
kallip skill index <PATH> [--depth <N>]
kallip skill meta <PATH>
```

`index` 从各文件的 frontmatter 生成 skill 目录的索引（`--depth 1` 为单层视图，小目录可加深）；`meta` 显示单个 skill 的元数据，直接读自文件系统。

### `task`：tagma 上的任务台账

队列、状态机、确认周期、事件轨迹、硬门与已关闭任务的归档，由 tagma task API 提供：

| 子命令            | 用途                                                                                                                         |
| ---            | ---                                                                                                                        |
| `task create`  | 注册任务（`--title ...`）；`--require` 固定确认人名单（可重复），注册时解析为身份；`--assignee` 默认为认领人；另加关联键                                            |
| `task start`   | 按 id 认领排队任务；串行门生效，`--force` 可越过                                                                                            |
| `task confirm` | 为关单门交回确认，可附 `--note` 与从 `--file` 读取的报告正文                                                                                   |
| `task review`  | 将进行中任务转入评审                                                                                                                 |
| `task pause`   | 暂停进行中任务；无门生效                                                                                                               |
| `task resume`  | 恢复已暂停任务；串行门生效，`--force` 可越过                                                                                                |
| `task note`    | 向事件轨迹追加工作注记，不移动状态机                                                                                                         |
| `task close`   | 以 `--reason` 与可选 `--summary` 关闭任务；每个登记确认人都已确认，否则需 `--force`。带 `--dossier <目录>` 时该目录在关闭时规范化打包并随任务归档                         |
| `task reopen`  | 重新打开已关闭任务；串行门生效，`--force` 可越过                                                                                              |
| `task archive` | 归档已关闭任务，使其离开默认列表视图；closed-only 门生效，`--force` 可越过                                                                           |
| `task list`    | 列出任务（`--status`、`--assignee`、`--archived`、时间窗过滤、分页、`--relative-time`）                                                      |
| `task show`    | 显示单个任务的状态、关联键、事件轨迹，以及活跃窗口内的 token 用量                                                                             |
| `task export`  | 导出单个或全部任务；`--json` 输出供机器消费的 JSON                                                                                           |
| `task extract` | 解包已关闭任务的内容寻址 dossier 归档                                                                                                    |
| `task report`  | 回读事件轨迹中的确认：`report list` 列出已交回的确认（可选 `--confirmer` 过滤、`--out` 写文件），`report show` 显示单个确认（`--confirmer`、`--version`、`--out`） |

### `team`：声明式团队管理

```bash
kallip team status [--file <FILE>] [--json]
kallip team converge [--dry-run] [--drain] [--force] [--json]
kallip team lock-rebuild [--dir <DIR>]
```

`status` 显示三方比对（声明 vs 锁 vs 实时注册表），每个 role 一行，附 converge 的判定。`converge` 先规划、预检再执行收敛；结构性问题会整批拒绝，applied 与 aborted 结果都会写锁。`lock-rebuild` 以实况重建锁归档，列出每个被恢复的停泊 agent。

### 用法模式

#### 把工作委托给 subagent

```bash
# Spawn a subordinate, then send it work and poll its progress
CHILD=$(kallip subagent spawn --role researcher --workspace-root /projects/research --profile-set primary --permission-class normal <<'EOF'
explore the codebase
EOF
)
kallip message "$CHILD" <<'EOF'
Summarize the project structure
EOF
kallip status "$CHILD"
```

### 多 agent 编排

agent 用这个 CLI 管理自己的 subagent。单个 tagma 可同时承载多个项目的 agent。

#### 并行 subagent

```bash
# Spawn two subagents for different scopes
FRONTEND=$(kallip subagent spawn --role reviewer --workspace-root /projects/frontend --profile-set primary --permission-class guest < /dev/null)
BACKEND=$(kallip subagent spawn --role auditor --workspace-root /projects/backend --profile-set primary --permission-class normal < /dev/null)

# Send work to both
kallip message "$FRONTEND" <<'EOF' &
Review the latest changes for performance issues
EOF
kallip message "$BACKEND" <<'EOF' &
Audit dependencies for known vulnerabilities
EOF

# Wait for both sends to complete
wait
```

#### 检视与控制 subagent

```bash
# List your direct subagents
kallip subagent list

# Check a subagent's context usage before sending more work
kallip status $CHILD

# Interrupt a running subagent gracefully (without removing it)
kallip subagent interrupt $CHILD
```

### 环境变量

`KALLIPAI_AUTH_TOKEN`（必填）与 `KALLIPAI_TAGMA_URL`（默认 `http://127.0.0.1:3000`）是主变量。含 agent 调优参数的完整参考见 [配置](../configuration/index.md)。

### 客户端库

Rust 程序需要比 CLI 更多的控制时，`kallipai-client` crate 把 CLI 操作提供为异步方法，外加少数高级路径（事件流、subagent 拉起、root 查询）：

```rust
use kallipai_client::TagmaClient;

let client = TagmaClient::builder("http://127.0.0.1:3000")
    .auth_token(token)
    .build();

// The tagma owns a single root agent (eagerly created at startup); fetch it.
let root = client.get_root_agent().await?;
let id = root.id;

// Send a message (fire-and-forget)
client.post_message(&id, "Review src/main.rs").await?;

// Stream events (CLI exposes status/activity instead), check status.
let mut stream = client.event_stream(&id).await?;
let usage = client.agent_status(&id).await?;
// Note: the root cannot be removed (tagma-managed); `remove_agent` is for
// subagents only.
```

root agent 由 tagma 自管：启动时从环境变量一次性创建（`KALLIPAI_WORKSPACE_ROOT`、`KALLIPAI_MAX_TOOL_ROUNDS`、`KALLIPAI_ROOT_AGENT_PERMISSION_CLASS`；见 [Agent core 与 shell](../configuration/tagma/agent.md)），经 `get_root_agent()` 暴露。`spawn()` 只用于 **subagent**，它要求 `created_by`。
