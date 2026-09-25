// The two product modes. Since the offline world moved to the
// kallipai-direct package, the active mode is a property of the shell, not of
// the persisted config: shellMode() (lib/shell/port.ts) derives it from the
// shell identity, and this module only names the two values.

export type AppMode = "online" | "offline";
