---
title: Configuration
description: How the platform is configured, from environment variables to the model profiles file and the dev stack.
order: 10
---

Runtime configuration is environment-first: every component reads its settings from
`KALLIP_*` variables at startup. The LLM backend can also be tuned through a
profiles config file (see [Model configuration methods](tagma/methods.md)), and
NixOS deployments configure services through [module options](../deployment/nixos/index.md).

Copy `.env.example` to
`.env` and fill in the required values. If you use `direnv`, it loads `.env`
automatically via `.envrc`.

The tagma configuration reference lives at [Tagma](tagma/index.md); environment
variables for the other components (cron, the files service, the local daemon)
are in the [environment variables reference](../reference/environment-variables/index.md).

## Dev Stack Shape

Three variables drive the dev compose (`compose/dev/polis.nix`) and the web dev
server together (both flow from the root `.env` via direnv):

| Variable        | Default                                     | Purpose                                                                                                                         |
| --------------- | ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIP_EDGE_TLS` | `on` | Edge shape: `on` = Caddy-fronted https+domain topology with the mkcert cert; `off` = plain http (no cert/DNS trust setup; see docs/en/development/setup.md). |
| `KALLIP_EDGE_PORT` | `443` | Dev edge listener port (compose caddy + the web dev server when non-default); the browser-facing web origin carries it and CORS/oauth derive from it verbatim. |
| `KALLIP_DOMAIN` | `kallipai.com` | The domain the dev server and compose topology derive from (both edge shapes; the plain-http quick start sets `localhost` explicitly); the web app's URLs derive in the browser at runtime. |
