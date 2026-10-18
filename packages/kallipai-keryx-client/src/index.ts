// @kallipai/kallipai-keryx-client
//
// Browser client for the keryx data-plane relay (default :7210): the
// bilateral (1:1) E2EE conversation transport. `KeryxClient` covers
// conversation setup, the synchronous key exchange, envelope posting, and the
// bilateral `me/events` SSE stream; `openRelayChannel`/`RelayChannel` wire
// those into a pure E2EE pipe to a tagma. The pinned device key is TOFU from
// the archeion (control plane) and is passed in as a base64 string by the
// caller, so this package has no source dependency on the archeion client.
// Browser-first (session cookie shared cross-subdomain with the archeion);
// the fetch discipline and SSE frame machine live in kallipai-common, shared
// with the lesche client. The rooms family (rooms, invites, the lesche
// `me/events` stream) lives in @kallipai/kallipai-lesche-client.

export const PACKAGE_NAME = "@kallipai/kallipai-keryx-client";

export { KeryxClient } from "./http.ts";
export {
  ManageChannelError,
  type ManageChannelErrorReason,
  openRelayChannel,
  RelayChannel,
} from "./channel.ts";
export { ManageRestClient } from "./manageRest.ts";
export { LinearBackoff, ProjectionClient } from "./projection.ts";
export {
  type CachedLine,
  clear as clearConvCache,
  clearPendings,
  clearReadWatermarks,
  deletePending,
  getReadWatermark,
  isFailedPending,
  markPendingFailed,
  type PendingLine,
  put,
  putPending,
  putReadWatermark,
  readPendingByTagma,
  readTail,
  readTailBefore,
  setPendingRowsForTests,
} from "./cache.ts";

// Data-plane wire types the UI consumes. The remaining wire types
// (TagmaRequest, TagmaControl, KeyExchange*, CreateConversationResponse) are
// package-private (used inside the channel/crypto implementation).
export type {
  AuthoredEvent,
  Envelope,
  FileAttachment,
  HistoryEntry,
  Participant,
  SignalEvent,
  TagmaReply,
  TagmaStatusPayload,
} from "./types.ts";
export type { KeryxEvent } from "./types.ts";
export type { KeryxEventFrame } from "./http.ts";
export type {
  ProjectionAgentsResponse,
  ProjectionBudgetResponse,
  ProjectionDirty,
  ProjectionStatusResponse,
  ProjectionWorkScheduleResponse,
} from "./types.ts";
// Shared with the lesche client (both data-plane services emit the same
// error envelope); see kallipai-common's errors.ts.
export { DataPlaneApiError } from "@kallipai/kallipai-common";
