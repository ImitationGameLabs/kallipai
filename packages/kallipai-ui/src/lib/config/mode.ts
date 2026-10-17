// The two product modes. The active mode is a property of the shell, not of
// the persisted config: shellMode() (lib/shell/port.ts) derives it from the
// shell identity, and this module only names the two values.

export type AppMode = "online" | "offline";
