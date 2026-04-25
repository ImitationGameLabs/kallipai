//! Command classification constants and list-based check functions.

use rable::Node;

use super::super::ToolDecision;
use super::util;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub(super) const SHELL_INTERPRETERS: &[&str] =
    &["bash", "sh", "dash", "zsh", "ksh", "csh", "tcsh", "fish", "ash", "busybox"];

pub(super) const EVAL_COMMANDS: &[&str] = &["eval", "exec", "source", "."];

pub(super) const COMMAND_STRING_FLAGS: &[&str] = &["-c"];

pub(super) const DANGEROUS_COMMANDS: &[&str] = &[
    // Privilege escalation
    "sudo",
    "su",
    "doas",
    "pkexec",
    "gksudo",
    "gksu",
    "kdesudo",
    // Destructive filesystem
    "mkfs",
    "mkswap",
    "dd",
    // System control
    "shutdown",
    "reboot",
    "poweroff",
    "halt",
    "init",
    "systemctl",
    // Filesystem ownership / permissions / mounts
    "chown",
    "chgrp",
    "chroot",
    "mount",
    "umount",
    // Network remote access
    "ssh",
    "scp",
    "sftp",
    "telnet",
];

/// Environment variables that can alter security-critical behavior.
pub(super) const DANGEROUS_ENV_VARS: &[&str] =
    &["PATH", "LD_LIBRARY_PATH", "LD_PRELOAD", "PYTHONPATH", "HOME", "SHELL", "IFS"];

/// Truly read-only commands that cannot modify filesystem state or execute code.
const ALLOW_READONLY: &[&str] = &[
    // Filesystem info
    "pwd", "ls", "find", "stat", "file", "tree", "du", "df", // Search
    "rg", "grep", "ag", "ack", "fgrep", "egrep", // File viewing
    "cat", "head", "tail", "less", "more", "bat", // Text processing (read-only pipes)
    "sort", "uniq", "wc", "cut", "tr", "comm", "diff", // Output
    "echo", "printf", // Environment info
    "env", "printenv", "which", "type", // System info
    "date", "uname", "hostname", "whoami", "id", // Process info
    "ps", "top", "htop", // Data processing (read-only)
    "jq", "yq", // Shell builtins (side-effect-free)
    "test", "true", "false",
    // Test runners (consistent with subcommand-gated cargo test / go test)
    "pytest",
];

// ---------------------------------------------------------------------------
// Check functions
// ---------------------------------------------------------------------------

pub(super) fn check_dangerous_invocation(cmd_name: &str, words: &[Node]) -> Option<ToolDecision> {
    match cmd_name {
        "rm" if util::has_any_flag(words, &["-r", "-rf", "-fr", "-R", "-rRf"]) => {
            Some(ToolDecision::Ask {
                reason: "rm with recursive/force flags".into(),
                dangerous: true,
            })
        }
        "chmod" if util::has_any_flag(words, &["777"]) => {
            Some(ToolDecision::Ask { reason: "chmod 777".into(), dangerous: true })
        }
        "git"
            if util::has_subcommand_and_flag(words, "reset", "--hard")
                || util::has_subcommand_and_flag(words, "reset", "--keep")
                || util::has_subcommand_and_flag(words, "clean", "-fd") =>
        {
            Some(ToolDecision::Ask { reason: "destructive git operation".into(), dangerous: true })
        }
        _ => None,
    }
}

pub(super) fn check_allow_list(cmd_name: &str, _words: &[Node]) -> ToolDecision {
    if ALLOW_READONLY.contains(&cmd_name) {
        return ToolDecision::Allow;
    }

    ToolDecision::Ask {
        reason: "command is not on the low-risk allowlist".into(),
        dangerous: false,
    }
}
