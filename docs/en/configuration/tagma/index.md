---
title: Tagma
description: Configuration surfaces for the tagma server, its agents, and the LLM backend.
order: 10
---

The tagma's configuration lives in environment variables and model
configuration files, split across pages and indexed in a one-line quick
reference:

- [Environment variables](environment-variables.md): every variable at a
  glance, one line each, linked to its section.
- [Model configuration](llm.md): the provider, profile, and profile set
  layers, and why each layer exists on its own.
- [Model configuration methods](methods.md): configure models through the
  web interface, the `kallip profile-set` command, or manual
  `profiles.toml` editing.
- [Agent core and shell](agent.md): agent runtime tuning and the
  variables injected into agent shell sessions.
- [Tagma service](service.md): listen address, data and skill roots,
  and logging.
