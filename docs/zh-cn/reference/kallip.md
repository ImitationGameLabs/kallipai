---
title: kallip CLI 参考手册
description: 无头 kallip 客户端的 CLI 参考。
order: 70
---

这是 agent 用来与其他 agent 协作、管理自己的 subagent 与运行时事务的 CLI。

所有子命令都使用 `KALLIP_AUTH_TOKEN`（必填）与 `KALLIP_TAGMA_URL`（环境变量，默认 `http://127.0.0.1:3000`）。

## 子命令

### `message`：向 agent 发送消息

```bash
kallip message <ID>
```

把消息送进 agent 的输入队列。消息文本从完整 stdin 读取（支持多行：管道、heredoc、`< file` 都可以）；没有文本参数，shell 展开因此无法破坏消息。tagma 立即受理（202 Accepted）并异步处理。轮询 `status` 观察结果。成功时 CLI 打印一行受理文本的 JSON 回显（失败时什么都不打印，失败的发送绝不伪装成已送达）：

```json
{"kallip.message.sent":{"to":"<id>","text":"<message>","queue_depth":0}}
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

### `subagent`：管理直接 subagent

```bash
kallip subagent <subcommand> [args]
```

管理**当前 agent 的直接 subagent**。执行操作的 supervisor 取自 `KALLIP_ID` 环境变量，这些命令只在 agent 上下文里有意义，未设置时报错。`subagent` 是唯一的管理入口；spawn、list、remove、interrupt、改标签全部经此。

| 子命令                    | 用途                                             |
| ------------------------- | ------------------------------------------------ |
| `subagent spawn`          | 创建直接 subagent（必填标志见下文说明）。        |
| `subagent list`           | 列出当前 agent 的直接 subagent。                 |
| `subagent remove <ID>`    | 移除直接 subagent。                              |
| `subagent interrupt <ID>` | 中断直接 subagent 当前操作。                     |
| `subagent metadata <ID>`  | 更新直接 subagent 的 role/description。          |

作用域说明（服务端强制）：

- `subagent metadata` 限于**直接 supervisor**（`require_direct_supervisor`）；祖父级不能给孙辈改标签。
- `subagent remove` / `subagent interrupt` 授权**任意祖先**（`require_superior`），所以这里“直接 subagent”的表述只是 CLI 便利，不是服务端限制。
- `subagent spawn` 要求 `--workspace-root`、`--profile-set` 与 `--permission-class`；未知的 profile set 名称在 spawn 时被拒。`--role` 对每次 subagent 创建都要求非空。
- `subagent spawn --permission-class {normal,guest}` 显式**降级** subagent 的 FS 访问等级，使其低于父级自身等级（如 `normal` 父级创建只读 `guest` 评审者）。高于父级等级的取值被 tagma 以 `403` 拒绝。该标志必填。被授予的等级可由 `kallip`/`GET /agents/{id}/permissions` 查看。

```bash
$ kallip subagent list
researcher  idle  ws=/projects/frontend
$ kallip subagent spawn --role reviewer --workspace-root /projects/reviews --profile-set primary --permission-class guest --description "reviews PRs" < /dev/null
b4c2d3e5-...
```

spawn 从 stdin 读取可选的初始 prompt：上面的 `< /dev/null` 表示“无 prompt”，id 被捕获时也避免 spawn 吞掉外层脚本的 stdin。要传 prompt 就用管道或 heredoc。

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

在 files 服务（`kallip-files`；HTTP 参考见 `docs/en/reference/files-api.md`）上传、下载、投递、列举记录。执行主体是 bearer 令牌所指的 tagma（即 spawn 环境里的那个）；`--space self` 是它自己的区域，`shared` 是该 space 的共享区域。

```bash
$ kallip file put <PATH> --file <FILE> [--json]   # PATH may be relative for a tagma: it lands in its own region (e.g. images/x.png)
$ kallip file get <ID> [--out <FILE>]
$ kallip file send <ID> (--to-tagma <TAGMA> | --to-user <USER>) [--json]
$ kallip file ls --space self|shared [--prefix <PREFIX>] [--limit <N>] [--json]
```

凭据走环境变量，不走标志：`KALLIP_POLIS_URL`（平台边缘 origin，CLI 从它推导 `<origin>/v1/files`；必填，未设置即报错（凭据绝不发往假定的部署））与 `KALLIP_FILES_TOKEN`（tagma 的长效 bearer）。在 CLI 运行处供给它们；agent shell 继承 spawn 时点 tagma 环境的实况，而启动清扫会最先移除 `KALLIP_FILES_TOKEN`，所以被供给的令牌绝不会到达 agent shell（服务端文件读取以 tagma 注册的登记凭据认证）。`--json` 把成功响应打成 JSON；`get` 缓冲内容（受服务最大 body 尺寸约束）写到 stdout（或 `--out`），内容从不被 JSON 包裹。

### `image`：把图片读入会话

把一张图片摄取进本 agent 的实时上下文（tagma 强制所属 set 的模态并记录该轮）。三种目标形态：

- 本地路径：字节落进 tagma 自己的内容寻址附件存储（以内容哈希为键），该轮记录之；命令打印 blob id 与 turn id。
- `--id`：files 记录 id，由 tagma 以自己注册的凭据取回。
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

`--id` 可指定其他 agent 的收件箱，默认取 `KALLIP_ID`。列表按最新在前排列。

### `lesche`：经聊天中继投递消息

`send` 默认发到与用户的一对一会话，`--room` 发到已加入的群聊（room），`--tagma` 发到对端 tagma 的直连会话：

```bash
kallip lesche send [--room <ROOM>] [--tagma <TAGMA>]
kallip lesche rooms
kallip lesche read (--room <ROOM> | --tagma <TAGMA>) [--after-seq <N>] [--limit <N>]
kallip lesche sessions
```

`rooms` 列出本 tagma 已加入的群聊；`read` 从某个序列位置回放会话历史；`sessions` 一次列出全部可寻址面（用户一对一、已加入群聊、直连会话）。附件必须放在对端可读的 workspace 里。

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

队列、状态机、事件轨迹、硬门与已关闭任务的归档，由 tagma task API 提供：

| 子命令 | 用途 |
| --- | --- |
| `task start` | 注册任务（`--title ...`）或按 id 认领排队任务；串行门生效，`--force` 可越过 |
| `task checkpoint` | 记录工作注记、评审回执（`--receipt`）、转入评审（`--review`）和/或等待标记 |
| `task close` | 关闭任务；每个指派的席位都已交回执，否则需 `--force` |
| `task reopen` | 重新打开已关闭的任务 |
| `task annotate` | 向事件轨迹追加注记，不移动状态机 |
| `task dispatch` | 为当前评审轮注册席位名单 |
| `task gate-report` | 记录先于每次历史操作的公告 |
| `task chain-op` | 记录历史操作（commit/amend/rebase/reset）；要求存在更新的门报 |
| `task archive` | 归档已关闭任务，使其离开默认列表视图 |
| `task list` | 列出任务（`--status`、`--assignee`、`--archived`） |
| `task show` | 显示单个任务的状态、关联键与事件轨迹 |
| `task export` | 导出单个或全部任务；`--json` 输出供机器消费的 JSON |
| `task extract` | 解包已关闭任务的内容寻址 dossier 归档 |

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

`KALLIP_AUTH_TOKEN`（必填）与 `KALLIP_TAGMA_URL`（默认 `http://127.0.0.1:3000`）是主变量。含 LLM provider 配置与 agent 调优参数的完整参考见 [配置](../configuration/index.md)。

### 客户端库

Rust 程序需要比 CLI 更多的控制时，`kallip-client` crate 把 CLI 操作提供为异步方法，外加少数高级路径（事件流、subagent 拉起、root 查询）：

```rust
use kallip_client::TagmaClient;

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

root agent 由 tagma 自管：启动时从环境变量一次性创建（`KALLIP_WORKSPACE_ROOT`、`KALLIP_MAX_TOOL_ROUNDS`、`KALLIP_ROOT_AGENT_PERMISSION_CLASS`；见 [Agent core 与 shell](../configuration/tagma/agent.md)），经 `get_root_agent()` 暴露。`spawn()` 只用于 **subagent**，它要求 `created_by`。
