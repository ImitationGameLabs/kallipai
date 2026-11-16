// The shell's route list: one row per page, the single source both host
// apps dispatch through. Each host keeps a rest route (`[...path]`) and no
// per-page route files, so adding a page means three edits: the component,
// one row here, and one entry in `routePatterns.ts` (the URL list the hosts
// promise; that file says why it is deliberately a second place).
//
// A row says what the page is (pattern, component, where its props come
// from), never how a file is laid out. `row()` binds the component type, so
// a wrong or missing prop in a row is a type error at the row, not a
// runtime warning at navigation.
//
// The imports below are eager on purpose: both host builds ship one page
// bundle today, so the table can name every page. They are the one barrier
// to code-splitting the pages later, which would turn them into loaders.
import type { Component, ComponentProps } from "svelte";
import PanoramaPage from "../../pages/PanoramaPage.svelte";
import AccountHubPage from "../../pages/account/AccountHubPage.svelte";
import AccountTokensPage from "../../pages/account/AccountTokensPage.svelte";
import GatewayCatalogPage from "../../pages/manage/gateway/GatewayCatalogPage.svelte";
import OAuthCallbackPage from "../../pages/OAuthCallbackPage.svelte";
import OAuthSignupPage from "../../pages/OAuthSignupPage.svelte";
import ChannelChatPage from "../../pages/ChannelChatPage.svelte";
import ChatsHubPage from "../../pages/ChatsHubPage.svelte";
import FilesPage from "../../pages/FilesPage.svelte";
import GatewayMinePage from "../../pages/gateway/GatewayMinePage.svelte";
import LoginPage from "../../pages/LoginPage.svelte";
import PairPage from "../../pages/PairPage.svelte";
import RegisterPage from "../../pages/RegisterPage.svelte";
import RoomsPage from "../../pages/RoomsPage.svelte";
import RoomConversationPage from "../../pages/RoomConversationPage.svelte";
import RoomThreadPage from "../../pages/RoomThreadPage.svelte";
import RoomSettingsPage from "../../pages/RoomSettingsPage.svelte";
import SettingsPage from "../../pages/SettingsPage.svelte";
import TagmaProfilePage from "../../pages/TagmaProfilePage.svelte";
import TagmaChatPage from "../../pages/TagmaChatPage.svelte";
import OnlineManagePage from "../../pages/manage/OnlineManagePage.svelte";
import OnlineAgentDetailPage from "../../pages/manage/OnlineAgentDetailPage.svelte";
import OnlineAgentHistoryPage from "../../pages/manage/OnlineAgentHistoryPage.svelte";
import DirectSessionPage from "../../pages/DirectSessionPage.svelte";
import TagmataPage from "../../pages/TagmataPage.svelte";
import UserProfilePage from "../../pages/UserProfilePage.svelte";
import {
  adminGatewayPath,
  tagmaDetailsSectionPath,
  type TagmaDetailsSection,
} from "./routes.ts";
import { desktopQuery } from "./breakpoint.ts";
import { shellIdentity } from "./port.ts";
import { matchPath, type Params } from "./routeMatch.ts";
import type { ROUTE_PATTERNS } from "./routePatterns.ts";

/** What a row's props may read: the matched params and the query string. */
export interface RouteContext {
  params: Params;
  search: URLSearchParams;
}

/** A route that sends the visitor elsewhere instead of rendering a page. */
export interface RouteRedirect {
  status: 301 | 307;
  to: (ctx: RouteContext) => string;
}

/**
 * A redirect decided per load, allowed only on a row that renders a
 * component. The condition is what makes it conditional, so a row without
 * a component cannot express one: `redirectRow()` takes the plain
 * `RouteRedirect`, and every such row always redirects.
 */
export interface ConditionalRedirect extends RouteRedirect {
  when: (ctx: RouteContext) => boolean;
}

/** A row that renders a component. */
export interface PageRow<C extends Component<any, any, any>> {
  pattern: string;
  component: C;
  props?: (ctx: RouteContext) => ComponentProps<C>;
  /** Wrapped in <AdminArea> (the /admin subtree's gate). */
  area?: "admin";
  /**
   * A conditional redirect, decided per load: the panorama is desktop-only,
   * so a small screen goes to the chats hub before it mounts (307). Crossing
   * the breakpoint afterwards is PanoramaPage's own listener.
   */
  redirect?: ConditionalRedirect;
}

/** A row that only redirects. */
export interface RedirectRow {
  pattern: string;
  redirect: RouteRedirect;
}

export type RouteRow = PageRow<any> | RedirectRow;

/** Bind the component type of a page row. */
export function row<P extends string, C extends Component<any, any, any>>(
  r: PageRow<C> & { pattern: P },
): PageRow<C> & { pattern: P } {
  return r;
}

/** A row with no component. */
export function redirectRow<P extends string>(
  r: RedirectRow & { pattern: P },
): RedirectRow & { pattern: P } {
  return r;
}

/** The manage sections, in table order. The annotation keeps the entries
 * honest but cannot prove the list complete: `SectionsCoverUnion` below
 * checks that against the union. */
const TagmaDetailsSections: readonly TagmaDetailsSection[] = [
  "agents",
  "budget",
  "overview",
  "profiles",
  "schedules",
  "tasks",
];

/** A permanent redirect through a path builder (301, never a 302). */
function permanentRedirect(to: (ctx: RouteContext) => string): RouteRedirect {
  return { status: 301, to };
}
export const ROUTES = [
  row({
    pattern: "",
    component: PanoramaPage,
    redirect: {
      status: 307,
      to: () => "/chats",
      when: () => !matchMedia(desktopQuery).matches,
    },
  }),
  row({ pattern: "account", component: AccountHubPage }),
  row({ pattern: "account/tokens", component: AccountTokensPage }),
  redirectRow({
    pattern: "admin",
    redirect: permanentRedirect(() => adminGatewayPath()),
  }),
  row({
    pattern: "admin/gateway",
    component: GatewayCatalogPage,
    area: "admin",
  }),
  row({
    pattern: "admin/gateway/collections/[name]",
    component: GatewayCatalogPage,
    area: "admin",
    props: ({ params }) => ({ collection: params.name }),
  }),
  row({
    pattern: "auth/callback",
    component: OAuthCallbackPage,
    props: ({ search }) => ({
      code: search.get("code") ?? undefined,
      oauthState: search.get("state") ?? undefined,
    }),
  }),
  row({ pattern: "auth/signup", component: OAuthSignupPage }),
  row({
    pattern: "chat/[id]",
    component: ChannelChatPage,
    props: ({ params }) => ({ conversationId: params.id ?? "" }),
  }),
  row({ pattern: "chats", component: ChatsHubPage }),
  row({ pattern: "files", component: FilesPage }),
  row({ pattern: "gateway", component: GatewayMinePage }),
  // Honor the `?next=` set by the auth gate, so a deep link returns to the
  // page that asked for a login.
  row({
    pattern: "login",
    component: LoginPage,
    props: ({ search }) => ({
      returnPath: search.get("next") ?? undefined,
      offlineLogin: shellIdentity().offlineLogin,
    }),
  }),
  row({
    pattern: "pair",
    component: PairPage,
    props: ({ search }) => ({ code: search.get("code") ?? undefined }),
  }),
  row({ pattern: "register", component: RegisterPage }),
  row({ pattern: "rooms", component: RoomsPage }),
  row({
    pattern: "rooms/[id]",
    component: RoomConversationPage,
    // `at` is the row a deep link lands on (a discussion's own page sends
    // its room step here, and a quote jump names the row it answers).
    props: ({ params, search }) => ({
      roomId: params.id ?? "",
      at: search.get("at") ?? undefined,
    }),
  }),
  row({
    pattern: "rooms/[id]/threads/[root]",
    component: RoomThreadPage,
    // A discussion's own address: the root is in the path, and the reader
    // may be sent to one of its rows (`at`) or asked to answer one (`quote`).
    props: ({ params, search }) => ({
      roomId: params.id ?? "",
      root: params.root ?? "",
      at: search.get("at") ?? undefined,
      quote: search.get("quote") ?? undefined,
    }),
  }),
  row({
    pattern: "rooms/[id]/settings",
    component: RoomSettingsPage,
    props: ({ params }) => ({ roomId: params.id ?? "" }),
  }),
  row({ pattern: "settings", component: SettingsPage }),
  row({
    pattern: "tagma/[id]",
    component: TagmaProfilePage,
    props: ({ params }) => ({ tagmaId: params.id ?? "" }),
  }),
  row({
    pattern: "tagma/[id]/chat",
    component: TagmaChatPage,
    props: ({ params }) => ({ tagmaId: params.id ?? "" }),
  }),
  redirectRow({
    pattern: "tagma/[id]/details",
    redirect: permanentRedirect(({ params }) =>
      tagmaDetailsSectionPath(params.id ?? "", "overview"),
    ),
  }),
  // The six manage sections render one page with its section name; the
  // pattern and the section come from the same list entry, so a section
  // page cannot show a neighbor's content.
  ...TagmaDetailsSections.map((section) =>
    row({
      pattern: `tagma/[id]/details/${section}` as const,
      component: OnlineManagePage,
      props: ({ params }) => ({ tagmaId: params.id ?? "", page: section }),
    }),
  ),
  row({
    pattern: "tagma/[id]/details/agents/[agentId]",
    component: OnlineAgentDetailPage,
    props: ({ params }) => ({
      tagmaId: params.id ?? "",
      agentId: params.agentId ?? "",
    }),
  }),
  row({
    pattern: "tagma/[id]/details/agents/[agentId]/history",
    component: OnlineAgentHistoryPage,
    props: ({ params }) => ({
      tagmaId: params.id ?? "",
      agentId: params.agentId ?? "",
    }),
  }),
  row({
    pattern: "tagma/[id]/direct/[peer]",
    component: DirectSessionPage,
    props: ({ params }) => ({
      tagmaId: params.id ?? "",
      peerId: params.peer ?? "",
    }),
  }),
  row({ pattern: "tagmata", component: TagmataPage }),
  row({
    pattern: "user/[handle]",
    component: UserProfilePage,
    props: ({ params }) => ({ handle: params.handle ?? "" }),
  }),
] as const;

/** Every pattern the table declares, as a literal union. */
export type RoutePattern = (typeof ROUTES)[number]["pattern"];

// Every pattern the table declares must appear in `ROUTE_PATTERNS` and
// nothing else. A rename, or a removed route that leaves the list stale, is
// a type error; with no per-route file to compare against, this is the only
// gate a pattern change has.
type Equal<A, B> =
  (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
    ? true
    : false;
type Expect<T extends true> = T;

/** Compile-time proof that the table's patterns are the URL list. */
export type UrlListMatchesTable = Expect<
  Equal<RoutePattern, (typeof ROUTE_PATTERNS)[number]>
>;

/**
 * Compile-time proof that the section rows cover the whole union: a section
 * added to `TagmaDetailsSection` without a list entry leaves a member of
 * `Exclude` behind, and the `never` below stops compiling.
 */
export type SectionsCoverUnion = Expect<
  Equal<
    Exclude<TagmaDetailsSection, (typeof TagmaDetailsSections)[number]>,
    never
  >
>;

/** The row whose pattern matches this pathname, with its params. */
export function findRoute(
  pathname: string,
): { row: RouteRow; params: Params } | null {
  for (const candidate of ROUTES) {
    const params = matchPath(candidate.pattern, pathname);
    if (params !== null) return { row: candidate, params };
  }
  return null;
}

/** Why a pathname renders nothing, renders a page, or must be sent away. */
export type RouteResolution =
  | { kind: "page"; row: PageRow<any>; params: Params }
  | { kind: "redirect"; status: 301 | 307; location: string }
  | { kind: "missing" };

/**
 * Resolve a pathname to a page, a redirect, or a miss. Hosts run this in the
 * rest route's load, where redirects and 404s happen before anything mounts.
 *
 * `when` exists only on page rows, so the two row shapes are exhaustive: a
 * page row redirects when its condition holds and renders otherwise, and a
 * componentless row always redirects.
 */
export function resolveRoute(
  pathname: string,
  search: URLSearchParams = new URLSearchParams(),
): RouteResolution {
  const found = findRoute(pathname);
  if (found === null) return { kind: "missing" };
  const { row: matched, params } = found;
  const ctx: RouteContext = { params, search };

  if ("component" in matched) {
    const redirect = matched.redirect;
    if (redirect !== undefined && redirect.when(ctx)) {
      return {
        kind: "redirect",
        status: redirect.status,
        location: redirect.to(ctx),
      };
    }
    return { kind: "page", row: matched, params };
  }
  return {
    kind: "redirect",
    status: matched.redirect.status,
    location: matched.redirect.to(ctx),
  };
}
