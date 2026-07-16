// @kallipai/kallip-ui
//
// Presentational Svelte 5 components + headless view-models shared by kallip-web
// and kallip-app. No transport or session-store imports: every component is
// prop-driven against @kallipai/kallip-common types, and the headless factories
// (composer, auto-scroll, connection projection, error classification) are pure.
// Consumers provide the Skeleton/Tailwind theme and the session store.
//
// Interactive Skeleton primitives are permitted only for structural chrome (e.g.
// AppShell's Navigation); feature components (Composer, TranscriptView,
// ApprovalsView) continue to consume Skeleton via CSS tokens only, so they stay
// portable to a non-Skeleton theme.

// App chrome
export { default as AppShell } from "./components/AppShell.svelte";
export type { NavItem } from "./lib/shell.ts";

// Components
export { default as Markdown } from "./components/Markdown.svelte";
export { default as TranscriptView } from "./components/TranscriptView.svelte";
export { default as Composer } from "./components/Composer.svelte";
export { default as ApprovalsView } from "./components/ApprovalsView.svelte";
export { default as ApprovalRow } from "./components/ApprovalRow.svelte";

// Headless view-models + helpers
export { createComposer } from "./lib/composer.svelte.ts";
export type { ComposerModel, ComposerOptions } from "./lib/composer.svelte.ts";
export { createAutoScroll } from "./lib/transcript.svelte.ts";
export type { AutoScroll, AutoScrollOptions } from "./lib/transcript.svelte.ts";
export { connectionViewModel } from "./lib/connection.svelte.ts";
export type {
  ConnectionViewModel,
  ConnectionState,
  ConnectionSource,
} from "./lib/connection.svelte.ts";
export { classifyError } from "./lib/errors.ts";
export type { ErrorView } from "./lib/errors.ts";
