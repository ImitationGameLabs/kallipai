//! The `kallip turns` family: rendering over the turn-history read routes.
//!
//! Every fact printed here comes from the tagma's read face. The log stays the
//! single source of truth, and this module never opens a history file: a
//! reader that parses the store turns the store's format into a contract.
//!
//! Two promises the routes are built around hold here as well. A cursor is a
//! server-side ordering key, so it travels back byte for byte, never parsed or
//! normalised on the way. And `--json` prints the tagma's own document instead
//! of reshaped rows, so a consumer reads one shape, not two.

use std::io::Write;

use anyhow::{Context, Result, anyhow, bail};
use kallipai_client::TagmaClient;
use kallipai_common::protocol::{
    CallBody, CallBodyQuery, CallRow, MessageRow, SPILL_RECLAIMED, TURN_SCHEMA_VERSION, TurnDetail,
    TurnProfile, TurnRow, TurnUsageView, TurnsPage, TurnsQuery,
};
use serde::Serialize;
use serde_json::Value;

use crate::args::turns::{TurnsCommand, TurnsExportArgs, TurnsListArgs, TurnsShowArgs};

/// The columns a bare `turns list` prints: the triage facts of a round.
const DEFAULT_FIELDS: &[&str] = &["turn_id", "at", "profile", "usage", "elapsed_ms", "calls"];

/// Run one `turns` command against the tagma's read routes.
pub async fn run_turns(client: &TagmaClient, cmd: &TurnsCommand) -> Result<()> {
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    run(client, cmd, &mut out, &mut err).await
}

/// The command's body, with its two sinks handed in: printed text goes to
/// `out`, every notice goes to `err`, so a piped read carries the payload
/// alone.
async fn run(
    client: &TagmaClient,
    cmd: &TurnsCommand,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    match cmd {
        TurnsCommand::List(args) => list(client, args, out).await,
        TurnsCommand::Show(args) => show(client, args, out, err).await,
        TurnsCommand::Export(args) => export(client, args, out, err).await,
    }
}

async fn list(client: &TagmaClient, args: &TurnsListArgs, out: &mut dyn Write) -> Result<()> {
    if args.json && (!args.fields.is_empty() || args.tools) {
        bail!(
            "--json prints the page as the tagma returned it; --fields and --tools shape the table, so they cannot be combined with it"
        );
    }
    refuse_blank(
        "--day",
        args.day.as_deref(),
        "the UTC day file to read, as YYYY-MM-DD",
    )?;
    refuse_blank(
        "--before",
        args.before.as_deref(),
        "a cursor copied from a page's next_cursor",
    )?;
    let fields = columns(&args.fields)?;
    let agent = client.resolve_agent_ref(&args.agent).await?;
    let query = TurnsQuery {
        day: args.day.clone(),
        before: args.before.clone(),
        limit: args.limit,
    };
    if args.json {
        let document = client.turns_page_document(&agent, &query).await?;
        writeln!(out, "{document}")?;
        return Ok(());
    }
    let page = client.turns_page(&agent, &query).await?;
    check_schema(page.schema_version)?;
    write!(out, "{}", render_list(&page, &fields, args.tools))?;
    Ok(())
}

/// A flag whose value is whitespace only names nothing: refused here, where the
/// message can name the flag, instead of on the wire.
fn refuse_blank(flag: &str, value: Option<&str>, what: &str) -> Result<()> {
    if value.is_some_and(|raw| raw.trim().is_empty()) {
        bail!("{flag} takes {what}; a blank value is not one");
    }
    Ok(())
}

/// Refuse a page whose view version this build does not know: the routes bump
/// `schema_version` when a field changes meaning, so a reader that kept going
/// would print the new field's values under the old name. The faces that hand
/// the tagma's document over byte for byte (`--json`, `export`) pass the
/// version through instead of judging it.
fn check_schema(version: u32) -> Result<()> {
    if version != TURN_SCHEMA_VERSION {
        bail!(
            "the tagma answered with turns view v{version}, and this build reads v{TURN_SCHEMA_VERSION}; run the kallip built for that view, or bring the tagma and this CLI onto the same one"
        );
    }
    Ok(())
}

/// Every column name a row can offer, taken from the row type itself: a probe
/// row fills every optional block, so its JSON object names every field the
/// page can render. One source for the names, so a new field is selectable the
/// day it reaches the wire.
fn field_names() -> Vec<String> {
    let probe = TurnRow {
        turn_id: 0,
        at: String::new(),
        profile: Some(TurnProfile {
            profile_set: String::new(),
            profile_id: String::new(),
            provider: String::new(),
            model: String::new(),
            effort: None,
        }),
        usage: Some(TurnUsageView::default()),
        elapsed_ms: Some(0),
        calls: Vec::new(),
        messages: Vec::new(),
    };
    let value = serde_json::to_value(&probe)
        .expect("a turn row holds only serializable fields, so a probe cannot fail");
    match value {
        Value::Object(map) => map.keys().cloned().collect(),
        other => unreachable!("a struct serializes to a JSON object, got {other}"),
    }
}

/// Resolve `--fields` into column names, refusing any name the row does not
/// carry: silently dropping one would print a table that lies about the
/// columns it was asked for.
fn columns(requested: &[String]) -> Result<Vec<String>> {
    let available = field_names();
    if requested.is_empty() {
        return Ok(DEFAULT_FIELDS
            .iter()
            .map(|name| (*name).to_string())
            .collect());
    }
    for name in requested {
        if !available.iter().any(|known| known == name) {
            bail!(
                "no field '{name}'; the page offers: {}",
                available.join(", ")
            );
        }
    }
    Ok(requested.to_vec())
}

/// The page as a table, one line per turn, plus a footer naming the page's
/// own bounds: what a reader needs to ask for the next one.
fn render_list(page: &TurnsPage, fields: &[String], tools: bool) -> String {
    let values: Vec<Value> = page
        .rows
        .iter()
        .map(|row| serde_json::to_value(row).expect("a turn row holds only serializable fields"))
        .collect();
    let cells: Vec<Vec<String>> = values
        .iter()
        .map(|value| fields.iter().map(|field| cell(field, value)).collect())
        .collect();
    let mut widths: Vec<usize> = fields.iter().map(|field| field.chars().count()).collect();
    for line in &cells {
        for (index, text) in line.iter().enumerate() {
            widths[index] = widths[index].max(text.chars().count());
        }
    }
    let mut text = String::new();
    let header: Vec<&str> = fields.iter().map(|field| field.as_str()).collect();
    text.push_str(&padded(&header, &widths));
    text.push('\n');
    if page.rows.is_empty() {
        text.push_str("(no turns)\n");
    }
    for (index, line) in cells.iter().enumerate() {
        let cells: Vec<&str> = line.iter().map(|cell| cell.as_str()).collect();
        text.push_str(&padded(&cells, &widths));
        text.push('\n');
        if tools {
            for call in &page.rows[index].calls {
                text.push_str("  ");
                text.push_str(&call_line(call));
                text.push('\n');
            }
        }
    }
    text.push_str(&format!(
        "rows={} bad_lines={} page_truncated={} next_cursor={}\n",
        page.rows.len(),
        page.bad_lines,
        page.page_truncated,
        page.next_cursor.as_deref().unwrap_or("-"),
    ));
    text
}

/// One table line: cells joined by two spaces, padded to the column widths.
fn padded(cells: &[&str], widths: &[usize]) -> String {
    let mut line = String::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        for _ in cell.chars().count()..widths[index] {
            line.push(' ');
        }
    }
    line.trim_end().to_string()
}

/// One cell, read off the row's own JSON: a field the record does not carry
/// reads `-`, and a field this table has no shorter form for reads as its own
/// compact value, never as a blank.
fn cell(field: &str, row: &Value) -> String {
    let value = match row.get(field) {
        None | Some(Value::Null) => return "-".to_string(),
        Some(value) => value,
    };
    match field {
        "profile" => format!("{}/{}", text(&value["provider"]), text(&value["model"])),
        "usage" => usage_cell(value),
        "elapsed_ms" => format!("{}ms", text(value)),
        "calls" | "messages" => value
            .as_array()
            .map(|entries| entries.len())
            .unwrap_or(0)
            .to_string(),
        _ => text(value),
    }
}

/// The `usage` column: prompt and completion first, then the cache counters
/// (read/write) and the reasoning counter when the backend reported them, each
/// under its own name, so a row that spent its budget on cache or thinking
/// cannot look like a plain round and no reader has to guess a symbol.
fn usage_cell(value: &Value) -> String {
    let mut text = format!("{}→{}", text(&value["prompt"]), text(&value["completion"]));
    let cache_read = count(&value["cache_read"]);
    let cache_write = count(&value["cache_write"]);
    if cache_read > 0 || cache_write > 0 {
        text.push_str(&format!(" cache:{cache_read}/{cache_write}"));
    }
    let reasoning = count(&value["reasoning"]);
    if reasoning > 0 {
        text.push_str(&format!(" reason:{reasoning}"));
    }
    text
}

/// A counter read off the row; a counter the record omitted reads zero, as the
/// view itself defines it.
fn count(value: &Value) -> u64 {
    value.as_u64().unwrap_or(0)
}

/// A JSON value as one line of text: a string unquoted, everything else in
/// its own compact form.
fn text(value: &Value) -> String {
    match value {
        Value::String(string) => string.clone(),
        Value::Null => "-".to_string(),
        other => other.to_string(),
    }
}

async fn show(
    client: &TagmaClient,
    args: &TurnsShowArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    if (args.offset.is_some() || args.limit.is_some()) && !args.body {
        bail!("--offset and --limit address a text range; they only apply with --body");
    }
    let body_index = if args.body {
        Some(
            args.call
                .ok_or_else(|| anyhow!("--body reads one call's text: name it with --call N"))?,
        )
    } else {
        None
    };
    let agent = client.resolve_agent_ref(&args.agent).await?;
    let range = CallBodyQuery {
        offset: args.offset,
        limit: args.limit,
    };
    if let Some(index) = body_index {
        if args.json {
            let document = client
                .call_body_document(&agent, args.turn, index, &range)
                .await?;
            writeln!(out, "{document}")?;
            return Ok(());
        }
        let body = client.call_body(&agent, args.turn, index, &range).await?;
        check_schema(body.schema_version)?;
        write_body_notices(&body, err)?;
        out.write_all(body.text.as_bytes())?;
        out.flush()?;
        return Ok(());
    }
    if args.json {
        let text = client.turn_document(&agent, args.turn).await?;
        match args.call {
            Some(index) => {
                let document: Value =
                    serde_json::from_str(&text).context("read the turn the tagma sent")?;
                let call = find_call_in_document(&document, args.turn, index)?;
                writeln!(out, "{}", serde_json::to_string_pretty(call)?)?;
            }
            None => writeln!(out, "{text}")?,
        }
        return Ok(());
    }
    let detail = client.turn(&agent, args.turn).await?;
    check_schema(detail.schema_version)?;
    let agent_text = agent.to_string();
    match args.call {
        Some(index) => {
            let call = find_call(&detail, index)?;
            write!(out, "{}", render_call(call, &agent_text, args.turn))?;
        }
        None => write!(out, "{}", render_turn(&detail))?,
    }
    Ok(())
}

/// The error a wrong `--call` earns, built from the indices the turn carried.
fn no_such_call(turn: u64, index: u32, carried: &[u64]) -> anyhow::Error {
    let list = if carried.is_empty() {
        "none".to_string()
    } else {
        carried
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    anyhow!("turn {turn} carried no call {index}; its calls: {list}")
}

/// The call a `--call N` names in the document the tagma sent, or the error
/// saying why it cannot be named: a list this build cannot read, or a miss
/// listing the indices the document carried. A newer view keeps its unknown
/// fields here, which is the whole point of the `--json` face.
fn find_call_in_document(document: &Value, turn: u64, index: u32) -> Result<&Value> {
    let view = document
        .get("schema_version")
        .and_then(Value::as_u64)
        .map_or_else(|| "unknown".to_string(), |found| found.to_string());
    let calls = document
        .pointer("/turn/calls")
        .and_then(Value::as_array)
        .with_context(|| format!("the calls of turn {turn} do not read as a list (view {view})"))?;
    calls
        .iter()
        .find(|call| call["call_index"].as_u64() == Some(u64::from(index)))
        .ok_or_else(|| {
            let carried: Vec<u64> = calls
                .iter()
                .filter_map(|call| call["call_index"].as_u64())
                .collect();
            no_such_call(turn, index, &carried)
        })
}

/// The call a `--call N` names, or the error listing the indices the turn
/// did carry.
fn find_call(detail: &TurnDetail, index: u32) -> Result<&CallRow> {
    detail
        .turn
        .calls
        .iter()
        .find(|call| call.call_index == index)
        .ok_or_else(|| {
            let carried: Vec<u64> = detail
                .turn
                .calls
                .iter()
                .map(|call| u64::from(call.call_index))
                .collect();
            no_such_call(detail.turn.turn_id, index, &carried)
        })
}

/// Where a text range sits and what came of it, on stderr: the payload on
/// stdout stays exactly the range the tagma returned.
fn write_body_notices(body: &CallBody, err: &mut dyn Write) -> Result<()> {
    let end = body.offset + body.text.len() as u64;
    writeln!(
        err,
        "call {} ({}) covers bytes {}..{end} of {}",
        body.call_index, body.name, body.offset, body.total_bytes
    )?;
    match body.next_offset {
        Some(next) => writeln!(err, "the range stops early; ask for offset {next} to go on")?,
        None => writeln!(err, "the range reaches the end of the text")?,
    }
    if body.reclaimed > 0 {
        writeln!(err, "{} spill path(s) {SPILL_RECLAIMED}", body.reclaimed)?;
    }
    Ok(())
}

/// One call as a line: what a list scans for, the index first.
fn call_line(call: &CallRow) -> String {
    let mut line = format!("[{}] {}", call.call_index, call.name);
    if call.pending_approval == Some(true) {
        line.push_str(" awaiting approval");
    } else {
        line.push_str(if call.ok { " ok" } else { " failed" });
    }
    if let Some(code) = call.exit_code {
        line.push_str(&format!(" exit={code}"));
    }
    if call.timed_out == Some(true) {
        line.push_str(" timed_out");
    }
    if call.truncated {
        line.push_str(" truncated");
    }
    if let Some(error) = &call.first_error_line {
        line.push_str(": ");
        line.push_str(error);
    }
    line
}

fn render_call(call: &CallRow, agent: &str, turn: u64) -> String {
    let mut text = format!("{}\n", call_line(call));
    text.push_str(&format!("args:   {}\n", fold(&call.args)));
    text.push_str(&stream_line("stdout", call.stdout_bytes, call.stdout_lines));
    text.push_str(&stream_line("stderr", call.stderr_bytes, call.stderr_lines));
    text.push_str(&format!(
        "text:   kallip turns show {agent} {turn} --call {} --body\n",
        call.call_index
    ));
    text
}

/// A stream's size as the record holds it, spill paths included; `-` when the
/// record carried no such stream at all.
fn stream_line(label: &str, bytes: Option<u64>, lines: Option<u64>) -> String {
    match bytes {
        Some(bytes) => format!(
            "{label}: {bytes} bytes / {} lines (as recorded)\n",
            lines.unwrap_or(0)
        ),
        None => format!("{label}: -\n"),
    }
}

/// Text folded onto one line, so a record that carries newlines cannot break
/// the layout around it.
fn fold(value: &str) -> String {
    value.lines().collect::<Vec<_>>().join(" ")
}

fn render_turn(detail: &TurnDetail) -> String {
    let row = &detail.turn;
    let mut text = format!("turn {}  at {}\n", row.turn_id, row.at);
    text.push_str("  profile  ");
    match &row.profile {
        Some(profile) => {
            text.push_str(&format!("{}/{}", profile.provider, profile.model));
            text.push_str(&format!(
                " ({}/{})",
                profile.profile_set, profile.profile_id
            ));
            if let Some(effort) = &profile.effort {
                text.push_str(&format!(" effort={effort}"));
            }
        }
        None => text.push('-'),
    }
    text.push('\n');
    text.push_str("  usage    ");
    match &row.usage {
        Some(usage) => text.push_str(&format!(
            "prompt={} completion={} cache_read={} cache_write={} reasoning={}",
            usage.prompt, usage.completion, usage.cache_read, usage.cache_write, usage.reasoning
        )),
        None => text.push('-'),
    }
    text.push('\n');
    match row.elapsed_ms {
        Some(elapsed) => text.push_str(&format!("  elapsed  {elapsed}ms\n")),
        None => text.push_str("  elapsed  -\n"),
    }
    text.push_str(&format!("  messages {}\n", row.messages.len()));
    for message in &row.messages {
        text.push_str("    ");
        text.push_str(&message_line(message));
        text.push('\n');
    }
    text.push_str(&format!("  calls    {}\n", row.calls.len()));
    for call in &row.calls {
        text.push_str("    ");
        text.push_str(&call_line(call));
        text.push('\n');
    }
    text
}

/// One message of a turn: its shape, and the shortened text the detail face
/// carries, folded onto one line.
fn message_line(message: &MessageRow) -> String {
    let mut line = format!("{} {} chars", message.role, message.chars);
    if message.has_reasoning {
        line.push_str(" reasoning");
    }
    if message.truncated {
        line.push_str(" truncated");
    }
    if let Some(text) = &message.text {
        line.push_str(": ");
        line.push_str(&fold(text));
    }
    line
}

/// Walk pages up to the bound and emit one document: a manifest saying which
/// tagma, which agent, which window, and whether the walk reached the end of
/// the log or stopped at the bound — plus the pages themselves, exactly as the
/// tagma returned them. The bound is not a convenience: an export that could
/// walk forever would be a way to pull a whole log into memory by accident.
async fn export(
    client: &TagmaClient,
    args: &TurnsExportArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    if args.pages == 0 {
        bail!("--pages bounds the walk; ask for at least one page");
    }
    refuse_blank(
        "--day",
        args.day.as_deref(),
        "the UTC day file to read, as YYYY-MM-DD",
    )?;
    let agent = client.resolve_agent_ref(&args.agent).await?;
    let mut pages: Vec<TurnsPage> = Vec::new();
    let mut documents: Vec<Value> = Vec::new();
    let mut view_version: Option<u32> = None;
    let mut before: Option<String> = None;
    let mut rows = 0usize;
    let mut bad_lines = 0u32;
    let mut stopped_by = StoppedBy::Pages;
    for _ in 0..args.pages {
        let text = client
            .turns_page_document(
                &agent,
                &TurnsQuery {
                    day: args.day.clone(),
                    before: before.clone(),
                    limit: args.limit,
                },
            )
            .await?;
        let document: Value =
            serde_json::from_str(&text).context("read the page the tagma sent")?;
        let page: TurnsPage =
            serde_json::from_value(document.clone()).context("parse turn page")?;
        match view_version {
            Some(version) if version != page.schema_version => bail!(
                "the tagma answered in view v{} after v{version}; one document cannot mix two views",
                page.schema_version
            ),
            _ => view_version = Some(page.schema_version),
        }
        documents.push(document);
        rows += page.rows.len();
        bad_lines += page.bad_lines;
        let next = page.next_cursor.clone();
        if next.is_some() && next == before {
            bail!(
                "the tagma handed back the cursor this page was asked for; stopping instead of walking the same page again"
            );
        }
        before = next;
        let exhausted = before.is_none();
        pages.push(page);
        if exhausted {
            stopped_by = StoppedBy::End;
            break;
        }
    }
    let page_count = pages.len();
    let first = pages.first().expect("the loop runs at least once");
    let newest_at = first.rows.first().map(|row| row.at.clone());
    let oldest_at = pages
        .iter()
        .rev()
        .find_map(|page| page.rows.last())
        .map(|row| row.at.clone());
    let manifest = ExportManifest {
        tagma: client.base_url().to_string(),
        agent: agent.to_string(),
        view: ExportView {
            name: "turns",
            schema_version: first.schema_version,
        },
        window: ExportWindow {
            day: args.day.clone(),
            newest_at,
            oldest_at,
            pages: page_count,
            stopped_by,
            next_cursor: before.clone(),
            limit: args.limit,
        },
        counts: ExportCounts { rows, bad_lines },
    };
    let document = serde_json::json!({ "manifest": manifest, "pages": documents });
    let text = serde_json::to_string_pretty(&document)?;
    match &args.out {
        Some(path) => {
            std::fs::write(path, text.as_bytes())
                .with_context(|| format!("write {}", path.display()))?;
            writeln!(
                err,
                "wrote {page_count} page(s) / {rows} row(s) to {} (stopped by {})",
                path.display(),
                stopped_by.as_str()
            )?;
        }
        None => writeln!(out, "{text}")?,
    }
    Ok(())
}

/// The manifest of one export: which tagma, which agent, which view version,
/// the window walked, and whether the walk reached the end of the log or
/// stopped at the bound. A walk stopped at the bound carries the cursor and
/// page size a later walk resumes from, so the bound never costs coverage.
#[derive(Serialize)]
struct ExportManifest {
    tagma: String,
    agent: String,
    view: ExportView,
    window: ExportWindow,
    counts: ExportCounts,
}

#[derive(Serialize)]
struct ExportView {
    name: &'static str,
    schema_version: u32,
}

#[derive(Serialize)]
struct ExportWindow {
    day: Option<String>,
    newest_at: Option<String>,
    oldest_at: Option<String>,
    pages: usize,
    stopped_by: StoppedBy,
    next_cursor: Option<String>,
    limit: Option<u32>,
}

#[derive(Clone, Copy)]
enum StoppedBy {
    End,
    Pages,
}

impl StoppedBy {
    /// The one spelling of a stop name: the notice prints it and the manifest
    /// serializes it, so the two faces cannot drift apart.
    fn as_str(self) -> &'static str {
        match self {
            StoppedBy::End => "end",
            StoppedBy::Pages => "pages",
        }
    }
}

impl Serialize for StoppedBy {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Serialize)]
struct ExportCounts {
    rows: usize,
    bad_lines: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallipai_common::protocol::{
        CALL_BODY_MAX_BYTES, TURN_SCHEMA_VERSION, TURNS_PAGE_LIMIT_MAX,
    };
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const AGENT: &str = "3f1c0a52-0000-4000-8000-000000000001";

    fn call(index: u32, name: &str) -> CallRow {
        CallRow {
            call_index: index,
            call_id: format!("call-{index}"),
            name: name.to_string(),
            args: "{}".to_string(),
            ok: true,
            pending_approval: None,
            stdout_bytes: Some(12),
            stderr_bytes: Some(0),
            stdout_lines: Some(2),
            stderr_lines: Some(0),
            exit_code: Some(0),
            timed_out: Some(false),
            truncated: false,
            first_error_line: None,
        }
    }

    fn message(role: &str) -> MessageRow {
        MessageRow {
            role: role.to_string(),
            chars: 5,
            has_reasoning: false,
            truncated: false,
            text: Some("hello".to_string()),
            reasoning: None,
            reasoning_truncated: false,
        }
    }

    fn row(turn_id: u64) -> TurnRow {
        TurnRow {
            turn_id,
            at: "2026-10-24T10:00:00Z".to_string(),
            profile: Some(TurnProfile {
                profile_set: "default".to_string(),
                profile_id: "main".to_string(),
                provider: "openai".to_string(),
                model: "gpt-5".to_string(),
                effort: None,
            }),
            usage: Some(TurnUsageView {
                prompt: 100,
                completion: 20,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            }),
            elapsed_ms: Some(1234),
            calls: vec![call(0, "bash")],
            messages: vec![message("user")],
        }
    }

    /// A row whose `at` follows its turn id, so a bound that reads the wrong
    /// end of the walk cannot pass by coincidence.
    fn stamped(turn_id: u64) -> TurnRow {
        let mut row = row(turn_id);
        row.at = format!("2026-10-24T10:{:02}:00Z", turn_id % 60);
        row
    }

    fn page(rows: Vec<TurnRow>, next: Option<&str>) -> TurnsPage {
        TurnsPage {
            schema_version: TURN_SCHEMA_VERSION,
            rows,
            bad_lines: 0,
            next_cursor: next.map(|cursor| cursor.to_string()),
            page_truncated: false,
        }
    }

    fn client(server: &MockServer) -> TagmaClient {
        TagmaClient::builder(&server.uri()).build().unwrap()
    }

    fn turns_path() -> String {
        format!("/agents/{AGENT}/turns")
    }

    fn list_args() -> TurnsListArgs {
        TurnsListArgs {
            agent: AGENT.to_string(),
            day: None,
            before: None,
            limit: None,
            fields: Vec::new(),
            tools: false,
            json: false,
        }
    }

    fn show_args(turn: u64) -> TurnsShowArgs {
        TurnsShowArgs {
            agent: AGENT.to_string(),
            turn,
            call: None,
            body: false,
            offset: None,
            limit: None,
            json: false,
        }
    }

    fn export_args(pages: u32) -> TurnsExportArgs {
        TurnsExportArgs {
            agent: AGENT.to_string(),
            day: None,
            limit: None,
            pages,
            out: None,
        }
    }

    fn serve(body: &(impl serde::Serialize + ?Sized)) -> ResponseTemplate {
        let text = serde_json::to_string(body).expect("fixture serializes");
        ResponseTemplate::new(200).set_body_string(text)
    }

    /// The default columns have to exist on the view, or the bare command
    /// prints a table of dashes.
    #[test]
    fn default_fields_are_part_of_the_view() {
        let available = field_names();
        for field in DEFAULT_FIELDS {
            assert!(
                available.iter().any(|known| known == field),
                "default column '{field}' is not a row field: {available:?}"
            );
        }
    }

    #[test]
    fn unknown_field_is_rejected_with_the_available_names() {
        let err = columns(&["turn_id".to_string(), "bogus".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("bogus"), "{err}");
        assert!(err.contains("elapsed_ms"), "{err}");
    }

    #[test]
    fn selected_fields_render_only_those_columns() {
        let fixture = page(vec![row(1)], None);
        let text = render_list(
            &fixture,
            &["turn_id".to_string(), "usage".to_string()],
            false,
        );
        assert!(text.starts_with("turn_id  usage\n"), "{text}");
        assert!(!text.contains("elapsed_ms"), "{text}");
        assert!(text.contains("100→20"), "{text}");
    }

    #[test]
    fn tool_expansion_lists_each_call_name() {
        let mut fixture_row = row(7);
        fixture_row.calls = vec![call(0, "bash"), call(1, "read_file")];
        let fixture = page(vec![fixture_row], None);
        let text = render_list(&fixture, &["turn_id".to_string()], true);
        let names: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("  ["))
            .collect();
        assert_eq!(names.len(), 2, "{text}");
        assert!(names[0].contains("bash"), "{text}");
        assert!(names[1].contains("read_file"), "{text}");
    }

    /// The cursor is the tagma's ordering key: it goes back untouched, so the
    /// mock here matches only the exact bytes it handed out.
    #[tokio::test]
    async fn cursor_is_carried_verbatim() {
        let server = MockServer::start().await;
        let fixture = page(vec![row(9)], None);
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .and(query_param("before", "a b%c+d"))
            .respond_with(serve(&fixture))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = list_args();
        args.before = Some("a b%c+d".to_string());
        let mut out = Vec::new();
        list(&client, &args, &mut out).await.unwrap();
        let text = String::from_utf8(out).expect("utf-8 table");
        assert!(text.contains('9'), "{text}");
    }

    /// `--json` hands over the tagma's document byte for byte, so a field this
    /// build does not know still reaches the reader: that is the whole reason
    /// the audited faces exist.
    #[tokio::test]
    async fn json_page_is_the_tagma_document_verbatim() {
        let server = MockServer::start().await;
        let sent = r#"{"schema_version":1,"rows":[{"turn_id":3,"future_field":7}],"bad_lines":0,"next_cursor":null,"page_truncated":false}"#;
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .respond_with(ResponseTemplate::new(200).set_body_string(sent))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = list_args();
        args.json = true;
        let mut out = Vec::new();
        list(&client, &args, &mut out).await.unwrap();
        assert_eq!(
            String::from_utf8(out).expect("utf-8 document"),
            format!("{sent}\n")
        );
    }

    /// A walk that reaches the end of the log says so, and its counts cover
    /// every page it read.
    #[tokio::test]
    async fn export_walks_to_the_end_and_says_so() {
        let server = MockServer::start().await;
        let first = page(vec![row(2), row(1)], Some("cursor-2"));
        let second = page(vec![row(0)], None);
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .and(query_param_is_missing("before"))
            .respond_with(serve(&first))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .and(query_param("before", "cursor-2"))
            .respond_with(serve(&second))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut out = Vec::new();
        let mut err = Vec::new();
        export(&client, &export_args(5), &mut out, &mut err)
            .await
            .unwrap();
        let document: Value = serde_json::from_slice(&out).expect("json document");
        assert_eq!(document["manifest"]["window"]["stopped_by"], "end");
        assert_eq!(document["manifest"]["window"]["pages"], 2);
        assert_eq!(document["manifest"]["counts"]["rows"], 3);
        assert_eq!(
            document["manifest"]["view"]["schema_version"],
            TURN_SCHEMA_VERSION
        );
        assert_eq!(document["manifest"]["tagma"], server.uri());
        assert_eq!(document["pages"].as_array().unwrap().len(), 2);
    }

    /// A walk that hits the bound says that too, instead of passing itself off
    /// as the whole log.
    #[tokio::test]
    async fn export_stops_at_the_bound_and_says_so() {
        let server = MockServer::start().await;
        let first = page(vec![row(2)], Some("cursor-2"));
        let second = page(vec![row(1)], Some("cursor-3"));
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .and(query_param_is_missing("before"))
            .respond_with(serve(&first))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .and(query_param("before", "cursor-2"))
            .respond_with(serve(&second))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut out = Vec::new();
        let mut err = Vec::new();
        export(&client, &export_args(2), &mut out, &mut err)
            .await
            .unwrap();
        let document: Value = serde_json::from_slice(&out).expect("json document");
        assert_eq!(document["manifest"]["window"]["stopped_by"], "pages");
        assert_eq!(document["manifest"]["window"]["pages"], 2);
        assert_eq!(document["manifest"]["counts"]["rows"], 2);
    }

    #[tokio::test]
    async fn show_narrows_to_one_call() {
        let server = MockServer::start().await;
        let mut fixture_row = row(142);
        fixture_row.calls = vec![call(0, "bash"), call(1, "read_file")];
        let detail = TurnDetail {
            schema_version: TURN_SCHEMA_VERSION,
            turn: fixture_row,
        };
        Mock::given(method("GET"))
            .and(path(format!("/agents/{AGENT}/turns/142")))
            .respond_with(serve(&detail))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = show_args(142);
        args.call = Some(1);
        let mut out = Vec::new();
        let mut err = Vec::new();
        show(&client, &args, &mut out, &mut err).await.unwrap();
        let text = String::from_utf8(out).expect("utf-8 turn");
        assert!(text.contains("read_file"), "{text}");
        assert!(!text.contains("bash"), "{text}");
    }

    /// A wrong index names the calls the turn did carry: the reader should not
    /// have to ask for the whole turn again to learn them.
    #[tokio::test]
    async fn show_rejects_an_absent_call() {
        let server = MockServer::start().await;
        let mut fixture_row = row(142);
        fixture_row.calls = vec![call(0, "bash"), call(1, "read_file")];
        let detail = TurnDetail {
            schema_version: TURN_SCHEMA_VERSION,
            turn: fixture_row,
        };
        Mock::given(method("GET"))
            .and(path(format!("/agents/{AGENT}/turns/142")))
            .respond_with(serve(&detail))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = show_args(142);
        args.call = Some(9);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let message = show(&client, &args, &mut out, &mut err)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("no call 9"), "{message}");
        assert!(message.contains("0, 1"), "{message}");
    }

    /// The `--json` face names the same miss, built from the document it
    /// walked: a pointer that missed would report a real call as absent.
    #[tokio::test]
    async fn show_json_rejects_an_absent_call() {
        let server = MockServer::start().await;
        let mut fixture_row = row(142);
        fixture_row.calls = vec![call(0, "bash"), call(1, "read_file")];
        let detail = TurnDetail {
            schema_version: TURN_SCHEMA_VERSION,
            turn: fixture_row,
        };
        Mock::given(method("GET"))
            .and(path(format!("/agents/{AGENT}/turns/142")))
            .respond_with(serve(&detail))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = show_args(142);
        args.call = Some(9);
        args.json = true;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let message = show(&client, &args, &mut out, &mut err)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("carried no call 9"), "{message}");
        assert!(message.contains("its calls: 0, 1"), "{message}");
        assert!(out.is_empty(), "the document is not written on a miss");
    }

    /// A call list this build cannot read is not the same news as a call the
    /// turn never carried: the reader gets one sentence or the other.
    #[tokio::test]
    async fn show_json_reports_a_call_list_it_cannot_read() {
        let server = MockServer::start().await;
        let sent = serde_json::json!({
            "schema_version": TURN_SCHEMA_VERSION,
            "turn": { "turn_id": 142, "calls": "not a list" },
        });
        Mock::given(method("GET"))
            .and(path(format!("/agents/{AGENT}/turns/142")))
            .respond_with(serve(&sent))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = show_args(142);
        args.call = Some(9);
        args.json = true;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let message = show(&client, &args, &mut out, &mut err)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("do not read as a list"), "{message}");
        assert!(
            message.contains(&format!("view {TURN_SCHEMA_VERSION}")),
            "{message}"
        );
        assert!(!message.contains("its calls: none"), "{message}");
    }

    /// The text range is the payload: stdout carries its bytes alone, and the
    /// notices that help a reader continue stay on stderr.
    #[tokio::test]
    async fn body_text_is_stdout() {
        let server = MockServer::start().await;
        let body = CallBody {
            schema_version: TURN_SCHEMA_VERSION,
            turn_id: 142,
            call_index: 1,
            call_id: "call-1".to_string(),
            name: "bash".to_string(),
            offset: 0,
            total_bytes: 9,
            text: "abcde".to_string(),
            truncated: true,
            next_offset: Some(5),
            reclaimed: 1,
        };
        Mock::given(method("GET"))
            .and(path(format!("/agents/{AGENT}/turns/142/calls/1/body")))
            .respond_with(serve(&body))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = show_args(142);
        args.call = Some(1);
        args.body = true;
        let mut out = Vec::new();
        let mut err = Vec::new();
        show(&client, &args, &mut out, &mut err).await.unwrap();
        assert_eq!(out, b"abcde".to_vec());
        assert!(!err.is_empty(), "a range read notices on stderr");
    }

    /// `--json` on a body hands the range's document over instead of the raw
    /// text, and leaves stderr to whoever reads it next.
    #[tokio::test]
    async fn json_body_is_the_tagma_document_verbatim() {
        let server = MockServer::start().await;
        let body = CallBody {
            schema_version: TURN_SCHEMA_VERSION,
            turn_id: 142,
            call_index: 1,
            call_id: "call-1".to_string(),
            name: "bash".to_string(),
            offset: 0,
            total_bytes: 5,
            text: "abcde".to_string(),
            truncated: false,
            next_offset: None,
            reclaimed: 0,
        };
        Mock::given(method("GET"))
            .and(path(format!("/agents/{AGENT}/turns/142/calls/1/body")))
            .respond_with(serve(&body))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = show_args(142);
        args.call = Some(1);
        args.body = true;
        args.json = true;
        let mut out = Vec::new();
        let mut err = Vec::new();
        show(&client, &args, &mut out, &mut err).await.unwrap();
        let printed: Value = serde_json::from_slice(&out).expect("json body");
        assert_eq!(printed, serde_json::to_value(&body).unwrap());
        assert!(err.is_empty(), "a json read writes no notices");
        assert_eq!(
            String::from_utf8(out).expect("utf-8 document"),
            format!(
                "{}\n",
                serde_json::to_string(&body).expect("body serializes")
            )
        );
    }

    /// A bound of zero asks for nothing: the walk refuses before it reads.
    #[tokio::test]
    async fn export_refuses_a_zero_page_bound() {
        let server = MockServer::start().await;
        let client = client(&server);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let message = export(&client, &export_args(0), &mut out, &mut err)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("--pages"), "{message}");
        assert!(out.is_empty(), "nothing is printed for an empty walk");
    }

    /// A whole turn is what an operator reads first, so every message and
    /// every call of it has to reach the page.
    #[test]
    fn render_turn_lists_every_message_and_call() {
        let mut fixture_row = row(142);
        fixture_row.messages = vec![message("user"), message("assistant")];
        fixture_row.calls = vec![call(0, "bash"), call(1, "read_file")];
        let detail = TurnDetail {
            schema_version: TURN_SCHEMA_VERSION,
            turn: fixture_row,
        };
        let text = render_turn(&detail);
        for needle in ["user", "assistant", "bash", "read_file"] {
            assert!(text.contains(needle), "{needle} is missing: {text}");
        }
    }

    /// With `--json`, `--call N` narrows the JSON to that call's document
    /// rather than printing the whole turn.
    #[tokio::test]
    async fn json_call_narrows_to_one_call() {
        let server = MockServer::start().await;
        let mut fixture_row = row(142);
        fixture_row.calls = vec![call(0, "bash"), call(1, "read_file")];
        let wanted = fixture_row.calls[1].clone();
        let detail = TurnDetail {
            schema_version: TURN_SCHEMA_VERSION,
            turn: fixture_row,
        };
        Mock::given(method("GET"))
            .and(path(format!("/agents/{AGENT}/turns/142")))
            .respond_with(serve(&detail))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = show_args(142);
        args.call = Some(1);
        args.json = true;
        let mut out = Vec::new();
        let mut err = Vec::new();
        show(&client, &args, &mut out, &mut err).await.unwrap();
        let printed: Value = serde_json::from_slice(&out).expect("json call");
        assert_eq!(printed, serde_json::to_value(&wanted).unwrap());
    }

    /// Column selection shapes the table, so it has no meaning beside the
    /// tagma's own document; the command says so instead of ignoring it.
    #[tokio::test]
    async fn json_page_rejects_column_selection() {
        let server = MockServer::start().await;
        let client = client(&server);
        let mut args = list_args();
        args.json = true;
        args.fields = vec!["turn_id".to_string()];
        let mut out = Vec::new();
        let message = list(&client, &args, &mut out)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("--fields"), "{message}");
    }

    /// A range offset means nothing outside a body read, so it is refused
    /// rather than dropped.
    #[tokio::test]
    async fn offset_is_rejected_outside_a_body_read() {
        let server = MockServer::start().await;
        let client = client(&server);
        let mut args = show_args(142);
        args.offset = Some(16);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let message = show(&client, &args, &mut out, &mut err)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("--body"), "{message}");
    }

    /// The selectable names are a contract, so this list is the nail: a field
    /// that leaves the view cannot quietly stop being selectable.
    #[test]
    fn the_selectable_names_are_pinned() {
        let mut names = field_names();
        names.sort();
        let expected: Vec<String> = [
            "at",
            "calls",
            "elapsed_ms",
            "messages",
            "profile",
            "turn_id",
            "usage",
        ]
        .iter()
        .map(|name| (*name).to_string())
        .collect();
        assert_eq!(names, expected);
    }

    /// The help prose quotes the default columns and the wire bounds by hand,
    /// so a test holds that copy to the constants it quotes.
    #[test]
    fn the_help_prose_quotes_the_live_bounds() {
        use clap::Subcommand as _;
        let command =
            TurnsCommand::augment_subcommands(clap::Command::new("kallip turns")).term_width(0);
        let list = command.find_subcommand("list").expect("list subcommand");
        let fields = help_text(list, "fields");
        assert!(fields.contains(&DEFAULT_FIELDS.join(",")), "{fields}");
        let page_limit = help_text(list, "limit");
        assert!(
            page_limit.contains(&format!("1..={TURNS_PAGE_LIMIT_MAX}")),
            "{page_limit}"
        );
        let show = command.find_subcommand("show").expect("show subcommand");
        let body_limit = help_text(show, "limit");
        assert!(
            body_limit.contains(&format!("1..={CALL_BODY_MAX_BYTES}")),
            "{body_limit}"
        );

        let export = command
            .find_subcommand("export")
            .expect("export subcommand");
        let export_limit = help_text(export, "limit");
        let live_page_bound = format!("1..={TURNS_PAGE_LIMIT_MAX}");
        assert!(export_limit.contains(&live_page_bound), "{export_limit}");
    }

    fn help_text(command: &clap::Command, id: &str) -> String {
        let arg = command
            .get_arguments()
            .find(|arg| arg.get_id().as_str() == id)
            .unwrap_or_else(|| panic!("no argument {id}"));
        let short = arg
            .get_help()
            .map(|help| help.to_string())
            .unwrap_or_default();
        let long = arg
            .get_long_help()
            .map(|help| help.to_string())
            .unwrap_or_default();
        format!("{short} {long}")
    }

    /// A call that stopped to ask for approval did not finish, so its row says
    /// what it waits for instead of reading as a success.
    #[test]
    fn a_call_awaiting_approval_says_so() {
        let mut deferred = call(0, "bash");
        deferred.pending_approval = Some(true);
        let line = call_line(&deferred);
        assert!(line.contains("awaiting approval"), "{line}");
        assert!(!line.contains(" ok"), "{line}");
        let mut fixture_row = row(142);
        fixture_row.calls = vec![deferred];
        let detail = TurnDetail {
            schema_version: TURN_SCHEMA_VERSION,
            turn: fixture_row,
        };
        let text = render_turn(&detail);
        assert!(text.contains("awaiting approval"), "{text}");
    }

    /// A call that ran to an end says how it ended, so `awaiting approval` is
    /// a distinction rather than the only word a call row can carry.
    #[test]
    fn a_finished_call_reads_as_ok_or_failed() {
        let line = call_line(&call(0, "bash"));
        assert!(line.contains(" ok"), "{line}");
        assert!(!line.contains("awaiting approval"), "{line}");
        let mut broke = call(1, "bash");
        broke.ok = false;
        let line = call_line(&broke);
        assert!(line.contains(" failed"), "{line}");
    }

    /// The usage column names the cache and reasoning counters when the record
    /// carries them, so a round that spent its budget there cannot look plain.
    #[test]
    fn the_usage_column_names_every_counter_it_can() {
        let mut fixture_row = row(1);
        fixture_row.usage = Some(TurnUsageView {
            prompt: 100,
            completion: 20,
            cache_read: 5,
            cache_write: 0,
            reasoning: 7,
        });
        let value = serde_json::to_value(&fixture_row).expect("row serializes");
        assert_eq!(cell("usage", &value), "100\u{2192}20 cache:5/0 reason:7");
        let quiet = serde_json::to_value(row(2)).expect("row serializes");
        assert_eq!(cell("usage", &quiet), "100\u{2192}20");
    }

    /// The turn faces judge the view as the table does: a newer page would put
    /// the new field's values under the old names.
    #[tokio::test]
    async fn a_newer_view_is_refused_by_the_turn_faces() {
        let server = MockServer::start().await;
        let mut detail_row = row(142);
        detail_row.calls = vec![call(0, "bash")];
        let detail = TurnDetail {
            schema_version: TURN_SCHEMA_VERSION + 1,
            turn: detail_row,
        };
        Mock::given(method("GET"))
            .and(path(format!("/agents/{AGENT}/turns/142")))
            .respond_with(serve(&detail))
            .mount(&server)
            .await;
        let body = CallBody {
            schema_version: TURN_SCHEMA_VERSION + 1,
            turn_id: 142,
            call_index: 0,
            call_id: "call-0".to_string(),
            name: "bash".to_string(),
            offset: 0,
            total_bytes: 5,
            text: "later".to_string(),
            truncated: false,
            next_offset: None,
            reclaimed: 0,
        };
        Mock::given(method("GET"))
            .and(path(format!("/agents/{AGENT}/turns/142/calls/0/body")))
            .respond_with(serve(&body))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let newer = format!("v{}", TURN_SCHEMA_VERSION + 1);
        let message = show(&client, &show_args(142), &mut out, &mut err)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains(&newer), "{message}");
        let mut args = show_args(142);
        args.call = Some(0);
        args.body = true;
        let message = show(&client, &args, &mut out, &mut err)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains(&newer), "{message}");
        assert!(out.is_empty(), "a refused read prints nothing");
    }

    /// A page from a newer view is refused before it is rendered: the same
    /// names there may mean something else.
    #[tokio::test]
    async fn a_newer_view_is_refused_by_the_table() {
        let server = MockServer::start().await;
        let mut fixture = page(vec![row(1)], None);
        fixture.schema_version = TURN_SCHEMA_VERSION + 1;
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .respond_with(serve(&fixture))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut out = Vec::new();
        let message = list(&client, &list_args(), &mut out)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            message.contains(&format!("v{}", TURN_SCHEMA_VERSION + 1)),
            "{message}"
        );
        assert!(
            message.contains(&format!("v{TURN_SCHEMA_VERSION}")),
            "{message}"
        );
        assert!(out.is_empty(), "a refused page prints nothing");
    }

    /// The pass-through faces keep working on a newer view: the document
    /// carries its own version, so the reader judges it.
    #[tokio::test]
    async fn a_newer_view_still_passes_through_as_json() {
        let server = MockServer::start().await;
        let mut fixture = page(vec![row(1)], None);
        fixture.schema_version = TURN_SCHEMA_VERSION + 1;
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .respond_with(serve(&fixture))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = list_args();
        args.json = true;
        let mut out = Vec::new();
        list(&client, &args, &mut out).await.unwrap();
        let printed: Value = serde_json::from_slice(&out).expect("json out");
        assert_eq!(printed, serde_json::to_value(&fixture).unwrap());
    }

    /// Tool expansion shapes the table, so it is refused beside `--json`
    /// rather than dropped by the branch that returns early.
    #[tokio::test]
    async fn json_page_rejects_tool_expansion() {
        let server = MockServer::start().await;
        let client = client(&server);
        let mut args = list_args();
        args.json = true;
        args.tools = true;
        let mut out = Vec::new();
        let message = list(&client, &args, &mut out)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("--tools"), "{message}");
    }

    /// A size means nothing outside a body read.
    #[tokio::test]
    async fn limit_is_rejected_outside_a_body_read() {
        let server = MockServer::start().await;
        let client = client(&server);
        let mut args = show_args(142);
        args.limit = Some(16);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let message = show(&client, &args, &mut out, &mut err)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("--body"), "{message}");
    }

    /// A text read names the call it reads: refused before the wire, with the
    /// flag the operator has to add.
    #[tokio::test]
    async fn body_without_a_call_names_the_flag() {
        let server = MockServer::start().await;
        let client = client(&server);
        let mut args = show_args(142);
        args.body = true;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let message = show(&client, &args, &mut out, &mut err)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("--call"), "{message}");
    }

    /// A cursor copied from a page is bytes, not a sentence: an empty one is
    /// refused here instead of earning a server-side rejection.
    #[tokio::test]
    async fn an_empty_cursor_is_refused_before_the_wire() {
        let server = MockServer::start().await;
        let client = client(&server);
        let mut args = list_args();
        args.before = Some(String::new());
        let mut out = Vec::new();
        let message = list(&client, &args, &mut out)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("--before"), "{message}");
    }

    /// A column name the fan-out does not know is refused before the wire: the
    /// request would only carry a page the table cannot print.
    #[tokio::test]
    async fn a_bad_field_is_refused_before_the_wire() {
        let server = MockServer::start().await;
        let fixture = page(vec![row(1)], None);
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .respond_with(serve(&fixture))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = list_args();
        args.fields = vec!["bogus".to_string()];
        let mut out = Vec::new();
        let message = list(&client, &args, &mut out)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("bogus"), "{message}");
        let requests = server
            .received_requests()
            .await
            .expect("the mock records requests");
        assert!(
            requests.is_empty(),
            "the request never left: {}",
            requests.len()
        );
    }

    /// A blank value names nothing: refused where the message can name the
    /// flag, and no request leaves.
    #[tokio::test]
    async fn blank_flags_are_refused_before_the_wire() {
        let server = MockServer::start().await;
        let client = client(&server);
        let mut out = Vec::new();
        let mut err = Vec::new();
        for blank in ["", "   "] {
            let mut args = list_args();
            args.before = Some(blank.to_string());
            let message = list(&client, &args, &mut out)
                .await
                .unwrap_err()
                .to_string();
            assert!(message.contains("--before"), "{message}");
            let mut args = list_args();
            args.day = Some(blank.to_string());
            let message = list(&client, &args, &mut out)
                .await
                .unwrap_err()
                .to_string();
            assert!(message.contains("--day"), "{message}");
            let mut args = export_args(2);
            args.day = Some(blank.to_string());
            let message = export(&client, &args, &mut out, &mut err)
                .await
                .unwrap_err()
                .to_string();
            assert!(message.contains("--day"), "{message}");
        }
        let requests = server
            .received_requests()
            .await
            .expect("the mock records requests");
        assert!(requests.is_empty(), "no request left: {}", requests.len());
    }

    /// The window and the page size reach the wire.
    #[tokio::test]
    async fn a_page_read_carries_its_window() {
        let server = MockServer::start().await;
        let fixture = page(vec![row(1)], None);
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .and(query_param("day", "2026-10-24"))
            .and(query_param("limit", "20"))
            .respond_with(serve(&fixture))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = list_args();
        args.day = Some("2026-10-24".to_string());
        args.limit = Some(20);
        let mut out = Vec::new();
        list(&client, &args, &mut out).await.unwrap();
        let text = String::from_utf8(out).expect("utf-8 table");
        assert!(text.contains("rows=1"), "{text}");
    }

    /// A body read carries its range the same way.
    #[tokio::test]
    async fn a_body_read_carries_its_range() {
        let server = MockServer::start().await;
        let body = CallBody {
            schema_version: TURN_SCHEMA_VERSION,
            turn_id: 142,
            call_index: 1,
            call_id: "call-1".to_string(),
            name: "bash".to_string(),
            offset: 16,
            total_bytes: 96,
            text: "later".to_string(),
            truncated: false,
            next_offset: None,
            reclaimed: 0,
        };
        Mock::given(method("GET"))
            .and(path(format!("/agents/{AGENT}/turns/142/calls/1/body")))
            .and(query_param("offset", "16"))
            .and(query_param("limit", "16"))
            .respond_with(serve(&body))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = show_args(142);
        args.call = Some(1);
        args.body = true;
        args.offset = Some(16);
        args.limit = Some(16);
        let mut out = Vec::new();
        let mut err = Vec::new();
        show(&client, &args, &mut out, &mut err).await.unwrap();
        assert_eq!(String::from_utf8(out).expect("utf-8 body"), "later");
    }

    /// Both sinks write into one buffer here, so the order a terminal would see
    /// is readable: the range notice comes before the text it describes.
    struct SharedSink(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl std::io::Write for SharedSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_body_read_notices_before_the_text() {
        let server = MockServer::start().await;
        let body = CallBody {
            schema_version: TURN_SCHEMA_VERSION,
            turn_id: 142,
            call_index: 1,
            call_id: "call-1".to_string(),
            name: "bash".to_string(),
            offset: 0,
            total_bytes: 5,
            text: "later".to_string(),
            truncated: false,
            next_offset: None,
            reclaimed: 0,
        };
        Mock::given(method("GET"))
            .and(path(format!("/agents/{AGENT}/turns/142/calls/1/body")))
            .respond_with(serve(&body))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = show_args(142);
        args.call = Some(1);
        args.body = true;
        let buffer = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut out = SharedSink(buffer.clone());
        let mut err = SharedSink(buffer.clone());
        show(&client, &args, &mut out, &mut err).await.unwrap();
        let seen = String::from_utf8(buffer.borrow().clone()).expect("utf-8 seen");
        let notice = seen.find("covers bytes").expect("the notice");
        let text = seen.find("later").expect("the text");
        assert!(notice < text, "the notice comes first: {seen}");
    }

    /// A walk that stopped at the bound names where to resume from, and the
    /// second request is the one carrying the first page's cursor.
    #[tokio::test]
    async fn a_bound_stop_carries_its_resume_point() {
        let server = MockServer::start().await;
        let first = page(vec![row(2)], Some("cursor-2"));
        let second = page(vec![row(1)], Some("cursor-3"));
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .and(query_param("limit", "20"))
            .and(query_param_is_missing("before"))
            .respond_with(serve(&first))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .and(query_param("limit", "20"))
            .and(query_param("before", "cursor-2"))
            .respond_with(serve(&second))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut args = export_args(2);
        args.limit = Some(20);
        let mut out = Vec::new();
        let mut err = Vec::new();
        export(&client, &args, &mut out, &mut err).await.unwrap();
        let document: Value = serde_json::from_slice(&out).expect("json document");
        assert_eq!(document["manifest"]["window"]["next_cursor"], "cursor-3");
        assert_eq!(document["manifest"]["window"]["limit"], 20);
        let requests = server
            .received_requests()
            .await
            .expect("the mock records requests");
        let carried = requests[1]
            .url
            .query()
            .expect("the second request carries a query");
        assert!(carried.contains("before=cursor-2"), "{carried}");
    }

    /// A trailing empty page does not lose the oldest turn the walk saw, and
    /// the oldest bound is the deepest page's last row, not the first one's.
    #[tokio::test]
    async fn oldest_at_skips_a_trailing_empty_page() {
        let server = MockServer::start().await;
        let first = page((51..=60u64).rev().map(stamped).collect(), Some("cursor-2"));
        let second = page((41..=50u64).rev().map(stamped).collect(), Some("cursor-3"));
        let trailing = page(Vec::new(), None);
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .and(query_param_is_missing("before"))
            .respond_with(serve(&first))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .and(query_param("before", "cursor-2"))
            .respond_with(serve(&second))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .and(query_param("before", "cursor-3"))
            .respond_with(serve(&trailing))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut out = Vec::new();
        let mut err = Vec::new();
        export(&client, &export_args(5), &mut out, &mut err)
            .await
            .unwrap();
        let document: Value = serde_json::from_slice(&out).expect("json document");
        assert_eq!(document["manifest"]["counts"]["rows"], 20);
        assert_eq!(document["manifest"]["window"]["stopped_by"], "end");
        assert_eq!(
            document["manifest"]["window"]["newest_at"],
            "2026-10-24T10:00:00Z"
        );
        assert_eq!(
            document["manifest"]["window"]["oldest_at"],
            "2026-10-24T10:41:00Z"
        );
    }

    /// A page that answers in another view would mix two meanings in one
    /// document, so the walk stops instead of shipping it.
    #[tokio::test]
    async fn a_page_in_another_view_stops_the_walk() {
        let server = MockServer::start().await;
        let first = page(vec![stamped(2)], Some("cursor-2"));
        let mut second = page(vec![stamped(1)], None);
        second.schema_version = TURN_SCHEMA_VERSION + 1;
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .and(query_param_is_missing("before"))
            .respond_with(serve(&first))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .and(query_param("before", "cursor-2"))
            .respond_with(serve(&second))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let message = export(&client, &export_args(5), &mut out, &mut err)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            message.contains(&format!("v{}", TURN_SCHEMA_VERSION + 1)),
            "{message}"
        );
        assert!(
            message.contains(&format!("v{TURN_SCHEMA_VERSION}")),
            "{message}"
        );
        assert!(out.is_empty(), "a refused walk prints no document");
    }

    /// An exported page body is the document the tagma sent, not this build's
    /// parse of it: a field this build does not know still reaches the reader.
    #[tokio::test]
    async fn export_pages_are_the_documents_the_tagma_sent() {
        let server = MockServer::start().await;
        let mut sent_page =
            serde_json::to_value(page(vec![row(1)], None)).expect("page serializes");
        sent_page["future_page_field"] = serde_json::json!("kept");
        sent_page["rows"][0]["future_field"] = serde_json::json!(7);
        let sent = serde_json::to_string(&sent_page).expect("page serializes");
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .respond_with(ResponseTemplate::new(200).set_body_string(sent))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut out = Vec::new();
        let mut err = Vec::new();
        export(&client, &export_args(1), &mut out, &mut err)
            .await
            .unwrap();
        let document: Value = serde_json::from_slice(&out).expect("json document");
        assert_eq!(document["pages"][0]["future_page_field"], "kept");
        assert_eq!(document["pages"][0]["rows"][0]["future_field"], 7);
        assert_eq!(document["manifest"]["counts"]["rows"], 1);
    }

    /// An empty log names no turns in either bound.
    #[tokio::test]
    async fn an_empty_walk_names_no_turns() {
        let server = MockServer::start().await;
        let fixture = page(Vec::new(), None);
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .respond_with(serve(&fixture))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut out = Vec::new();
        let mut err = Vec::new();
        export(&client, &export_args(5), &mut out, &mut err)
            .await
            .unwrap();
        let document: Value = serde_json::from_slice(&out).expect("json document");
        assert_eq!(document["manifest"]["counts"]["rows"], 0);
        assert!(document["manifest"]["window"]["newest_at"].is_null());
        assert!(document["manifest"]["window"]["oldest_at"].is_null());
    }

    /// A cursor that comes back unchanged would repeat the same page forever:
    /// the walk stops and says so.
    #[tokio::test]
    async fn a_stuck_cursor_stops_the_walk() {
        let server = MockServer::start().await;
        let stuck = page(vec![row(1)], Some("same"));
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .respond_with(serve(&stuck))
            .mount(&server)
            .await;
        let client = client(&server);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let message = export(&client, &export_args(5), &mut out, &mut err)
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("cursor"), "{message}");
    }

    /// The document can go to a file instead of stdout, and the terminal still
    /// The manifest spells the stop name as JSON and the notice spells it as
    /// prose, so a test holds the two spellings together.
    #[test]
    fn a_stop_name_reads_the_same_on_both_faces() {
        for stop in [StoppedBy::End, StoppedBy::Pages] {
            assert_eq!(
                serde_json::to_string(&stop).expect("the manifest serializes the stop"),
                format!("\"{}\"", stop.as_str())
            );
        }
    }

    /// learns what was written.
    #[tokio::test]
    async fn out_writes_the_document_and_says_so() {
        let server = MockServer::start().await;
        let fixture = page(vec![row(1)], None);
        Mock::given(method("GET"))
            .and(path(turns_path()))
            .respond_with(serve(&fixture))
            .mount(&server)
            .await;
        let client = client(&server);
        let directory = tempfile::tempdir().expect("a scratch directory");
        let path = directory.path().join("turns.json");
        let mut args = export_args(5);
        args.out = Some(path.clone());
        let mut out = Vec::new();
        let mut err = Vec::new();
        export(&client, &args, &mut out, &mut err).await.unwrap();
        assert!(out.is_empty(), "the document went to the file");
        let written: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("the file exists")).expect("json");
        assert_eq!(written["manifest"]["counts"]["rows"], 1);
        let notice = String::from_utf8(err).expect("utf-8 notice");
        assert!(notice.contains("wrote 1 page(s) / 1 row(s)"), "{notice}");
        assert!(notice.contains("(stopped by end)"), "{notice}");
    }
}
