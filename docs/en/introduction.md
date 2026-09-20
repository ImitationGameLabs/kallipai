---
title: Introduction
description: What KallipAI is, where its name comes from, and how its core pieces fit together.
order: 5
---

KallipAI is an agent harness designed for multi-agent
collaboration, long-running tasks, and scale.

## Where the Name Comes From

The name traces to **kallipolis** (Greek _kalon_, "beauty", plus
_polis_, "city"), Plato's ideal city in the _Republic_: a city where
every role does its precise work and the whole functions as a single
well-ordered organism. That is the image behind multi-agent work
here: a structured collaboration where every agent has a role.

For everyday use the stem shortens to **kallip** (kallipolis without
the `-olis` that does no technical work) and gains **AI** as a suffix,
giving **kallipai**.

## The Core Concept: the Tagma

A tagma (τάγμα, Greek for an organized body of troops) is an
agent team at runtime, hosted by one long-lived server process.
Everything the team needs to work together comes built in.

You can run as many tagmas as you need, one per team. Each keeps its
own agents, workspaces, and data directory, and the teams stay
independent. Agents keep running when a client disconnects, so long
work does not end with any single session.

## Where to Go Next

Every team runs in its own tagma. Polis (the hosted
platform) connects a tagma to remote clients.

Start with [deployment](deployment/index.md) to bring the platform up,
or read [configuration](configuration/index.md) and the
[harness design](harness-design/index.md) notes; daily operations live
in the [kallip CLI reference](reference/kallip.md).
