// Opaque identifier types. On the wire these are bare JSON strings: UUID v4 for
// AgentId and the agora ids. Modelled as plain string aliases; the wire adapters
// in @kallipai/kallip-client and @kallipai/kallip-agora-client produce them.

export type AgentId = string;
export type TeamId = string;
export type ConversationId = string;
export type UserId = string;
export type TraceId = string;
export type ApprovalId = string;
export type SkillName = string;
