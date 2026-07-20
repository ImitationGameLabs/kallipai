// @kallipai/kallip-ui
//
// Shared Svelte 5 layer for kallip-web and kallip-app. Two tiers:
//
//   - Presentational components + headless view-models (the original tier): every
//     component is prop-driven against @kallipai/kallip-common types, and the
//     headless factories (composer, auto-scroll, connection projection, error
//     classification) are pure.
//   - App layer (session stores, config facade, auth gate, route page bodies):
//     added when kallip-app converged on agora login. This tier depends on the
//     transport clients (@kallipai/kallip-client, @kallipai/kallip-agora-client)
//     as peerDeps, and stays free of `$app/*` -- navigation is injected via
//     initShell(), the agora URL via initAgora(), storage via initConfigStorage().
//
// SPLIT PENDING: the app tier should eventually live in its own package
// (kallip-app-core) so this package returns to being purely presentational.
// Until then, both tiers coexist here; the seam is the boundary between
// `components/`+pure `lib/` (presentational) and `lib/{session,config,shell}`+
// `pages/` (app). The injectable ports (initShell/initAgora/initConfigStorage)
// are designed so the split is a move, not a rewrite.
//
// Interactive Skeleton primitives are permitted only for structural chrome (e.g.
// AppShell's Navigation); feature components (Composer, TranscriptView,
// ApprovalsView) continue to consume Skeleton via CSS tokens only, so they stay
// portable to a non-Skeleton theme.

// App bootstrap ports (inject $app/navigation, the agora URL, and storage).
export {
  type Goto,
  type GotoOptions,
  initShell,
  navigate,
} from "./lib/shell/port.ts";
export { initAgora } from "./lib/session/agora.svelte.ts";
export {
  initConfigStorage,
  type OfflineModeConfig,
  type PersistedConfig,
} from "./lib/config/config.ts";
export { type AppMode, modeOf } from "./lib/config/mode.ts";
export {
  type ConfigStorage,
  localStorageConfigStorage,
} from "./lib/config/storage.ts";

// Reactive stores (singletons).
export { agoraSession } from "./lib/session/agora.svelte.ts";
export { sessionStore } from "./lib/session/session.svelte.ts";
export { configStore } from "./lib/config/config.svelte.ts";
export { connectDirect } from "./lib/session/connect.ts";

// Shell: shared root layout (auth gate + nav + banner), nav derivation, gate.
export { default as RootLayout } from "./lib/shell/RootLayout.svelte";
export { navFor, type NavIcons } from "./lib/shell/links.ts";
export {
  appGateDecision,
  type GateDecision,
  isPublicRoute,
} from "./lib/shell/gate.ts";

// Route page bodies (consumed by each app's thin +page.svelte wrappers).
export { default as ChatPage } from "./pages/ChatPage.svelte";
export { default as ApprovalsPage } from "./pages/ApprovalsPage.svelte";
export { default as TagmataPage } from "./pages/TagmataPage.svelte";
export { default as SettingsPage } from "./pages/SettingsPage.svelte";
export { default as LoginPage } from "./pages/LoginPage.svelte";
export { default as RegisterPage } from "./pages/RegisterPage.svelte";
export { default as ConnectPage } from "./pages/ConnectPage.svelte";

// App chrome
export { default as AppShell } from "./components/AppShell.svelte";
export { default as Banner } from "./components/Banner.svelte";
export { type BannerTone, bannerTones } from "./components/Banner.svelte";
export { default as Brand } from "./components/Brand.svelte";
export type { NavItem } from "./lib/shell.ts";

// Components
export { default as Markdown } from "./components/Markdown.svelte";
export { default as TranscriptView } from "./components/TranscriptView.svelte";
export { default as Composer } from "./components/Composer.svelte";
export { default as ApprovalsView } from "./components/ApprovalsView.svelte";
export { default as ApprovalRow } from "./components/ApprovalRow.svelte";

// Tagmata dashboard
export { default as TagmataDashboard } from "./components/tagmata/TagmataDashboard.svelte";
export { default as TagmaCard } from "./components/tagmata/TagmaCard.svelte";
export { default as EnrollmentCodeCard } from "./components/tagmata/EnrollmentCodeCard.svelte";
export type {
  EnrollmentCodeCardProps,
  SectionPhase,
  TagmaCardProps,
} from "./lib/tagmata.svelte.ts";
export {
  formatDateTime,
  isExpired,
  onlineDotClass,
} from "./lib/tagmata.svelte.ts";

// Headless view-models + helpers
export { createComposer } from "./lib/composer.svelte.ts";
export type { ComposerModel, ComposerOptions } from "./lib/composer.svelte.ts";
export { createAutoScroll } from "./lib/transcript.svelte.ts";
export type { AutoScroll, AutoScrollOptions } from "./lib/transcript.svelte.ts";
export { connectionViewModel } from "./lib/connection.svelte.ts";
export type {
  ConnectionSource,
  ConnectionState,
  ConnectionViewModel,
} from "./lib/connection.svelte.ts";
export { classifyError } from "./lib/errors.ts";
export type { ErrorView } from "./lib/errors.ts";
