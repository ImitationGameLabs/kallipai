//! `history` command subtree of the `kallip` CLI (clap derive).

use clap::{Args, Subcommand, ValueEnum};
use std::path::PathBuf;

const LIST_EXAMPLES: &str = "\
Examples:
  kallip history list my-agent
  kallip history list my-agent --day 2026-10-24 --limit 20 --tools
  kallip history list my-agent --fields turn_id,at,usage
  kallip history list my-agent --json
  kallip history list my-agent --before \"<next_cursor>\"   # pass it back verbatim";

const SHOW_EXAMPLES: &str = "\
Examples:
  kallip history show my-agent 142
  kallip history show my-agent 142 --call 1
  kallip history show my-agent 142 --call 1 --body
  kallip history show my-agent 142 --call 1 --body --offset 16384 --json
  kallip history show my-agent 142 --call 1 --body --part args
  kallip history show my-agent 142 --message 0 --body --part reasoning";

const EXPORT_EXAMPLES: &str = "\
Examples:
  kallip history export my-agent --out history.json
  kallip history export my-agent --day 2026-10-24 --pages 5
  kallip history export my-agent --limit 200 --pages 1";

/// Read an agent's turn history through the tagma's read routes: the paged
/// log, one turn, and one call's text. The tagma computes every answer from
/// the log on demand; the CLI never reads those files itself.
#[derive(Subcommand)]
pub(crate) enum HistoryCommand {
    /// List an agent's turns, newest first, one page per request. The table
    /// is compact by default (`--fields` picks the columns); `--tools`
    /// expands each turn's calls, outcome and all, under its row, and
    /// `--json` prints the page exactly as the tagma returned it.
    #[command(after_help = LIST_EXAMPLES)]
    List(TurnsListArgs),
    /// Show one turn: its messages and call rows. `--call N` narrows to one
    /// call and `--message N` to one message; either with `--body` fetches a
    /// range of that text instead, `--part` picking which text.
    #[command(after_help = SHOW_EXAMPLES)]
    Show(TurnsShowArgs),
    /// Walk pages and emit one JSON document: a manifest (which tagma, which
    /// agent, the window covered, the view version) plus the pages exactly as
    /// the tagma returned them.
    #[command(after_help = EXPORT_EXAMPLES)]
    Export(TurnsExportArgs),
}

#[derive(Args)]
pub(crate) struct TurnsListArgs {
    /// Agent ID or role.
    pub agent: String,
    /// The UTC day file to read, `YYYY-MM-DD`. Absent pages backwards from
    /// the newest day.
    #[arg(long)]
    pub day: Option<String>,
    /// Opaque cursor taken from a previous page's `next_cursor`. Passed back
    /// byte for byte: only the tagma reads its shape.
    #[arg(long)]
    pub before: Option<String>,
    /// Rows wanted, clamped by the tagma into 1..=200.
    #[arg(long)]
    pub limit: Option<u32>,
    /// Columns to print, named exactly as the view's own fields
    /// (`turn_id,at,profile,usage,elapsed_ms,calls` by default).
    #[arg(long, value_delimiter = ',')]
    pub fields: Vec<String>,
    /// Expand each turn's calls under its row: the name, how it ended, and
    /// the exit status when it has one.
    #[arg(long)]
    pub tools: bool,
    /// Print the page as the tagma returned it, field names and all.
    #[arg(long)]
    pub json: bool,
}

/// Which text a body read walks: a call's result, arguments or error, or a
/// message's own text or its reasoning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum PartArg {
    /// A call's result record, the default for --call.
    Result,
    /// The arguments the model sent.
    Args,
    /// The call record's error text.
    Error,
    /// A message's own text, the default for --message.
    Text,
    /// The reasoning stored beside a message.
    Reasoning,
}

impl PartArg {
    /// The name the tagma's query takes and the notices print.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Result => "result",
            Self::Args => "args",
            Self::Error => "error",
            Self::Text => "text",
            Self::Reasoning => "reasoning",
        }
    }
}

#[derive(Args)]
pub(crate) struct TurnsShowArgs {
    /// Agent ID or role.
    pub agent: String,
    /// Turn id.
    pub turn: u64,
    /// Narrow to one call of the turn, by its index within the round.
    #[arg(long)]
    pub call: Option<u32>,
    /// Narrow to one message of the turn, by its position in the round.
    #[arg(long)]
    pub message: Option<u32>,
    /// With --call or --message: fetch a range of that text instead of the
    /// row. The text goes to stdout so it can be piped; every notice goes to
    /// stderr.
    #[arg(long)]
    pub body: bool,
    /// With --body: byte offset into the text.
    #[arg(long)]
    pub offset: Option<u64>,
    /// With --body: bytes wanted, clamped by the tagma into 1..=65536.
    #[arg(long)]
    pub limit: Option<usize>,
    /// With --body: which text to read. A call holds result, args and error;
    /// a message holds text and reasoning.
    #[arg(long, value_enum)]
    pub part: Option<PartArg>,
    /// Print the tagma's JSON for the turn, that call or message, or the
    /// text range.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub(crate) struct TurnsExportArgs {
    /// Agent ID or role.
    pub agent: String,
    /// The UTC day file to read, `YYYY-MM-DD`.
    #[arg(long)]
    pub day: Option<String>,
    /// Rows per page request, clamped by the tagma into 1..=200.
    #[arg(long)]
    pub limit: Option<u32>,
    /// Pages to walk at most. The manifest says whether the walk reached the
    /// end of the log or stopped at this bound.
    #[arg(long, default_value_t = 20)]
    pub pages: u32,
    /// Write the document here instead of stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
}
