// @kallipai/kallipai-lesche-client
//
// Browser client for the lesche data-plane relay (default :7200): the rooms
// family -- room envelopes, room history, room management, and the lesche
// `me/events` SSE. The bilateral E2EE conversation transport (conversation
// setup, the key exchange, envelope posting, the keryx `me/events` stream)
// lives in @kallipai/kallipai-keryx-client. Browser-first (session cookie
// shared cross-subdomain with the archeion; the stream is parsed with the
// shared frame machine in kallipai-common).

export const PACKAGE_NAME = "@kallipai/kallipai-lesche-client";

export { LescheClient } from "./client.ts";

// Data-plane wire types the UI consumes. The remaining wire types are
// package-private.
export type {
  AuthoredEvent,
  Envelope,
  FileAttachment,
  HistoryEntry,
  Participant,
  RoomMessageView,
  RoomUnreadView,
  SignalEvent,
  TagmaReply,
  TagmaStatusPayload,
} from "./types.ts";
export type { LescheEvent } from "./types.ts";
export type { MeEventFrame } from "./client.ts";
// Room management (relocated from the archeion client).
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
  ThreadChoice,
  ThreadDelivery,
  ThreadSubscriptionRequest,
  ThreadSubscriptionView,
  Visibility,
} from "./types.ts";
export { DataPlaneApiError } from "./types.ts";
