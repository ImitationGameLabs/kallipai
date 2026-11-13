// The URL set both hosts promise. Every pattern the shell table declares
// appears here and nothing else: routeTable.ts pins the two lists equal at
// compile time, and routeBuilderBindings_test.ts matches every path builder
// in routes.ts against a row of this list.
//
// This list is the second place a URL is written, on purpose. A new page or a
// rename has to touch both the table and this file; the equality check turns
// the mismatch into a compile error, so the second edit is not forgotten
// silently. It also gives the tests a route list to match against: they
// cannot import the table itself (it pulls in .svelte components).

/** Every pattern the table declares, "" for the root. */
export const ROUTE_PATTERNS = [
  "",
  "account",
  "account/tokens",
  "admin",
  "admin/gateway",
  "admin/gateway/collections/[name]",
  "auth/callback",
  "auth/signup",
  "chat/[id]",
  "chats",
  "files",
  "gateway",
  "login",
  "pair",
  "register",
  "rooms",
  "rooms/[id]",
  "rooms/[id]/settings",
  "settings",
  "tagma/[id]",
  "tagma/[id]/chat",
  "tagma/[id]/details",
  "tagma/[id]/details/agents",
  "tagma/[id]/details/agents/[agentId]",
  "tagma/[id]/details/agents/[agentId]/history",
  "tagma/[id]/details/budget",
  "tagma/[id]/details/overview",
  "tagma/[id]/details/profiles",
  "tagma/[id]/details/schedules",
  "tagma/[id]/details/tasks",
  "tagma/[id]/direct/[peer]",
  "tagmata",
  "user/[handle]",
] as const;
