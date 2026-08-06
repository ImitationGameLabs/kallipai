// @kallipai/kallip-lesche-client
//
// Browser client for the lesche data-plane relay (default :7200): the E2EE
// conversation transport. `LescheClient` covers conversation setup, the
// synchronous key exchange, envelope posting, and the multiplexed `me/events`
// SSE stream; `openRelayChannel`/`RelayChannel` wire those into a pure E2EE pipe
// to a tagma. The pinned device key is TOFU from the agora (control plane) and
// is passed in as a base64 string by the caller, so this package has no source
// dependency on the agora client. Browser-first (session cookie shared
// cross-subdomain with the agora; `me/events` is parsed with the shared
// `parseSseStream`).

export const PACKAGE_NAME = "@kallipai/kallip-lesche-client";

export { LescheClient } from "./http.ts";
export { openRelayChannel, RelayChannel } from "./channel.ts";
export { clear as clearConvCache, loadAll, put } from "./cache.ts";

// Data-plane wire types the UI consumes. The remaining wire types
// (Participant, TagmaRequest, TagmaControl, KeyExchange*, etc.) are
// package-private (used inside the channel/crypto implementation).
export type {
  AuthoredEvent,
  Envelope,
  HistoryEntry,
  Participant,
  RoomMessageView,
  SignalEvent,
  TagmaReply,
} from "./types.ts";
export type { LescheEvent } from "./types.ts";
// Room management (relocated from the agora client).
export type {
  AddTagmaRequest,
  CreateInviteRequest,
  CreateInviteResponse,
  ParticipantKind,
  RoomInviteView,
  RoomMember,
  RoomMemberProfile,
  RoomRosterView,
  RoomView,
  TagmaRoomView,
  Visibility,
} from "./types.ts";
export { LescheApiError } from "./types.ts";
