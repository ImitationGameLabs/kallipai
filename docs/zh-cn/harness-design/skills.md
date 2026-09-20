---
title: Skill 管理
description: 经验如何蒸馏成上下文内容。
order: 25
---

skills 是 [Agentic 上下文管理](context-management.md)的一个应用：经验蒸馏成 markdown 文件，像其他上下文内容一样读取并固定。

## 共享 skill 目录

一个 tagma 有一个共享 skill 目录。root agent（tagma 的唯一顶层 agent，启动时创建）是这个目录的唯一作者；其他 agent 要新增或改动 skill，在会话里向 root 提议。

## Skill 文件格式

skill 是带 YAML frontmatter 的单个 markdown 文件：

```markdown
---
name: my-skill
description: When and how to use this skill
---

Skill content here: tips, patterns, pitfalls.
```

frontmatter 的 name 与 description 是给列表用的元数据：加载 skill 时 frontmatter 被剥除，只有正文进入上下文。

与标准 Agent Skill 的设计差异：标准做法以目录呈现单个 skill，my-skill/ 下是 SKILL.md（必须，同样是 frontmatter 元数据+正文）加 scripts/、references/、assets/ 等可选资源。这里只保留了 SKILL.md 这一个文件，单个 skill 的目录结构退化掉。这出自一个简化的判断：大部分 skill 不需要这样复杂的目录结构；为所有 skill 引入目录形态，会出现大量只装着一个 SKILL.md 的文件夹层级，skill 反而不容易被文件系统天然的树形结构管理。兼容的路径是明确的：未来只需扩展 index 子命令对标准目录格式的识别；skill 的加载本来就只是读文件，标准 SKILL.md 天然可用。

## Skill 发现机制

`kallip skill index` 扫描指定目录，取每个 skill 文件的 frontmatter，形成一份 skill 索引，是发现 skill 的入口。它是渐进式披露的关键：索引只给出每个 skill 的名字与一句话描述，不读文件全文，先据此判断哪个 skill 适合当前场景，合适再加载。索引每次运行即时生成，没有缓存也没有索引文件，列表永不与盘上文件漂移。默认渲染两层；`--depth 1` 给出扁平单层视图。

命令记录在 [kallip CLI 参考手册](../reference/kallip.md)。
