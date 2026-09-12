//! kannaka-tui — Rich terminal dashboard for the Kannaka constellation.
//!
//! A full-screen TUI built on ratatui + crossterm that shells out to the
//! `kannaka` CLI binary for all memory operations, status polling, and
//! dream control.  This binary is a pure FRONTEND — it never links
//! against kannaka-memory as a library.

use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        canvas::{Canvas, Circle as CanvasCircle, Line as CanvasLine},
        Block, Borders, Gauge, List, ListItem, Paragraph, Tabs, Wrap,
    },
    Frame, Terminal,
};
use std::collections::VecDeque;
use std::io;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Translate a CHAR index into a BYTE offset within `s`. The input box
/// tracks the cursor as a char index (one step per keystroke), but
/// `String::insert`/`remove` take byte offsets — calling them with a
/// char index panics the moment the buffer holds multi-byte UTF-8.
/// A char index at or past the end maps to `s.len()` (the tail).
fn byte_offset(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(b, _)| b)
}

/// Run a `kannaka` subprocess to completion with a hard wall-clock
/// timeout, killing it if it overruns. Returns the captured `Output` or
/// an error string. Every shellout that runs inside a worker thread goes
/// through here so a wedged child can never strand its worker (and thus
/// the in-flight guard that gates the next refresh) forever. The
/// `KANNAKA_QUIET=1` env keeps the child from polluting stdout with
/// progress chatter we'd otherwise mis-parse.
///
/// NOTE: like the original `execute_passthrough` loop this drains stdout
/// only after the child exits, so a child that emits more than a pipe
/// buffer (~64 KiB) without exiting could deadlock — bounded now by the
/// timeout, which kills it. Callers must therefore only pass commands
/// with small, bounded output (status/recall/clusters), never `export`.
fn run_capture(
    bin: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let mut child = Command::new(bin)
        .args(args)
        .env("KANNAKA_QUIET", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed at '{bin}': {e}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => return child.wait_with_output().map_err(|e| e.to_string()),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timeout after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    }
}

/// One constellation app as reported by `kannaka constellation`. The CLI
/// prints human-readable lines (`✓ Name   URL` / `✗ Name   URL`) rather
/// than JSON, so the Cosmos tab parses them by the leading status glyph.
#[derive(Clone, PartialEq, Eq)]
struct CosmosApp {
    name: String,
    up: bool,
    url: String,
}

/// Parse one line of `kannaka constellation` output into a `CosmosApp`.
/// Returns `None` for the header / divider / blank lines (anything not
/// led by a ✓ or ✗ glyph). The URL is detected by its `://` scheme so a
/// multi-word app name with variable padding still splits correctly.
fn parse_constellation_line(line: &str) -> Option<CosmosApp> {
    let t = line.trim();
    let (up, rest) = if let Some(r) = t.strip_prefix('\u{2713}') {
        (true, r) // ✓
    } else {
        // Not a ✓ line, so it must be a ✗ line — anything else (header,
        // divider, blank) is not an app row and yields None.
        (false, t.strip_prefix('\u{2717}')?) // ✗
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let (name, url) = match rest.rfind("://") {
        Some(pos) => {
            // Walk back from the scheme to the whitespace that starts the
            // URL token; everything before it is the (possibly multi-word)
            // app name. Advance past the FULL width of that whitespace char
            // (`i + c.len_utf8()`, not `i + 1`) so a multibyte separator
            // (NBSP, ideographic space) can't land `url_start` inside a
            // code point and panic the byte slices below.
            let url_start = rest[..pos]
                .char_indices()
                .rev()
                .find(|(_, c)| c.is_whitespace())
                .map_or(0, |(i, c)| i + c.len_utf8());
            (
                rest[..url_start].trim().to_string(),
                rest[url_start..].trim().to_string(),
            )
        }
        None => (rest.to_string(), String::new()),
    };
    Some(CosmosApp { name, up, url })
}

/// Trim `kannaka radio` stdout to its non-empty display lines (now
/// playing / station / listeners). Rendered as-is on the Cosmos tab.
fn clean_radio_lines(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .take(4)
        .collect()
}

// ---------------------------------------------------------------------------
// Colour palette — the Kannaka brand
// ---------------------------------------------------------------------------

const BG: Color = Color::Rgb(10, 10, 26);
const ACCENT: Color = Color::Rgb(123, 104, 238); // purple
const SUCCESS: Color = Color::Rgb(74, 222, 128);
const ERROR: Color = Color::Rgb(248, 113, 113);
const WARNING: Color = Color::Rgb(251, 191, 36);
const INFO: Color = Color::Rgb(0, 229, 255);
const TEXT: Color = Color::Rgb(224, 224, 224);
const DIM: Color = Color::Rgb(102, 102, 102);

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Message {
    role: Role,
    content: String,
}

/// Result of an off-thread remember/recall/forget command. The worker
/// builds the message lines to append and flags whether the memory state
/// changed (so the event loop refreshes the observe view).
struct CmdResult {
    messages: Vec<Message>,
    refresh_observe: bool,
}

#[derive(Clone)]
enum Role {
    User,
    System,
    Result,
    Error,
}

#[derive(Clone)]
struct MemoryEntry {
    content: String,
    amplitude: f32,
}

#[derive(Clone, Default)]
struct Status {
    phi: f32,
    xi: f32,
    order: f32,
    memories: u64,
    clusters: u64,
    links: u64,
    level: String,
    active: u64,
}

// Type aliases for the async-poll channels — clippy::type_complexity
// flags the Receiver<Result<(...), String>> pile-up otherwise.
type StatusRx = mpsc::Receiver<Result<Status, String>>;
type ObserveRx = mpsc::Receiver<Result<(u64, Vec<MemoryEntry>), String>>;
/// Cosmos poll result: (constellation apps, radio now-playing lines).
type CosmosRx = mpsc::Receiver<Result<(Vec<CosmosApp>, Vec<String>), String>>;

// ---------------------------------------------------------------------------
// Agent harness — the coding-agent surface. The TUI drives a long-running
// `kannaka agent --json` child, renders its agentic transcript, and gates
// filesystem/shell mutations behind a human approval dialog.
// ---------------------------------------------------------------------------

/// One event parsed from the `kannaka agent --json` NDJSON stream.
#[derive(Clone)]
enum AgentEvent {
    Ready {
        model: String,
        mode: String,
        cwd: String,
    },
    Text(String),
    ToolUse {
        id: String,
        name: String,
        summary: String,
        read_only: bool,
        danger: bool,
    },
    ApprovalRequired {
        id: String,
        name: String,
        summary: String,
        danger: bool,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    Usage {
        input: u64,
        output: u64,
    },
    Iteration(usize),
    Done(String),
    Error(String),
    Mode(String),
    /// The child's stdout closed — it exited or the pipe broke.
    Closed(String),
}

/// One rendered row of the harness transcript.
#[derive(Clone)]
enum AgentLine {
    User(String),
    Assistant(String),
    /// A tool call. `result` is filled in when the matching `tool_result`
    /// arrives (matched by `id`); `awaiting` flips false at that point.
    Tool {
        id: String,
        name: String,
        summary: String,
        danger: bool,
        result: Option<String>,
        is_error: bool,
        awaiting: bool,
    },
    Notice(String),
}

/// An in-flight approval request the human must resolve (a/s/d).
#[derive(Clone)]
struct PendingApproval {
    id: String,
    name: String,
    summary: String,
    danger: bool,
}

#[derive(Clone, PartialEq, Eq)]
enum HarnessStatus {
    Off,
    Starting,
    Ready,
    Thinking,
    AwaitingApproval,
    Closed,
}

/// Parse one NDJSON line from `kannaka agent --json` into an `AgentEvent`.
/// Returns `None` for unknown/malformed frames.
fn parse_agent_event(v: &serde_json::Value) -> Option<AgentEvent> {
    let kind = v.get("kind")?.as_str()?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let u = |k: &str| v.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
    let b = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    Some(match kind {
        "ready" => AgentEvent::Ready {
            model: s("model"),
            mode: s("mode"),
            cwd: s("cwd"),
        },
        "text" => AgentEvent::Text(s("text")),
        "tool_use" => AgentEvent::ToolUse {
            id: s("id"),
            name: s("name"),
            summary: tool_input_summary(
                &s("name"),
                v.get("input").unwrap_or(&serde_json::Value::Null),
            ),
            read_only: b("read_only"),
            danger: b("danger"),
        },
        "approval_required" => AgentEvent::ApprovalRequired {
            id: s("id"),
            name: s("name"),
            summary: s("summary"),
            danger: b("danger"),
        },
        "tool_result" => AgentEvent::ToolResult {
            id: s("id"),
            content: s("content"),
            is_error: b("is_error"),
        },
        "usage" => AgentEvent::Usage {
            input: u("input"),
            output: u("output"),
        },
        "iteration" => AgentEvent::Iteration(u("n") as usize),
        "done" => AgentEvent::Done(s("reason")),
        "error" => AgentEvent::Error(s("text")),
        "mode" => AgentEvent::Mode(s("mode")),
        _ => return None,
    })
}

/// One-line human summary of a tool call's input, for the transcript.
fn tool_input_summary(name: &str, input: &serde_json::Value) -> String {
    let get = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "bash" => get("command").to_string(),
        "read_file" | "write_file" | "edit_file" => get("file_path").to_string(),
        "glob" | "grep" => get("pattern").to_string(),
        "list_dir" => get("path").to_string(),
        "recall" => get("query").to_string(),
        "remember" => truncate(get("content"), 60),
        _ => truncate(&serde_json::to_string(input).unwrap_or_default(), 80),
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct App {
    active_tab: usize,
    tabs: Vec<&'static str>,
    input: String,
    cursor_pos: usize,
    messages: Vec<Message>,
    memories: Vec<MemoryEntry>,
    status: Option<Status>,
    agent_name: String,
    should_quit: bool,
    /// When the last paste burst landed. Used to absorb an Enter that arrives
    /// right after a paste (a multi-line paste's line break, not a submit).
    last_paste_at: Option<Instant>,
    scroll_offset: usize,
    last_status_poll: Instant,
    show_help: bool,
    history: Vec<String>,
    history_idx: Option<usize>,
    kannaka_bin: String,
    // Chat tab — persistent conversation with the agent. Each turn shells
    // out to `kannaka ask --session kannaka-tui` in a background thread so
    // the UI doesn't block during the API round-trip.
    chat_messages: Vec<ChatLine>,
    chat_pending: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    chat_tick: usize,
    // Async status/observe loading — set when a background thread is
    // working on a fresh poll, drained by the event loop. Without this
    // the initial `App::new()` would block ~30s on the first
    // `kannaka status` (eigendecomp on ~600 memories) and the TUI
    // looked like it never started.
    status_pending: Option<StatusRx>,
    observe_pending: Option<ObserveRx>,
    // Persistent `kannaka chat --json` child process — HRM loads once
    // at first chat turn, every subsequent turn reuses the loaded
    // medium for ~3-5s per turn instead of 30s per `kannaka ask`.
    chat_child: Option<ChatChildHandle>,
    chat_child_rx: Option<std::sync::mpsc::Receiver<ChatChildEvent>>,
    chat_pending_msg: Option<String>,
    // Live Bus tab — long-running `kannaka swarm tail` child whose
    // stdout we read as NDJSON. The reader thread pushes BusLine
    // entries through `bus_rx`; we drain them each tick and cap
    // `bus_lines` at BUS_BACKLOG_CAP entries.
    bus_lines: VecDeque<BusLine>,
    bus_rx: Option<mpsc::Receiver<BusLine>>,
    bus_status: BusStatus,
    bus_child: Option<std::process::Child>,
    // Constellation tab state — keyed by agent_id, populated by the
    // same bus reader thread that feeds bus_lines.
    agents: std::collections::HashMap<String, AgentSnapshot>,
    agent_rx: Option<mpsc::Receiver<AgentSnapshot>>,
    // Dreams tab — rolling history of KANNAKA.dreams events harvested
    // from the same bus stream, plus the local trigger state machine.
    dream_history: VecDeque<DreamEvent>,
    dream_rx: Option<mpsc::Receiver<DreamEvent>>,
    dream_run: DreamRunState,
    dream_trigger_rx: Option<mpsc::Receiver<Result<String, String>>>,
    /// Streaming stdout of a one-shot plugin invocation (`/code`,
    /// `/topus`). poll_plugin drains lines into chat_messages each
    /// tick; channel close → clear chat_pending so the spinner stops.
    plugin_output_rx: Option<mpsc::Receiver<String>>,
    /// In-flight remember/recall/forget worker result. These commands
    /// used to call `Command::output()` on the UI thread and froze the
    /// render loop for seconds; now they run on a worker and report back
    /// here, drained each tick by `poll_cmd`.
    cmd_pending: Option<mpsc::Receiver<CmdResult>>,
    /// Process handles for the children whose `Child` lives inside a
    /// worker thread (chat REPL, one-shot plugin). The worker stashes the
    /// `Child` here right after spawn so the teardown in `main` can
    /// kill()/wait() them — otherwise they outlive the TUI. The bus child
    /// is reaped directly via `bus_child`.
    chat_child_proc: ChildHandle,
    plugin_child_proc: ChildHandle,
    /// In-flight result of a one-shot passthrough command (ask, hear,
    /// search, assess, voice, swarm, relate, …). Was a blocking
    /// `thread::spawn(...).join()` on the UI thread that froze the entire
    /// TUI for up to the command's timeout; now the worker reports back
    /// here and `poll_passthrough` drains it each tick.
    passthrough_pending: Option<mpsc::Receiver<CmdResult>>,
    // ---- Cosmos tab — constellation-wide health (kannaka constellation +
    // kannaka radio). One-shot polled view; no long-running child.
    cosmos_apps: Vec<CosmosApp>,
    cosmos_radio: Vec<String>,
    cosmos_pending: Option<CosmosRx>,
    cosmos_error: Option<String>,
    cosmos_last_load: Instant,
    // ---- Agent harness tab — long-running `kannaka agent --json` child.
    // The child is owned here (reaped on quit); stdin is held to send
    // user/approval/mode frames; a reader thread parses stdout NDJSON into
    // AgentEvents drained by `poll_harness`.
    harness_child: Option<std::process::Child>,
    harness_stdin: Option<std::process::ChildStdin>,
    harness_rx: Option<mpsc::Receiver<AgentEvent>>,
    harness_lines: Vec<AgentLine>,
    harness_status: HarnessStatus,
    harness_mode: String,
    harness_model: String,
    harness_cwd: String,
    harness_pending: Option<PendingApproval>,
    harness_usage_in: u64,
    harness_usage_out: u64,
    harness_iter: usize,
    harness_tick: usize,
}

/// Shared slot holding a spawned `Child` so the main thread can reap a
/// process whose `Child` is otherwise owned by a worker thread.
type ChildHandle = std::sync::Arc<std::sync::Mutex<Option<std::process::Child>>>;

/// Kill + wait the child in a shared handle, if present. Used on quit so
/// no background `kannaka` process leaks past the TUI.
fn reap_handle(h: &ChildHandle) {
    if let Ok(mut guard) = h.lock() {
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

const BUS_BACKLOG_CAP: usize = 500;
const DREAM_HISTORY_CAP: usize = 30;
/// Cap on the Agent-harness transcript so a long session can't grow it
/// without bound — same discipline as every other buffer here.
const HARNESS_LINES_CAP: usize = 2000;
/// Cap on the Memory-tab message log. Like `bus_lines`/`dream_history`,
/// `messages` otherwise grows unbounded across a long session; trim the
/// oldest entries past this many.
const MESSAGES_CAP: usize = 1000;
/// Cap on the command-bar input history. `bus_lines`, `dream_history`, and
/// `messages` are all bounded; `history` was the only unbounded collection.
/// 500 entries covers any realistic session without truncating useful recall.
const HISTORY_CAP: usize = 500;
/// Cap on the Chat-tab conversation log. Without this `chat_messages` grows
/// without bound while every other collection in App is already capped.
const CHAT_MESSAGES_CAP: usize = 500;
/// Agents not heard from in this window get rendered as ghost outlines
/// instead of solid markers on the Constellation tab.
const AGENT_FRESH_WINDOW: Duration = Duration::from_secs(120);
/// Agents silent past this (10× the fresh window) are dropped from the
/// map entirely. Without this the `agents` HashMap grows unbounded across
/// a long session as constellation membership churns — every other buffer
/// in this file is capped, so this one is too.
const AGENT_EVICT_WINDOW: Duration = Duration::from_secs(1200);
/// Re-poll the Cosmos tab (constellation + radio health) no more often
/// than this when the tab is focused.
const COSMOS_POLL_INTERVAL: Duration = Duration::from_secs(20);

/// Canned task injected by `/qos`: provision (or reuse) a qBraid Lab
/// instance, boot QuantumOS in QEMU on it — networked (rtl8139 on SLIRP,
/// so the full net stack runs and the shell's http/nslookup/udping work)
/// and quiet (clean interactive console) — and open a local spectator
/// window on the serial console. The agent drives the whole flow with
/// its lab_* tools; paid provisioning still goes through the normal
/// spend approval — this string never bypasses a gate.
const QOS_BOOT_PROMPT: &str = "Boot QuantumOS on a qBraid Lab instance and open a watch window for me. \
Steps: \
1) lab_list_instances — if an instance is already running, reuse it; otherwise pick a cheap CPU profile \
via lab_list_profiles and lab_provision_instance (wait for it to be ready). \
2) lab_ssh_configure on that instance to get its ssh alias. \
3) lab_qos_boot with that ssh alias, network=true, quiet=true, and qseed='reservoir' (it installs qemu/build \
deps, clones and builds QuantumOS, and boots it in a detached tmux session with an rtl8139 NIC on user-mode \
networking AND a quiet kernel console — so the boot self-test runs the full network stack (ARP/DHCP/ICMP/DNS) \
and the ring-3 shell's nslookup/udping/http work against the real internet, while the demo kernel's \
steady-state chatter is silenced for a clean interactive qsh prompt; the quantum PRNG is seeded with real QPU \
bits from the local entropy reservoir) — report the boot tail, especially the 'QuantumOS ready' line and the \
'NET: DHCP lease 10.0.2.15' line, and whether qseed_confirmed shows the kernel echoed the seed back; include \
the qseed provenance job id. If the reservoir is empty, retry lab_qos_boot with network=true and quiet=true \
and no qseed, and say so plainly. \
4) lab_watch with the same alias and session so a local terminal window opens showing the live serial console \
— a clean qsh prompt where I can type 'http example.com', 'nslookup', 'udping', etc. \
When done, remind me the instance keeps billing until lab_stop_instance.";

/// Canned task injected by `/qos --graphical`: same flow as QOS_BOOT_PROMPT but
/// boots with a real VGA framebuffer over noVNC and opens a BROWSER watch
/// window, so the wave-interference boot splash can be watched live. Paid
/// provisioning still goes through the normal spend approval.
const QOS_BOOT_GRAPHICAL_PROMPT: &str = "Boot QuantumOS on a qBraid Lab instance with a GRAPHICAL display and open a browser watch window for me. \
Steps: \
1) lab_list_instances — if an instance is already running, reuse it; otherwise pick a cheap CPU profile via lab_list_profiles and lab_provision_instance (wait for it to be ready). \
2) lab_ssh_configure on that instance to get its ssh alias. \
3) lab_qos_boot with that ssh alias, graphical=true, network=true, and qseed='reservoir' (it installs qemu/build deps AND noVNC, clones and builds QuantumOS, and boots it PAUSED with a real VGA framebuffer over VNC AND an rtl8139 NIC on user-mode networking, so the boot self-test runs the full network stack — ARP/DHCP/ICMP/DNS against the real internet — as I watch, seeding the kernel's quantum PRNG with real QPU bits) — report the returned web_port and monitor_port, the qseed provenance job id, and whether qseed_confirmed is true. If the reservoir is empty, retry lab_qos_boot with graphical=true and network=true and no qseed, and say so plainly. \
4) lab_qos_watch with the same alias, passing the web_port and monitor_port from step 3 — it opens an SSH tunnel, launches my browser at the noVNC client, and resumes the paused VM so I watch the wave-interference boot splash animate live in my browser from the first frame. \
5) lab_qos_swarm_bridge with the same ssh alias, session, and qseed — QuantumOS's ring-3 swarm service emits a Lamport-signed boot attestation on COM2 as it boots; this tails that COM2 log, verifies the attestation, and (only if it verifies) joins the NATS swarm under QuantumOS's OWN signed identity, refusing to join on a bad attestation. Report the joined agent-id and that I can confirm the node is live with `kannaka swarm peers`; note the join runs as a background daemon (join_pid) that holds the node's presence on the mesh until killed. \
When done, remind me the instance keeps billing until lab_stop_instance (or lab_terminate_instance for full teardown), and that killing join_pid (or kannaka swarm leave) removes QuantumOS from the swarm.";

/// Handle to the spawned `kannaka chat --json` child. Stdin is held here
/// so the main thread can write user turns into it; stdout/stderr are
/// owned by reader threads inside the spawn helper and dispatch events
/// back via `chat_child_rx`.
struct ChatChildHandle {
    stdin: Option<std::process::ChildStdin>,
    ready: bool,
}

/// Events streamed from the chat-child worker threads back to the TUI.
enum ChatChildEvent {
    /// First event after spawn — hands stdin over for turn-sending.
    Stdin(std::process::ChildStdin),
    /// Child printed its `{"kind":"ready"}` line on stderr — HRM loaded.
    Ready,
    /// One NDJSON line from stdout: a response (chat / slash / error).
    Response { kind: String, text: String },
    /// Child exited or pipe broke. Next turn will re-spawn.
    Closed(String),
}

#[derive(Clone)]
struct ChatLine {
    who: ChatWho,
    text: String,
}

#[derive(Clone, PartialEq, Eq)]
enum ChatWho {
    User,
    Kannaka,
    System,
}

/// One row in the live Bus tab — produced by parsing NDJSON lines emitted
/// by the `kannaka swarm tail` child process.
#[derive(Clone)]
struct BusLine {
    ts_ms: i64,
    subject: String,
    summary: String,
}

#[derive(Clone, PartialEq, Eq)]
enum BusStatus {
    Off,
    Connecting,
    Streaming,
    Failed,
}

/// One KANNAKA.dreams report observed on the bus. The Dreams tab keeps
/// a rolling backlog of these so users can see what consolidation
/// activity has been happening across the constellation.
#[derive(Clone)]
struct DreamEvent {
    ts_ms: i64,
    agent_id: String,
    cycles: u64,
    strengthened: u64,
    pruned: u64,
    new_connections: u64,
    hallucinations: u64,
    consciousness_before: f32,
    consciousness_after: f32,
    emerged: bool,
}

/// Status of the most recent locally-triggered dream cycle. The TUI
/// dispatches `kannaka dream` in a worker thread so the event loop
/// stays responsive during the 30+ second consolidation pass.
#[derive(Clone)]
enum DreamRunState {
    Idle,
    Running {
        mode: String,
        started: Instant,
    },
    Done {
        mode: String,
        took: Duration,
        summary: String,
    },
    Failed {
        mode: String,
        error: String,
    },
}

/// Latest snapshot for one agent — harvested from `QUEEN.phase.<agent_id>`
/// payloads as they stream through the bus. Used by the Constellation tab
/// to plot agents on the unit circle and fade out anyone who's gone quiet.
#[derive(Clone)]
struct AgentSnapshot {
    agent_id: String,
    theta: f32,
    phi: f32,
    coherence: f32,
    order_parameter: f32,
    handedness: String,
    memory_count: u64,
    last_seen: Instant,
}

impl App {
    fn new() -> Self {
        // Find the kannaka binary — prefer the release build next to us
        let kannaka_bin = Self::find_kannaka_binary();
        let agent_name = Self::load_agent_name();

        Self {
            // Chat is the primary surface. The other tabs are still
            // reachable via Tab/Shift+Tab but the user lands in chat.
            // Bus sits between Status and Constellation as the live
            // constellation pulse view.
            // Land on the Agent harness — the primary surface. Every other
            // tab is still reachable via Tab/Shift+Tab.
            active_tab: 7,
            tabs: vec![
                "Memory",
                "Status",
                "Bus",
                "Constellation",
                "Dreams",
                "Chat",
                "Cosmos",
                "Agent",
            ],
            input: String::new(),
            cursor_pos: 0,
            messages: vec![Message {
                role: Role::System,
                content: format!(
                    "Welcome to Kannaka TUI. Agent: {agent_name}. Type a command or press F1 for help."
                ),
            }],
            memories: Vec::new(),
            status: None,
            agent_name,
            should_quit: false,
            last_paste_at: None,
            scroll_offset: 0,
            last_status_poll: Instant::now() - Duration::from_secs(60), // force initial poll
            show_help: false,
            history: Vec::new(),
            history_idx: None,
            kannaka_bin,
            chat_messages: vec![ChatLine {
                who: ChatWho::System,
                text: "Chat with Kannaka. Memories surface via wave resonance each turn. Enter to send.".into(),
            }],
            chat_pending: None,
            chat_tick: 0,
            status_pending: None,
            observe_pending: None,
            chat_child: None,
            chat_child_rx: None,
            chat_pending_msg: None,
            bus_lines: VecDeque::new(),
            bus_rx: None,
            bus_status: BusStatus::Off,
            bus_child: None,
            agents: std::collections::HashMap::new(),
            agent_rx: None,
            dream_history: VecDeque::new(),
            dream_rx: None,
            dream_run: DreamRunState::Idle,
            dream_trigger_rx: None,
            plugin_output_rx: None,
            cmd_pending: None,
            chat_child_proc: std::sync::Arc::new(std::sync::Mutex::new(None)),
            plugin_child_proc: std::sync::Arc::new(std::sync::Mutex::new(None)),
            passthrough_pending: None,
            cosmos_apps: Vec::new(),
            cosmos_radio: Vec::new(),
            cosmos_pending: None,
            cosmos_error: None,
            // Force an initial load the first time the Cosmos tab opens.
            cosmos_last_load: Instant::now() - COSMOS_POLL_INTERVAL,
            harness_child: None,
            harness_stdin: None,
            harness_rx: None,
            harness_lines: vec![AgentLine::Notice(
                "Kannaka Agent — a coding agent over the constellation. Type a task and \
                 press Enter to start. Filesystem/shell changes ask for your approval."
                    .into(),
            )],
            harness_status: HarnessStatus::Off,
            harness_mode: "default".into(),
            harness_model: String::new(),
            harness_cwd: String::new(),
            harness_pending: None,
            harness_usage_in: 0,
            harness_usage_out: 0,
            harness_iter: 0,
            harness_tick: 0,
        }
    }

    /// Spawn the long-running `kannaka agent --json` child and a reader
    /// thread that parses its NDJSON stdout into `AgentEvent`s. Lazy — only
    /// started when the user first sends a task (or after a restart).
    fn start_harness(&mut self) {
        if self.harness_child.is_some() {
            return;
        }
        let cwd = std::env::current_dir()
            .map_or_else(|_| ".".into(), |p| p.to_string_lossy().into_owned());
        self.harness_cwd.clone_from(&cwd);
        self.harness_status = HarnessStatus::Starting;
        let mode = self.harness_mode.clone();
        let model = self.harness_model.clone();

        let (tx, rx) = mpsc::channel::<AgentEvent>();
        self.harness_rx = Some(rx);

        let mut cmd = Command::new(&self.kannaka_bin);
        cmd.args(["agent", "--json", "--cwd", &cwd, "--mode", &mode]);
        if !model.is_empty() {
            cmd.args(["--model", &model]);
        }
        cmd.env("KANNAKA_QUIET", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.push_harness(AgentLine::Notice(format!(
                    "[could not start 'kannaka agent': {e} — is the kannaka binary on PATH?]"
                )));
                self.harness_status = HarnessStatus::Closed;
                self.harness_rx = None;
                return;
            }
        };
        self.harness_stdin = child.stdin.take();
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                self.harness_status = HarnessStatus::Closed;
                return;
            }
        };
        self.harness_child = Some(child);

        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    if let Some(ev) = parse_agent_event(&v) {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(AgentEvent::Closed("agent stdout closed".into()));
        });
    }

    /// Kill the running agent child (gracefully if possible) and reset
    /// harness state. Used by `/clear` and `/model`.
    fn restart_harness(&mut self) {
        if let Some(mut child) = self.harness_child.take() {
            if let Some(mut stdin) = self.harness_stdin.take() {
                use std::io::Write;
                let _ = writeln!(stdin, "{}", serde_json::json!({ "type": "exit" }));
                let _ = stdin.flush();
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        self.harness_stdin = None;
        self.harness_rx = None;
        self.harness_pending = None;
        self.harness_status = HarnessStatus::Off;
    }

    /// Interrupt an in-flight turn. The NDJSON protocol has no mid-LLM-call
    /// cancel (the backend is blocked in the request), so the only reliable
    /// stop is to kill the child; the next task starts a fresh agent (the
    /// HRM reloads in ~3s). Bound to Esc and `/stop` on the Agent tab.
    fn stop_harness(&mut self) {
        let was_busy = self.harness_child.is_some();
        self.restart_harness();
        if was_busy {
            self.push_harness(AgentLine::Notice(
                "[stopped — turn cancelled. Type a new task to start again.]".into(),
            ));
        }
    }

    /// Write one JSON frame (+newline) to the agent child's stdin.
    fn send_harness_frame(&mut self, frame: serde_json::Value) {
        if let Some(stdin) = &mut self.harness_stdin {
            use std::io::Write;
            let _ = writeln!(stdin, "{frame}");
            let _ = stdin.flush();
        }
    }

    /// Append to the transcript, trimming the oldest entries past the cap.
    fn push_harness(&mut self, line: AgentLine) {
        self.harness_lines.push(line);
        if self.harness_lines.len() > HARNESS_LINES_CAP {
            let overflow = self.harness_lines.len() - HARNESS_LINES_CAP;
            self.harness_lines.drain(0..overflow);
        }
    }

    /// Begin (or continue) a turn: ensure the child is up, echo the user
    /// line, and send the user frame. The frame buffers in the pipe until
    /// the backend finishes loading the HRM and starts reading stdin.
    fn harness_user_turn(&mut self, text: String) {
        if self.harness_child.is_none()
            || matches!(
                self.harness_status,
                HarnessStatus::Closed | HarnessStatus::Off
            )
        {
            self.start_harness();
        }
        self.push_harness(AgentLine::User(text.clone()));
        self.harness_status = HarnessStatus::Thinking;
        self.scroll_offset = 0;
        self.send_harness_frame(serde_json::json!({ "type": "user", "text": text }));
    }

    /// Resolve the active approval request (allow / allow_always / deny).
    fn resolve_approval(&mut self, decision: &str) {
        if let Some(p) = self.harness_pending.take() {
            self.send_harness_frame(serde_json::json!({
                "type": "approval", "id": p.id, "decision": decision
            }));
            let verb = match decision {
                "allow" => "allowed",
                "allow_always" => "allowed (always)",
                _ => "denied",
            };
            self.push_harness(AgentLine::Notice(format!(
                "[{verb}: {} {}]",
                p.name, p.summary
            )));
            self.harness_status = HarnessStatus::Thinking;
        }
    }

    /// Set the permission mode, forwarding to the running child if any.
    fn set_harness_mode(&mut self, mode: &str) {
        self.harness_mode = mode.to_string();
        if self.harness_child.is_some() {
            self.send_harness_frame(serde_json::json!({ "type": "mode", "mode": mode }));
        } else {
            self.push_harness(AgentLine::Notice(format!(
                "[mode set to {mode} — applies when the agent starts]"
            )));
        }
    }

    /// Drain agent events from the reader thread. Non-blocking; called each
    /// tick. Events are collected first to avoid holding the `harness_rx`
    /// borrow across the `&mut self` apply calls.
    fn poll_harness(&mut self) {
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &self.harness_rx {
            loop {
                match rx.try_recv() {
                    Ok(ev) => events.push(ev),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for ev in events {
            self.apply_agent_event(ev);
        }
        if disconnected {
            self.harness_rx = None;
        }
    }

    fn apply_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Ready { model, mode, cwd } => {
                self.harness_model = model;
                self.harness_mode = mode;
                self.harness_cwd = cwd;
                self.harness_status = HarnessStatus::Ready;
                self.push_harness(AgentLine::Notice(format!(
                    "agent ready · {} · {}",
                    self.harness_model, self.harness_cwd
                )));
            }
            AgentEvent::Text(t) => {
                if !t.trim().is_empty() {
                    self.push_harness(AgentLine::Assistant(t));
                }
                // Don't yank to the bottom on every streamed line — when offset
                // is 0 the view already auto-follows; when the user has scrolled
                // up to read, leave their position alone.
            }
            AgentEvent::ToolUse {
                id,
                name,
                summary,
                read_only,
                danger,
            } => {
                self.push_harness(AgentLine::Tool {
                    id,
                    name,
                    summary,
                    danger,
                    result: None,
                    is_error: false,
                    awaiting: !read_only,
                });
                // Preserve the user's scroll position (see AgentEvent::Text).
            }
            AgentEvent::ApprovalRequired {
                id,
                name,
                summary,
                danger,
            } => {
                self.harness_pending = Some(PendingApproval {
                    id,
                    name,
                    summary,
                    danger,
                });
                self.harness_status = HarnessStatus::AwaitingApproval;
            }
            AgentEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                for line in self.harness_lines.iter_mut().rev() {
                    if let AgentLine::Tool {
                        id: lid,
                        result,
                        is_error: le,
                        awaiting,
                        ..
                    } = line
                    {
                        if *lid == id {
                            *result = Some(content);
                            *le = is_error;
                            *awaiting = false;
                            break;
                        }
                    }
                }
            }
            AgentEvent::Usage { input, output } => {
                self.harness_usage_in += input;
                self.harness_usage_out += output;
            }
            AgentEvent::Iteration(n) => {
                self.harness_iter = n;
                if self.harness_pending.is_none() {
                    self.harness_status = HarnessStatus::Thinking;
                }
            }
            AgentEvent::Done(reason) => {
                self.harness_status = HarnessStatus::Ready;
                if reason != "end_turn" {
                    self.push_harness(AgentLine::Notice(format!("[turn ended: {reason}]")));
                }
            }
            AgentEvent::Error(t) => {
                self.push_harness(AgentLine::Notice(format!("[error: {t}]")));
            }
            AgentEvent::Mode(m) => {
                self.harness_mode.clone_from(&m);
                self.push_harness(AgentLine::Notice(format!("[mode → {m}]")));
            }
            AgentEvent::Closed(reason) => {
                self.harness_status = HarnessStatus::Closed;
                self.harness_pending = None;
                self.harness_stdin = None;
                self.harness_child = None;
                self.push_harness(AgentLine::Notice(format!(
                    "[agent closed: {reason} — type a task to restart]"
                )));
            }
        }
    }

    /// Spawn `kannaka swarm tail` and stream its NDJSON stdout into the
    /// Bus tab. Lazy — only kicked off the first time the user opens the
    /// Bus tab so the TUI doesn't open a NATS connection on launch for
    /// users who don't care.
    fn start_bus(&mut self) {
        if self.bus_child.is_some() {
            return;
        }
        self.bus_status = BusStatus::Connecting;
        let (bus_tx, bus_rx) = mpsc::channel::<BusLine>();
        let (agent_tx, agent_rx) = mpsc::channel::<AgentSnapshot>();
        let (dream_tx, dream_rx) = mpsc::channel::<DreamEvent>();
        self.bus_rx = Some(bus_rx);
        self.agent_rx = Some(agent_rx);
        self.dream_rx = Some(dream_rx);

        let mut cmd = Command::new(&self.kannaka_bin);
        cmd.args(["swarm", "tail"])
            .env("KANNAKA_QUIET", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.bus_lines.push_back(BusLine {
                    ts_ms: chrono::Utc::now().timestamp_millis(),
                    subject: "tui.error".into(),
                    summary: format!("could not spawn 'kannaka swarm tail': {e}"),
                });
                self.bus_status = BusStatus::Failed;
                return;
            }
        };
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                self.bus_status = BusStatus::Failed;
                return;
            }
        };
        self.bus_child = Some(child);

        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                    continue;
                };
                let ts_ms = val
                    .get("ts")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                let subject = val
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let payload = val
                    .get("payload")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                // Phase frames feed the Constellation tab.
                if subject.starts_with("QUEEN.phase.") {
                    if let Some(snap) = agent_snapshot_from_payload(&subject, &payload) {
                        // Send-fails are non-fatal — the main thread may have
                        // dropped agent_rx but bus_tx keeps the stream alive.
                        let _ = agent_tx.send(snap);
                    }
                }
                // Dream completion reports feed the Dreams tab.
                if subject == "KANNAKA.dreams" {
                    if let Some(ev) = dream_event_from_payload(ts_ms, &payload) {
                        let _ = dream_tx.send(ev);
                    }
                }

                let summary = summarize_payload(&subject, &payload);
                if bus_tx
                    .send(BusLine {
                        ts_ms,
                        subject,
                        summary,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    /// Tear down a dead/failed bus stream and start a fresh one. Bound to
    /// `r` on the Bus tab when the stream has Failed — a manual recovery
    /// path so a transient NATS blip doesn't require restarting the TUI.
    fn restart_bus(&mut self) {
        if let Some(mut child) = self.bus_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.bus_rx = None;
        self.agent_rx = None;
        self.dream_rx = None;
        self.bus_status = BusStatus::Off;
        self.start_bus();
    }

    /// Drain any new BusLine entries from the worker thread into the
    /// ring buffer. Capped at BUS_BACKLOG_CAP — older lines drop off
    /// the front.
    fn poll_bus(&mut self) {
        let mut got_any = false;
        if let Some(rx) = &self.bus_rx {
            loop {
                match rx.try_recv() {
                    Ok(line) => {
                        self.bus_lines.push_back(line);
                        while self.bus_lines.len() > BUS_BACKLOG_CAP {
                            self.bus_lines.pop_front();
                        }
                        got_any = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.bus_rx = None;
                        self.bus_status = BusStatus::Failed;
                        break;
                    }
                }
            }
        }
        if got_any && self.bus_status == BusStatus::Connecting {
            self.bus_status = BusStatus::Streaming;
        }

        // Drain per-agent snapshots into the map. Same channel discipline
        // as bus_rx; Disconnected means the reader thread died.
        if let Some(rx) = &self.agent_rx {
            loop {
                match rx.try_recv() {
                    Ok(snap) => {
                        self.agents.insert(snap.agent_id.clone(), snap);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.agent_rx = None;
                        break;
                    }
                }
            }
        }

        // Evict long-silent agents so the map can't grow unbounded as
        // constellation membership churns over a long session. Stale-but-
        // recent agents still render dimmed (AGENT_FRESH_WINDOW); only the
        // truly gone (past AGENT_EVICT_WINDOW) are dropped.
        let now = Instant::now();
        self.agents
            .retain(|_, s| now.duration_since(s.last_seen) < AGENT_EVICT_WINDOW);

        // Drain dream events into the rolling history.
        if let Some(rx) = &self.dream_rx {
            loop {
                match rx.try_recv() {
                    Ok(ev) => {
                        self.dream_history.push_front(ev);
                        while self.dream_history.len() > DREAM_HISTORY_CAP {
                            self.dream_history.pop_back();
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.dream_rx = None;
                        break;
                    }
                }
            }
        }

        // Drain the local dream-trigger worker (if one is running).
        if let Some(rx) = &self.dream_trigger_rx {
            match rx.try_recv() {
                Ok(Ok(summary)) => {
                    if let DreamRunState::Running { mode, started } = &self.dream_run {
                        let took = started.elapsed();
                        self.dream_run = DreamRunState::Done {
                            mode: mode.clone(),
                            took,
                            summary,
                        };
                    }
                    self.dream_trigger_rx = None;
                    // Refresh metrics — dream just changed memory state.
                    self.load_status();
                    self.load_observe();
                }
                Ok(Err(err)) => {
                    if let DreamRunState::Running { mode, .. } = &self.dream_run {
                        self.dream_run = DreamRunState::Failed {
                            mode: mode.clone(),
                            error: err,
                        };
                    }
                    self.dream_trigger_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.dream_trigger_rx = None;
                }
            }
        }
    }

    /// Trigger a non-blocking dream cycle. Returns immediately; the
    /// `dream_trigger_rx` Receiver fires when the child exits and
    /// `poll_bus` transitions DreamRunState to Done/Failed.
    fn start_dream(&mut self, mode: &str) {
        if self.dream_trigger_rx.is_some() {
            return;
        }
        let mode = mode.to_string();
        self.dream_run = DreamRunState::Running {
            mode: mode.clone(),
            started: Instant::now(),
        };
        let (tx, rx) = mpsc::channel::<Result<String, String>>();
        self.dream_trigger_rx = Some(rx);
        let bin = self.kannaka_bin.clone();
        std::thread::spawn(move || {
            let output = run_capture(&bin, &["dream", "--mode", &mode], Duration::from_secs(300));
            let result = match output {
                Ok(out) if out.status.success() => {
                    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    Err(stderr.trim().to_string())
                }
                Err(e) => Err(e),
            };
            let _ = tx.send(result);
        });
    }

    fn find_kannaka_binary() -> String {
        // Explicit override wins — the doctrine's adapter seam. Lets the
        // user point the TUI at any kannaka build (CI, a worktree, a
        // sibling checkout) without recompiling.
        if let Ok(p) = std::env::var("KANNAKA_BIN") {
            let p = p.trim();
            if !p.is_empty() {
                return p.to_string();
            }
        }
        // Check for release build next to this binary
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let sibling = dir.join("kannaka.exe");
                if sibling.exists() {
                    return sibling.to_string_lossy().into_owned();
                }
                let sibling = dir.join("kannaka");
                if sibling.exists() {
                    return sibling.to_string_lossy().into_owned();
                }
            }
        }
        // Fallback: the known release path (check Windows .exe then bare name).
        if let Some(home) = dirs::home_dir() {
            let base = home.join("Source/kannaka-memory/target/release");
            for name in &["kannaka.exe", "kannaka"] {
                let candidate = base.join(name);
                if candidate.exists() {
                    return candidate.to_string_lossy().into_owned();
                }
            }
        }
        // Last resort: rely on PATH
        "kannaka".to_string()
    }

    fn load_agent_name() -> String {
        let config_path = dirs::home_dir()
            .map(|h| h.join(".kannaka/config.toml"))
            .unwrap_or_default();
        if config_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                // Parse TOML for agent.id or agent.display_name
                if let Ok(val) = content.parse::<toml::Table>() {
                    if let Some(agent) = val.get("agent").and_then(|a| a.as_table()) {
                        if let Some(name) = agent.get("display_name").and_then(|v| v.as_str()) {
                            if !name.is_empty() {
                                return name.to_string();
                            }
                        }
                        if let Some(id) = agent.get("id").and_then(|v| v.as_str()) {
                            return id.to_string();
                        }
                    }
                }
            }
        }
        "unknown".to_string()
    }

    /// Spawn a background `kannaka status` poll. The TUI used to block
    /// `App::new()` on this for ~30s while the eigendecomp ran on the
    /// loaded HRM — users thought the TUI hadn't started. Now we kick
    /// off a worker and drain its result in the event loop.
    fn load_status(&mut self) {
        if self.status_pending.is_some() {
            return;
        } // already in flight
        let bin = self.kannaka_bin.clone();
        let (tx, rx) = std::sync::mpsc::channel::<Result<Status, String>>();
        self.status_pending = Some(rx);
        self.last_status_poll = Instant::now();
        std::thread::spawn(move || {
            // ADR-0029 Phase 4b — opt into the envelope shape so we
            // get an unambiguous success/error signal in stdout. We
            // read fields under .data.X; tolerate the legacy flat
            // shape too in case the kannaka binary is older than
            // v0.6.3 (envelope-aware status landed there).
            // 60s cap: the first poll runs an eigendecomp over the whole
            // medium and can take ~30s; a wedged binary past 60s gets
            // killed so `status_pending` clears instead of blocking every
            // future refresh forever.
            let output = run_capture(&bin, &["status", "--envelope"], Duration::from_secs(60));
            let result = match output {
                Ok(out) if out.status.success() => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    match serde_json::from_str::<serde_json::Value>(&stdout) {
                        Ok(val) => {
                            // Envelope detection: schema_version + data present.
                            // Fall back to flat shape so older kannaka binaries
                            // still work (they emit the legacy object directly).
                            let body = if val.get("schema_version").is_some()
                                && val.get("data").is_some()
                            {
                                val["data"].clone()
                            } else {
                                val
                            };
                            // Schema-drift guard: if neither of the two core
                            // fields is present, the binary's status shape
                            // changed under us — surface that instead of
                            // rendering a plausible-but-fake all-zero status.
                            if body.get("phi").is_none() && body.get("total_memories").is_none() {
                                Err("status schema unrecognized — kannaka binary may be newer or older than this TUI".to_string())
                            } else {
                                Ok(Status {
                                    phi: body["phi"].as_f64().unwrap_or(0.0) as f32,
                                    xi: body["xi"].as_f64().unwrap_or(0.0) as f32,
                                    order: body["mean_order"].as_f64().unwrap_or(0.0) as f32,
                                    memories: body["total_memories"].as_u64().unwrap_or(0),
                                    clusters: body["num_clusters"].as_u64().unwrap_or(0),
                                    links: 0,
                                    level: body["consciousness_level"]
                                        .as_str()
                                        .unwrap_or("Unknown")
                                        .to_string(),
                                    active: body["active_memories"].as_u64().unwrap_or(0),
                                })
                            }
                        }
                        Err(e) => Err(format!("status parse: {e}")),
                    }
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    Err(format!("status failed: {}", stderr.trim()))
                }
                Err(e) => Err(format!("status {e}")),
            };
            let _ = tx.send(result);
        });
    }

    /// Spawn a background `kannaka observe --json` poll. Same async
    /// pattern as load_status — never blocks the event loop.
    fn load_observe(&mut self) {
        if self.observe_pending.is_some() {
            return;
        }
        let bin = self.kannaka_bin.clone();
        let (tx, rx) = std::sync::mpsc::channel::<Result<(u64, Vec<MemoryEntry>), String>>();
        self.observe_pending = Some(rx);
        std::thread::spawn(move || {
            let output = run_capture(&bin, &["observe", "--json"], Duration::from_secs(60));
            let result = match output {
                Ok(out) if out.status.success() => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    match serde_json::from_str::<serde_json::Value>(&stdout) {
                        Ok(val) => {
                            let links = val["topology"]["total_links"].as_u64().unwrap_or(0);
                            let memories = val["waves"]["strongest"]
                                .as_array()
                                .map(|arr| {
                                    arr.iter()
                                        .map(|m| MemoryEntry {
                                            content: m["content_preview"]
                                                .as_str()
                                                .unwrap_or("")
                                                .to_string(),
                                            amplitude: m["amplitude"].as_f64().unwrap_or(0.0)
                                                as f32,
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            Ok((links, memories))
                        }
                        Err(e) => Err(format!("observe parse: {e}")),
                    }
                }
                Ok(out) => Err(format!(
                    "observe failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )),
                Err(e) => Err(format!("observe {e}")),
            };
            let _ = tx.send(result);
        });
    }

    /// Drain async status/observe responses if ready. Called every event
    /// loop tick. Non-blocking.
    fn poll_async_data(&mut self) {
        if let Some(rx) = &self.status_pending {
            match rx.try_recv() {
                Ok(Ok(s)) => {
                    self.status = Some(s);
                    self.status_pending = None;
                }
                Ok(Err(e)) => {
                    self.push_message(Message {
                        role: Role::Error,
                        content: e,
                    });
                    self.status_pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(_) => {
                    self.status_pending = None;
                }
            }
        }
        if let Some(rx) = &self.observe_pending {
            match rx.try_recv() {
                Ok(Ok((links, mems))) => {
                    if let Some(ref mut s) = self.status {
                        s.links = links;
                    }
                    self.memories = mems;
                    self.observe_pending = None;
                }
                Ok(Err(e)) => {
                    self.push_message(Message {
                        role: Role::Error,
                        content: e,
                    });
                    self.observe_pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(_) => {
                    self.observe_pending = None;
                }
            }
        }
    }

    /// Append to the Memory-tab message log, trimming the oldest entries
    /// so it never grows past `MESSAGES_CAP`. All log writes should go
    /// through here rather than `self.messages.push` directly.
    fn push_message(&mut self, msg: Message) {
        self.messages.push(msg);
        if self.messages.len() > MESSAGES_CAP {
            let overflow = self.messages.len() - MESSAGES_CAP;
            self.messages.drain(0..overflow);
        }
    }

    /// Trim the oldest Chat-tab entries if the log has grown past
    /// `CHAT_MESSAGES_CAP`. Called at the end of every function that pushes
    /// to `chat_messages` so the vec is bounded across a long session.
    fn trim_chat_messages(&mut self) {
        if self.chat_messages.len() > CHAT_MESSAGES_CAP {
            let overflow = self.chat_messages.len() - CHAT_MESSAGES_CAP;
            self.chat_messages.drain(0..overflow);
        }
    }

    /// Guard so only one remember/recall/forget runs at a time — they
    /// share the single `cmd_pending` channel. Returns true (and pushes a
    /// notice) if one is already in flight.
    fn cmd_busy(&mut self) -> bool {
        if self.cmd_pending.is_some() {
            self.push_message(Message {
                role: Role::System,
                content: "A memory command is already running — wait for it to finish.".into(),
            });
            true
        } else {
            false
        }
    }

    fn execute_remember(&mut self, text: &str) {
        self.push_message(Message {
            role: Role::User,
            content: format!("remember \"{text}\""),
        });
        if self.cmd_busy() {
            return;
        }

        let bin = self.kannaka_bin.clone();
        let text = text.to_string();
        let (tx, rx) = mpsc::channel::<CmdResult>();
        self.cmd_pending = Some(rx);
        std::thread::spawn(move || {
            let output = run_capture(&bin, &["remember", &text], Duration::from_secs(60));
            let result = match output {
                Ok(out) if out.status.success() => {
                    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    CmdResult {
                        messages: vec![Message {
                            role: Role::Result,
                            content: format!("Stored (id: {id})"),
                        }],
                        refresh_observe: true,
                    }
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    CmdResult {
                        messages: vec![Message {
                            role: Role::Error,
                            content: format!("Error: {}", stderr.trim()),
                        }],
                        refresh_observe: false,
                    }
                }
                Err(e) => CmdResult {
                    messages: vec![Message {
                        role: Role::Error,
                        content: format!("Failed to run kannaka: {e}"),
                    }],
                    refresh_observe: false,
                },
            };
            let _ = tx.send(result);
        });
    }

    fn execute_recall(&mut self, query: &str) {
        self.push_message(Message {
            role: Role::User,
            content: format!("recall \"{query}\""),
        });
        if self.cmd_busy() {
            return;
        }

        let bin = self.kannaka_bin.clone();
        let query = query.to_string();
        let (tx, rx) = mpsc::channel::<CmdResult>();
        self.cmd_pending = Some(rx);
        std::thread::spawn(move || {
            let start = Instant::now();
            let output = run_capture(
                &bin,
                &["recall", &query, "--top-k", "5"],
                Duration::from_secs(60),
            );
            let mut messages = Vec::new();
            match output {
                Ok(out) if out.status.success() => {
                    let elapsed = start.elapsed();
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    if let Ok(results) = serde_json::from_str::<Vec<serde_json::Value>>(&stdout) {
                        messages.push(Message {
                            role: Role::System,
                            content: format!(
                                "{} results ({:.0}ms):",
                                results.len(),
                                elapsed.as_secs_f64() * 1000.0
                            ),
                        });
                        for (i, r) in results.iter().enumerate() {
                            let content = r["content"].as_str().unwrap_or("?");
                            let sim = r["similarity"].as_f64().unwrap_or(0.0);
                            // Truncate content for display
                            let preview: String = content.chars().take(60).collect();
                            messages.push(Message {
                                role: Role::Result,
                                content: format!("  {}. {preview} ({sim:.2})", i + 1),
                            });
                        }
                    } else {
                        messages.push(Message {
                            role: Role::Result,
                            content: stdout.trim().to_string(),
                        });
                    }
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    messages.push(Message {
                        role: Role::Error,
                        content: format!("Error: {}", stderr.trim()),
                    });
                }
                Err(e) => {
                    messages.push(Message {
                        role: Role::Error,
                        content: format!("Failed: {e}"),
                    });
                }
            }
            let _ = tx.send(CmdResult {
                messages,
                refresh_observe: false,
            });
        });
    }

    /// Kick off a dream from the command bar (`dream` or `dream lite`).
    /// Always non-blocking — the worker thread reports back via
    /// dream_trigger_rx. Was blocking pre-v0.5.8 and froze the TUI for
    /// the duration of consolidation (~30s).
    fn execute_dream(&mut self) {
        if self.dream_trigger_rx.is_some() {
            self.push_message(Message {
                role: Role::System,
                content: "A dream is already running — wait for it to finish.".into(),
            });
            return;
        }
        self.push_message(Message {
            role: Role::User,
            content: "dream --mode deep".to_string(),
        });
        self.push_message(Message {
            role: Role::System,
            content: "Dream cycle started in background — Dreams tab for progress.".to_string(),
        });
        // Make sure the bus is on so the post-dream KANNAKA.dreams event
        // shows up in the history list.
        self.start_bus();
        self.start_dream("deep");
    }

    fn execute_forget(&mut self, query: &str) {
        self.push_message(Message {
            role: Role::User,
            content: format!("forget \"{query}\""),
        });
        if self.cmd_busy() {
            return;
        }

        let bin = self.kannaka_bin.clone();
        let query = query.to_string();
        let (tx, rx) = mpsc::channel::<CmdResult>();
        self.cmd_pending = Some(rx);
        std::thread::spawn(move || {
            let output = run_capture(&bin, &["forget", &query], Duration::from_secs(60));
            let result = match output {
                Ok(out) if out.status.success() => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    CmdResult {
                        messages: vec![Message {
                            role: Role::Result,
                            content: stdout.trim().to_string(),
                        }],
                        refresh_observe: true,
                    }
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    CmdResult {
                        messages: vec![Message {
                            role: Role::Error,
                            content: format!("Error: {}", stderr.trim()),
                        }],
                        refresh_observe: false,
                    }
                }
                Err(e) => CmdResult {
                    messages: vec![Message {
                        role: Role::Error,
                        content: format!("Failed: {e}"),
                    }],
                    refresh_observe: false,
                },
            };
            let _ = tx.send(result);
        });
    }

    /// Drain a completed remember/recall/forget worker. Non-blocking;
    /// called every event-loop tick.
    fn poll_cmd(&mut self) {
        if let Some(rx) = &self.cmd_pending {
            match rx.try_recv() {
                Ok(res) => {
                    for m in res.messages {
                        self.push_message(m);
                    }
                    let refresh = res.refresh_observe;
                    self.cmd_pending = None;
                    if refresh {
                        self.load_observe();
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.cmd_pending = None;
                }
            }
        }
    }

    // Forward an arbitrary kannaka subcommand to the binary and surface its
    // stdout/stderr in the message log. The label is what we echo back as
    // the User line; args is what we pass to kannaka after env scrubbing.
    // Used for hear, ask, assess, stats, voice, swarm subcommands, and
    // anything else the user types that we recognize as a real kannaka
    // command. Keeps the TUI the canonical surface without writing a
    // dedicated handler for every subcommand.
    fn execute_passthrough(&mut self, label: &str, args: &[&str], timeout_secs: u64) {
        self.push_message(Message {
            role: Role::User,
            content: label.to_string(),
        });
        // One passthrough at a time — they share the single
        // `passthrough_pending` channel. (remember/recall/forget run on
        // their own `cmd_pending` channel and may overlap with this.)
        if self.passthrough_pending.is_some() {
            self.push_message(Message {
                role: Role::System,
                content: "Another command is already running — wait for it to finish.".into(),
            });
            return;
        }
        self.push_message(Message {
            role: Role::System,
            content: format!("Running... (up to {timeout_secs}s)"),
        });

        // Run on a worker with a wall-clock timeout and report back via
        // `passthrough_pending`, drained in `poll_passthrough`. This used
        // to `thread::spawn(...).join()` ON THE UI THREAD, freezing the
        // ENTIRE TUI (no render, no input, no quit) for up to
        // `timeout_secs` — as long as 600s for `ask`. Now it is fully
        // async like every other op in this file.
        let bin = self.kannaka_bin.clone();
        let owned: Vec<String> = args.iter().map(ToString::to_string).collect();
        let (tx, rx) = mpsc::channel::<CmdResult>();
        self.passthrough_pending = Some(rx);
        std::thread::spawn(move || {
            let arg_refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            let result = run_capture(&bin, &arg_refs, Duration::from_secs(timeout_secs));
            let cmd_result = match result {
                Ok(out) if out.status.success() => {
                    let body = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    CmdResult {
                        messages: vec![Message {
                            role: Role::Result,
                            content: if body.is_empty() {
                                "(no output)".into()
                            } else {
                                body
                            },
                        }],
                        // Several passthroughs (hear/see/voice) mutate the
                        // medium; refresh observe on success as the old
                        // sync path did.
                        refresh_observe: true,
                    }
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    CmdResult {
                        messages: vec![Message {
                            role: Role::Error,
                            content: format!("Error: {}", stderr.trim()),
                        }],
                        refresh_observe: false,
                    }
                }
                Err(msg) => CmdResult {
                    messages: vec![Message {
                        role: Role::Error,
                        content: msg,
                    }],
                    refresh_observe: false,
                },
            };
            let _ = tx.send(cmd_result);
        });
    }

    /// Drain a completed passthrough command. Non-blocking; called every
    /// event-loop tick. Pops the trailing "Running…" placeholder if it is
    /// still the last line. The check matches the placeholder's CONTENT,
    /// not just its System role — a concurrent `recall` can push its own
    /// trailing System header (e.g. "0 results"), and a role-only check
    /// would wrongly delete that instead of our placeholder.
    fn poll_passthrough(&mut self) {
        if let Some(rx) = &self.passthrough_pending {
            match rx.try_recv() {
                Ok(res) => {
                    let pop_running = self.messages.last().is_some_and(|m| {
                        matches!(m.role, Role::System) && m.content.starts_with("Running...")
                    });
                    if pop_running {
                        self.messages.pop();
                    }
                    let refresh = res.refresh_observe;
                    for m in res.messages {
                        self.push_message(m);
                    }
                    self.passthrough_pending = None;
                    if refresh {
                        self.load_observe();
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.passthrough_pending = None;
                }
            }
        }
    }

    /// Kick off a one-shot Cosmos poll: `kannaka constellation` (health of
    /// every constellation app) + `kannaka radio` (now playing), both on a
    /// worker with timeouts. Partial success is fine — if radio is down we
    /// still show the app grid, and vice versa. Drained by `poll_cosmos`.
    fn load_cosmos(&mut self) {
        if self.cosmos_pending.is_some() {
            return;
        }
        self.cosmos_last_load = Instant::now();
        let bin = self.kannaka_bin.clone();
        let (tx, rx) = mpsc::channel::<Result<(Vec<CosmosApp>, Vec<String>), String>>();
        self.cosmos_pending = Some(rx);
        std::thread::spawn(move || {
            // constellation pings ~10 endpoints over HTTP — give it 30s.
            let apps = match run_capture(&bin, &["constellation"], Duration::from_secs(30)) {
                Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter_map(parse_constellation_line)
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            // radio: a single now-playing query; 15s is plenty.
            let radio = match run_capture(&bin, &["radio"], Duration::from_secs(15)) {
                Ok(out) if out.status.success() => {
                    clean_radio_lines(&String::from_utf8_lossy(&out.stdout))
                }
                _ => Vec::new(),
            };
            let result = if apps.is_empty() && radio.is_empty() {
                Err(
                    "could not reach the constellation (constellation/radio returned nothing)"
                        .to_string(),
                )
            } else {
                Ok((apps, radio))
            };
            let _ = tx.send(result);
        });
    }

    /// Drain a completed Cosmos poll into render state. Non-blocking.
    fn poll_cosmos(&mut self) {
        if let Some(rx) = &self.cosmos_pending {
            match rx.try_recv() {
                Ok(Ok((apps, radio))) => {
                    self.cosmos_apps = apps;
                    self.cosmos_radio = radio;
                    self.cosmos_error = None;
                    self.cosmos_pending = None;
                }
                Ok(Err(e)) => {
                    self.cosmos_error = Some(e);
                    self.cosmos_pending = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.cosmos_pending = None;
                }
            }
        }
    }

    fn submit_input(&mut self) {
        let input = self.input.trim().to_string();
        if input.is_empty() {
            return;
        }

        // Save to history, capped so the Vec never grows unbounded.
        self.history.push(input.clone());
        self.history_idx = None;
        if self.history.len() > HISTORY_CAP {
            self.history.remove(0);
        }

        // Agent harness tab — drive the kannaka coding agent.
        if self.tabs.get(self.active_tab).copied() == Some("Agent") {
            let cmd = input.strip_prefix('/').map(str::trim);
            match cmd {
                Some("help" | "?") => self.show_help = true,
                Some("quit" | "exit" | "q") => self.should_quit = true,
                Some("yolo") => self.set_harness_mode("yolo"),
                Some("plan") => self.set_harness_mode("plan"),
                Some("default") => self.set_harness_mode("default"),
                Some("auto" | "auto-edit") => self.set_harness_mode("auto-edit"),
                Some("stop" | "cancel") => self.stop_harness(),
                Some("clear") => {
                    self.restart_harness();
                    self.harness_lines.clear();
                    self.harness_usage_in = 0;
                    self.harness_usage_out = 0;
                    self.harness_iter = 0;
                    self.push_harness(AgentLine::Notice(
                        "[cleared — type a task to start a fresh agent]".into(),
                    ));
                }
                Some(rest) if rest.starts_with("mode ") => {
                    let m = rest["mode ".len()..].trim().to_string();
                    self.set_harness_mode(&m);
                }
                Some(rest) if rest.starts_with("model ") => {
                    let m = rest["model ".len()..].trim().to_string();
                    self.harness_model.clone_from(&m);
                    self.restart_harness();
                    self.push_harness(AgentLine::Notice(format!(
                        "[model set to {m} — type a task to start]"
                    )));
                }
                Some(c) if c == "qos" || c.starts_with("qos ") => {
                    let graphical = {
                        let rest = c.strip_prefix("qos").unwrap_or("").trim();
                        rest == "--graphical" || rest == "-g" || rest == "graphical"
                    };
                    if self.harness_pending.is_some() {
                        self.push_harness(AgentLine::Notice(
                            "[approval pending — press a (allow), s (allow always), or d (deny)]"
                                .into(),
                        ));
                    } else if matches!(
                        self.harness_status,
                        HarnessStatus::Thinking | HarnessStatus::Starting
                    ) {
                        self.push_harness(AgentLine::Notice(
                            "[agent is working — wait for it to finish or /stop]".into(),
                        ));
                    } else {
                        let (notice, prompt) = if graphical {
                            (
                                "[/qos --graphical — booting QuantumOS with a live browser view]",
                                QOS_BOOT_GRAPHICAL_PROMPT,
                            )
                        } else {
                            (
                                "[/qos — booting QuantumOS on a qBraid Lab instance]",
                                QOS_BOOT_PROMPT,
                            )
                        };
                        self.push_harness(AgentLine::Notice(notice.into()));
                        self.harness_user_turn(prompt.to_string());
                    }
                }
                Some(other) => self.push_harness(AgentLine::Notice(format!(
                    "[unknown command /{other} — try /help]"
                ))),
                None => {
                    if self.harness_pending.is_some() {
                        self.push_harness(AgentLine::Notice(
                            "[approval pending — press a (allow), s (allow always), or d (deny)]"
                                .into(),
                        ));
                    } else if matches!(
                        self.harness_status,
                        HarnessStatus::Thinking | HarnessStatus::Starting
                    ) {
                        self.push_harness(AgentLine::Notice(
                            "[agent is working — wait for it to finish]".into(),
                        ));
                    } else {
                        self.harness_user_turn(input.clone());
                    }
                }
            }
            self.input.clear();
            self.cursor_pos = 0;
            return;
        }

        // Chat tab — send to agent in a background thread.
        if self.tabs.get(self.active_tab).copied() == Some("Chat") {
            if self.chat_pending.is_some() {
                // A previous turn is still in flight — ignore new input.
                self.input.clear();
                self.cursor_pos = 0;
                return;
            }

            // Plugin slash commands. `/code <prompt>` execs kannaka-code,
            // `/topus <prompt>` execs kannaktopus. Both stream stdout
            // into ChatLines so the operator sees the plugin's output
            // inline in the conversation. Plugin invocation is async on
            // its own thread so the TUI stays responsive while the
            // plugin works.
            if let Some(prompt) = input.strip_prefix("/code ") {
                self.spawn_plugin_turn("kannaka-code", "/code", prompt.trim());
                self.input.clear();
                self.cursor_pos = 0;
                self.scroll_offset = 0;
                return;
            }
            if let Some(prompt) = input.strip_prefix("/topus ") {
                self.spawn_plugin_turn("kannaktopus", "/topus", prompt.trim());
                self.input.clear();
                self.cursor_pos = 0;
                self.scroll_offset = 0;
                return;
            }

            self.chat_messages.push(ChatLine {
                who: ChatWho::User,
                text: input.clone(),
            });
            self.spawn_chat_turn(input);
            self.input.clear();
            self.cursor_pos = 0;
            self.scroll_offset = 0;
            return;
        }

        // Strip an optional leading '/' so `/recall x` and `recall x` both work.
        // The slash is the conventional escape-hatch for "this is a command,
        // not chat" — useful when the user wants to be unambiguous.
        let cmd_input: &str = input.strip_prefix('/').unwrap_or(&input);

        // Parse the command. If nothing matches, default to chat — the agent
        // can call recall/remember/observe tools itself when the conversation
        // warrants. The TUI is a chat surface first, command surface second.
        if let Some(text) = cmd_input.strip_prefix("remember ") {
            self.execute_remember(text.trim().trim_matches('"'));
        } else if let Some(query) = cmd_input.strip_prefix("recall ") {
            self.execute_recall(query.trim().trim_matches('"'));
        } else if let Some(id) = cmd_input.strip_prefix("forget ") {
            self.execute_forget(id.trim());
        } else if cmd_input == "dream" || cmd_input.starts_with("dream ") {
            self.execute_dream();
        } else if cmd_input == "status" || cmd_input == "observe" {
            self.load_status();
            self.load_observe();
            self.push_message(Message {
                role: Role::System,
                content: "Status refreshed.".to_string(),
            });
        } else if cmd_input.starts_with("hear ") || cmd_input == "hear" {
            // hear <file-or-url> [--secs N]
            let rest = cmd_input.strip_prefix("hear").unwrap_or("").trim();
            if rest.is_empty() {
                self.push_message(Message {
                    role: Role::Error,
                    content: "Usage: hear <file-or-url> [--secs N]".into(),
                });
            } else {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                let mut args: Vec<&str> = vec!["hear"];
                args.extend(parts.iter().copied());
                // hear can take ~30-60s for stream sampling + decode + HRM
                // absorb. Give it 5 min wall-clock so /stream sampling at
                // --secs 60 has comfortable headroom.
                self.execute_passthrough(&format!("hear {rest}"), &args, 300);
            }
        } else if let Some(q) = cmd_input.strip_prefix("ask ") {
            let q = q.trim().trim_matches('"');
            // ask runs through Anthropic; budget 10 min like the radio's
            // peace-oration path so transient overload retries fit.
            self.execute_passthrough(
                &format!("ask \"{q}\""),
                &["ask", "--no-tools", "--quiet-tools", q],
                600,
            );
        } else if let Some(q) = cmd_input.strip_prefix("search ") {
            let q = q.trim().trim_matches('"');
            self.execute_passthrough(&format!("search \"{q}\""), &["search", q], 30);
        } else if let Some(id) = cmd_input.strip_prefix("boost ") {
            let id = id.trim();
            self.execute_passthrough(&format!("boost {id}"), &["boost", id], 30);
        } else if cmd_input == "assess" {
            self.execute_passthrough("assess", &["assess"], 60);
        } else if cmd_input == "stats" {
            self.execute_passthrough("stats", &["stats"], 30);
        } else if cmd_input == "cmf" {
            self.execute_passthrough("cmf", &["cmf"], 60);
        } else if cmd_input == "invariant" || cmd_input.starts_with("invariant ") {
            let parts: Vec<&str> = cmd_input.split_whitespace().collect();
            self.execute_passthrough(cmd_input, &parts, 60);
        } else if cmd_input.starts_with("voice") {
            let parts: Vec<&str> = cmd_input.split_whitespace().collect();
            // voice --mode dream-journal etc. — long-form generation, 5 min budget.
            self.execute_passthrough(cmd_input, &parts, 300);
        } else if cmd_input.starts_with("swarm ") || cmd_input == "swarm" {
            // Forward the whole `swarm <subcmd> [args]` line. swarm sync /
            // join / status / queen / hives / publish / leave / listen / serve
            // / peers / absorb / autoabsorb / enqueue / worker / exemplars are
            // all valid — let the binary's parser handle them.
            let parts: Vec<&str> = cmd_input.split_whitespace().collect();
            // Most swarm commands return quickly; serve/listen are blocking
            // and we don't want them via the TUI (they'd hang the input).
            // Cap at 60s so a network hang doesn't lock the UI.
            self.execute_passthrough(cmd_input, &parts, 60);
        } else if let Some(q) = cmd_input.strip_prefix("relate ") {
            let q = q.trim().trim_matches('"');
            self.execute_passthrough(&format!("relate \"{q}\""), &["relate", q], 60);
        } else if let Some(q) = cmd_input.strip_prefix("neighbors ") {
            let q = q.trim().trim_matches('"');
            self.execute_passthrough(&format!("neighbors \"{q}\""), &["neighbors", q], 60);
        } else if cmd_input == "clusters" || cmd_input.starts_with("clusters ") {
            let parts: Vec<&str> = cmd_input.split_whitespace().collect();
            self.execute_passthrough(cmd_input, &parts, 60);
        } else if cmd_input == "topology" {
            self.execute_passthrough("topology", &["topology"], 60);
        } else if cmd_input == "market" || cmd_input.starts_with("market ") {
            let parts: Vec<&str> = cmd_input.split_whitespace().collect();
            self.execute_passthrough(cmd_input, &parts, 30);
        } else if let Some(rest) = cmd_input.strip_prefix("see ") {
            let rest = rest.trim();
            if rest.is_empty() {
                self.push_message(Message {
                    role: Role::Error,
                    content: "Usage: see <file-or-url>".into(),
                });
            } else {
                let parts: Vec<&str> = std::iter::once("see")
                    .chain(rest.split_whitespace())
                    .collect();
                // Visual absorb — decode + wavefront embed can take a while.
                self.execute_passthrough(&format!("see {rest}"), &parts, 300);
            }
        } else if cmd_input == "constellation"
            || cmd_input == "cosmos"
            || cmd_input == "apps"
            || cmd_input == "radio"
            || cmd_input.starts_with("radio ")
        {
            // These live on the Cosmos tab — jump there and refresh rather
            // than dumping raw status text into the message log.
            if let Some(idx) = self.tabs.iter().position(|t| *t == "Cosmos") {
                self.active_tab = idx;
            }
            self.load_cosmos();
            self.push_message(Message {
                role: Role::System,
                content: "Opening Cosmos — constellation + radio status.".into(),
            });
        } else if cmd_input == "help" || cmd_input == "?" {
            self.show_help = true;
        } else if cmd_input == "quit" || cmd_input == "exit" || cmd_input == "q" {
            self.should_quit = true;
        } else {
            // Default: route to chat. Switch to the Chat tab so the user sees
            // the conversation, and let the agent decide which tools to call.
            if let Some(idx) = self.tabs.iter().position(|t| *t == "Chat") {
                self.active_tab = idx;
            }
            if self.chat_pending.is_some() {
                // A previous turn is still in flight — drop the new prompt
                // rather than queueing (avoids surprising long-tail behavior).
                self.input.clear();
                self.cursor_pos = 0;
                return;
            }
            self.chat_messages.push(ChatLine {
                who: ChatWho::User,
                text: input.clone(),
            });
            self.spawn_chat_turn(input);
        }

        self.input.clear();
        self.cursor_pos = 0;
        // Auto-scroll to bottom
        self.scroll_offset = 0;
        self.trim_chat_messages();
    }

    /// Lazily spawn the persistent `kannaka chat --json` child. The child
    /// loads HRM once at startup (the slow 15s step); every subsequent
    /// turn reuses that loaded medium for ~3-5s per turn instead of
    /// shelling out a fresh `kannaka ask` each time and paying the load
    /// cost on every message. First chat turn is therefore slow (~15s);
    /// everything after that is fast.
    fn ensure_chat_child(&mut self) {
        if self.chat_child.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<ChatChildEvent>();
        self.chat_child_rx = Some(rx);
        let bin = self.kannaka_bin.clone();
        let proc_slot = std::sync::Arc::clone(&self.chat_child_proc);
        // Spawn-and-attach happens on a worker so the TUI doesn't block
        // for the ~15s HRM load. The worker:
        //   1. Spawns `kannaka chat --json`
        //   2. Sends `Ready` once the child prints its `{"kind":"ready"}` line on stderr
        //   3. Streams stdout NDJSON as `Response { text, kind }` events
        //   4. On child exit / IO error, sends `Closed(reason)`
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            use std::process::{Command, Stdio};
            let mut child = match Command::new(&bin)
                .args(["chat", "--json"])
                .env("KANNAKA_QUIET", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(ChatChildEvent::Closed(format!("spawn failed: {e}")));
                    return;
                }
            };
            // Hand stdin back to the parent via a Stdin event so the
            // turn-sender side can write to it. Stdout/stderr stay in
            // the worker.
            if let Some(stdin) = child.stdin.take() {
                let _ = tx.send(ChatChildEvent::Stdin(stdin));
            } else {
                let _ = tx.send(ChatChildEvent::Closed("no stdin pipe".into()));
                return;
            }
            // Stderr reader thread — emits Ready on first ready event.
            if let Some(stderr) = child.stderr.take() {
                let tx_err = tx.clone();
                std::thread::spawn(move || {
                    let reader = BufReader::new(stderr);
                    // map_while(Result::ok) instead of .flatten() —
                    // a persistent io::Error would make flatten() loop
                    // forever burning CPU. map_while stops on first Err.
                    for line in reader.lines().map_while(Result::ok) {
                        if line.contains("\"ready\"") {
                            let _ = tx_err.send(ChatChildEvent::Ready);
                        }
                    }
                });
            }
            // Take stdout before handing the Child off to the shared slot
            // so the reader below still owns the pipe.
            let stdout = child.stdout.take();
            // Stash the Child so the main thread can kill()/wait() it on
            // quit — otherwise this `kannaka chat` process leaks.
            if let Ok(mut guard) = proc_slot.lock() {
                *guard = Some(child);
            }
            // Stdout reader — parse NDJSON and forward each turn response.
            if let Some(stdout) = stdout {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        let kind = v["kind"].as_str().unwrap_or("chat").to_string();
                        let text = v["text"].as_str().unwrap_or("").to_string();
                        let _ = tx.send(ChatChildEvent::Response { kind, text });
                    }
                }
            }
            let _ = tx.send(ChatChildEvent::Closed("child stdout EOF".into()));
        });
        self.chat_child = Some(ChatChildHandle {
            stdin: None,
            ready: false,
        });
    }

    /// Plugin slash commands — exec `binary` with the prompt as a
    /// single positional arg. Stdout streams into chat_messages as
    /// it arrives (line-by-line) so the operator sees the plugin's
    /// progress live instead of one big blob at the end. Inspired
    /// by the chat-child pattern but simpler: plugins are one-shot
    /// (run-to-completion), not interactive REPLs.
    ///
    /// `verb` is the slash command echoed back ("/code" or "/topus")
    /// so the conversation log keeps which path the prompt took.
    fn spawn_plugin_turn(&mut self, binary: &str, verb: &str, prompt: &str) {
        // Echo the invocation into the chat tab so the operator sees
        // what they typed routed to which plugin.
        self.chat_messages.push(ChatLine {
            who: ChatWho::User,
            text: format!("{verb} {prompt}"),
        });

        // Pre-flight: if the binary isn't on PATH, fail fast with a
        // discoverable install hint instead of letting Command::spawn
        // emit a cryptic OS error.
        if std::process::Command::new(binary)
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            self.chat_messages.push(ChatLine {
                who: ChatWho::System,
                text: format!(
                    "[plugin '{binary}' not found on PATH — install it and try again. \
                     For kannaka-code: cargo install --git https://github.com/NickFlach/kannaka-code]"
                ),
            });
            return;
        }

        let bin = binary.to_string();
        let prompt = prompt.to_string();
        let (tx, rx) = mpsc::channel::<String>();
        // The plugin invocation reuses the chat_pending sentinel so
        // the spinner animation kicks on. Replace with a real per-
        // plugin tracking field if you need to distinguish later.
        self.chat_pending = Some(std::sync::mpsc::channel().1);
        let proc_slot = std::sync::Arc::clone(&self.plugin_child_proc);

        std::thread::spawn(move || {
            let mut child = match std::process::Command::new(&bin)
                .arg(&prompt)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(format!("[plugin spawn failed: {e}]"));
                    return;
                }
            };
            let stdout = child.stdout.take();
            // Stash the Child so quit-time teardown can reap an in-flight
            // plugin run instead of leaking it.
            if let Ok(mut guard) = proc_slot.lock() {
                *guard = Some(child);
            }
            if let Some(stdout) = stdout {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
            // Reap the child and clear the slot so a finished run doesn't
            // leave a stale (already-exited) handle for teardown to wait on.
            if let Ok(mut guard) = proc_slot.lock() {
                if let Some(mut child) = guard.take() {
                    let _ = child.wait();
                }
            }
        });

        // Stash the receiver on a per-plugin field so poll() can drain
        // it into chat_messages. Reuse the bus rx slot conceptually —
        // actually we need a new field. For minimal-diff this round,
        // store it inline as a small queue + thread → see poll_plugin
        // for drain logic.
        self.plugin_output_rx = Some(rx);
        self.trim_chat_messages();
    }

    /// Drain any pending plugin-stdout lines into chat_messages.
    /// Called from the main event loop each tick. When the channel
    /// closes (plugin exited), clear chat_pending so the spinner
    /// stops and the input bar accepts new turns.
    fn poll_plugin(&mut self) {
        let mut closed = false;
        if let Some(rx) = &self.plugin_output_rx {
            loop {
                match rx.try_recv() {
                    Ok(line) => {
                        // Skip empty lines so the log stays tight.
                        if !line.trim().is_empty() {
                            self.chat_messages.push(ChatLine {
                                who: ChatWho::System,
                                text: line,
                            });
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        closed = true;
                        break;
                    }
                }
            }
        }
        if closed {
            self.plugin_output_rx = None;
            self.chat_pending = None;
        }
        self.trim_chat_messages();
    }

    fn spawn_chat_turn(&mut self, user_msg: String) {
        // Lazy-spawn the persistent REPL on the first turn so the user
        // sees the "Loading HRM…" status only once.
        self.ensure_chat_child();
        // If the child is already running and ready, write the message to
        // its stdin. The reader thread will deliver the response via the
        // ChatChildEvent channel; poll_chat drains it into chat_messages.
        if let Some(ref mut handle) = self.chat_child {
            if let Some(ref mut stdin) = handle.stdin {
                use std::io::Write;
                let _ = writeln!(stdin, "{user_msg}");
                let _ = stdin.flush();
                self.chat_pending = Some(std::sync::mpsc::channel().1); // sentinel: a turn is in flight
                return;
            }
            // Child spawned but stdin not yet attached — buffer the message.
            self.chat_pending_msg = Some(user_msg);
            self.chat_pending = Some(std::sync::mpsc::channel().1);
            return;
        }
        // Fallback path — shouldn't normally hit this since ensure_chat_child
        // installs a handle. If we do (spawn failed instantly), fall back
        // to the one-shot `ask` path so the user gets *some* response.
        let bin = self.kannaka_bin.clone();
        let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
        self.chat_pending = Some(rx);
        std::thread::spawn(move || {
            let output = Command::new(&bin)
                .args([
                    "ask",
                    "--session",
                    "kannaka-tui",
                    "--quiet-tools",
                    &user_msg,
                ])
                .env("KANNAKA_QUIET", "1")
                .output();
            let result = match output {
                Ok(out) if out.status.success() => {
                    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    Err(format!(
                        "agent exited {}: {}",
                        out.status.code().unwrap_or(-1),
                        stderr.trim()
                    ))
                }
                Err(e) => Err(format!("spawn failed: {e}")),
            };
            let _ = tx.send(result);
        });
    }

    /// Called from the event loop each tick. Drains the persistent chat
    /// child's event channel (Stdin attach / Ready / Response / Closed)
    /// AND any legacy fallback `chat_pending` Receiver from the one-shot
    /// path. Non-blocking; appends new chat lines to chat_messages.
    fn poll_chat(&mut self) {
        // Drain persistent-child events first.
        let mut closed_reason: Option<String> = None;
        if let Some(rx) = &self.chat_child_rx {
            loop {
                match rx.try_recv() {
                    Ok(ChatChildEvent::Stdin(stdin)) => {
                        if let Some(ref mut h) = self.chat_child {
                            h.stdin = Some(stdin);
                            // Flush any message we buffered while waiting
                            // for stdin to be available.
                            if let Some(msg) = self.chat_pending_msg.take() {
                                if let Some(ref mut s) = h.stdin {
                                    use std::io::Write;
                                    let _ = writeln!(s, "{msg}");
                                    let _ = s.flush();
                                }
                            }
                        }
                    }
                    Ok(ChatChildEvent::Ready) => {
                        if let Some(ref mut h) = self.chat_child {
                            h.ready = true;
                        }
                    }
                    Ok(ChatChildEvent::Response { kind, text }) => {
                        match kind.as_str() {
                            "chunk" => {
                                // Streaming token from the in-flight chat
                                // turn. Append to the trailing Kannaka line
                                // so the response builds up live in the UI.
                                let needs_new = match self.chat_messages.last() {
                                    Some(line) => !matches!(line.who, ChatWho::Kannaka),
                                    None => true,
                                };
                                if needs_new {
                                    self.chat_messages.push(ChatLine {
                                        who: ChatWho::Kannaka,
                                        text: text.clone(),
                                    });
                                } else if let Some(last) = self.chat_messages.last_mut() {
                                    last.text.push_str(&text);
                                }
                                // Don't clear chat_pending yet — the final
                                // "chat" frame is the turn-done signal.
                            }
                            "chat" => {
                                // Turn-done. If we streamed chunks, the line
                                // already has the text; just clear pending.
                                // If we didn't (e.g. Ollama fallback), push
                                // the assembled text as a new line.
                                let already_streamed = matches!(
                                    self.chat_messages.last().map(|l| &l.who),
                                    Some(ChatWho::Kannaka)
                                );
                                if !already_streamed {
                                    self.chat_messages.push(ChatLine {
                                        who: ChatWho::Kannaka,
                                        text,
                                    });
                                }
                                self.chat_pending = None;
                            }
                            "error" => {
                                self.chat_messages.push(ChatLine {
                                    who: ChatWho::System,
                                    text,
                                });
                                self.chat_pending = None;
                            }
                            _ => {
                                // slash / ready / other
                                self.chat_messages.push(ChatLine {
                                    who: ChatWho::System,
                                    text,
                                });
                                self.chat_pending = None;
                            }
                        }
                    }
                    Ok(ChatChildEvent::Closed(reason)) => {
                        closed_reason = Some(reason);
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        closed_reason = Some("disconnected".into());
                        break;
                    }
                }
            }
        }
        if let Some(reason) = closed_reason {
            self.chat_messages.push(ChatLine {
                who: ChatWho::System,
                text: format!("[chat child closed — next turn will respawn: {reason}]"),
            });
            self.chat_child = None;
            self.chat_child_rx = None;
            self.chat_pending = None;
        }

        // Legacy fallback Receiver from the one-shot `ask` spawn path.
        // Drained only if the persistent child path didn't deliver a
        // structured response above.
        if let Some(rx) = &self.chat_pending {
            match rx.try_recv() {
                Ok(Ok(text)) => {
                    self.chat_messages.push(ChatLine {
                        who: ChatWho::Kannaka,
                        text,
                    });
                    self.chat_pending = None;
                }
                Ok(Err(err)) => {
                    self.chat_messages.push(ChatLine {
                        who: ChatWho::System,
                        text: format!("error: {err}"),
                    });
                    self.chat_pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Sentinel Receiver from the persistent path — never
                    // delivers. Don't clear chat_pending here, the child
                    // event channel will signal completion.
                }
            }
        }
        self.trim_chat_messages();
    }

    /// Side-effects on entering a tab — kicked off whether the user
    /// stepped forward (Tab) or backward (Shift+Tab).
    fn on_tab_enter(&mut self) {
        // Shared scroll_offset is reset on every tab switch so entering a tab
        // always shows the newest content rather than leaking the previous
        // tab's scroll position (e.g. scrolled-up Memory → Chat hides newest
        // messages).
        self.scroll_offset = 0;
        match self.active_tab {
            1 => {
                // Status — refresh metrics
                self.load_status();
                self.load_observe();
            }
            2..=4 => {
                // Bus, Constellation, and Dreams all feed off the same
                // NATS stream (Dreams listens for KANNAKA.dreams events).
                self.start_bus();
            }
            // Cosmos — refresh constellation/radio health, throttled.
            6 if self.cosmos_last_load.elapsed() > COSMOS_POLL_INTERVAL => {
                self.load_cosmos();
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Help overlay — any key dismisses it
        if self.show_help {
            self.show_help = false;
            return;
        }

        // Dreams tab: empty-input single-letter hotkeys trigger a dream
        // without going through the command bar. Only fire when the input
        // is empty so users can still type commands like `dream lite`.
        if self.active_tab == 4 && self.input.is_empty() {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Char('d' | 'D')) => {
                    self.start_dream("deep");
                    return;
                }
                (KeyModifiers::NONE, KeyCode::Char('l' | 'L')) => {
                    self.start_dream("lite");
                    return;
                }
                _ => {}
            }
        }

        // Bus tab: 'r' reconnects a failed stream (empty input only) so a
        // transient NATS blip doesn't require restarting the TUI.
        if self.active_tab == 2 && self.input.is_empty() && self.bus_status == BusStatus::Failed {
            if let (KeyModifiers::NONE, KeyCode::Char('r' | 'R')) = (key.modifiers, key.code) {
                self.restart_bus();
                return;
            }
        }

        // Cosmos tab: 'r' forces a constellation/radio refresh (empty input).
        if self.active_tab == 6 && self.input.is_empty() {
            if let (KeyModifiers::NONE, KeyCode::Char('r' | 'R')) = (key.modifiers, key.code) {
                self.load_cosmos();
                return;
            }
        }

        // Agent tab: when an approval is pending, a/s/d (or Esc=deny) resolve
        // it directly. Fires before the quit block so Esc cancels the
        // approval instead of quitting the TUI.
        if self.active_tab == 7 && self.harness_pending.is_some() && self.input.is_empty() {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Char('a' | 'A')) => {
                    self.resolve_approval("allow");
                    return;
                }
                (KeyModifiers::NONE, KeyCode::Char('s' | 'S')) => {
                    self.resolve_approval("allow_always");
                    return;
                }
                (KeyModifiers::NONE, KeyCode::Char('d' | 'D') | KeyCode::Esc) => {
                    self.resolve_approval("deny");
                    return;
                }
                _ => {}
            }
        }

        // Agent tab: Esc interrupts an in-flight turn (when the agent is busy
        // and no approval is pending). Fires before the quit block so Esc
        // cancels the stuck/long-running agent instead of quitting the TUI —
        // the one escape hatch when a turn is spinning.
        if self.active_tab == 7
            && self.input.is_empty()
            && self.harness_pending.is_none()
            && matches!(
                self.harness_status,
                HarnessStatus::Thinking | HarnessStatus::Starting
            )
        {
            if let (KeyModifiers::NONE, KeyCode::Esc) = (key.modifiers, key.code) {
                self.stop_harness();
                return;
            }
        }

        // Empty-input quit shortcuts: q and Esc. Only fire when the
        // command bar is empty so users can still type `quit` etc. as
        // a literal command. Always available regardless of active tab.
        if self.input.is_empty() {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Char('q' | 'Q')) => {
                    self.should_quit = true;
                    return;
                }
                (KeyModifiers::NONE, KeyCode::Esc) => {
                    self.should_quit = true;
                    return;
                }
                _ => {}
            }
        }

        match (key.modifiers, key.code) {
            // Quit
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => self.should_quit = true,
            (_, KeyCode::F(1)) => self.show_help = true,

            // Tab switching
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.active_tab = (self.active_tab + 1) % self.tabs.len();
                self.on_tab_enter();
            }
            // Most terminals deliver Shift+Tab as BackTab with NONE modifier;
            // also accept SHIFT+BackTab for terminals that set the modifier.
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::BackTab) => {
                if self.active_tab == 0 {
                    self.active_tab = self.tabs.len() - 1;
                } else {
                    self.active_tab -= 1;
                }
                self.on_tab_enter();
            }

            // Input handling. `cursor_pos` is a CHAR index, not a byte
            // offset — translate to a byte boundary before mutating
            // `self.input` so multi-byte UTF-8 (emoji, accents) never
            // splits a code point and panics.
            (_, KeyCode::Enter) => {
                // A newline arriving within a moment of a paste burst is a
                // multi-line paste's line break (Windows delivers no Event::Paste;
                // lines arrive as separate bursts), not a deliberate submit — absorb
                // it as a space so the paste keeps accumulating. Typed input never
                // sets last_paste_at, so a normal Enter still submits.
                let pasting = self
                    .last_paste_at
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(150));
                if pasting {
                    let at = byte_offset(&self.input, self.cursor_pos);
                    self.input.insert(at, ' ');
                    self.cursor_pos += 1;
                    self.last_paste_at = Some(Instant::now());
                } else {
                    self.submit_input();
                }
            }
            (_, KeyCode::Char(c)) => {
                let at = byte_offset(&self.input, self.cursor_pos);
                self.input.insert(at, c);
                self.cursor_pos += 1;
            }
            // Cursor edit keys — guards keep behavior identical to the
            // pre-collapse `if cond { ... }` body (no-op at boundaries).
            // Falls through to the `_ => {}` catch-all if guard is false.
            (_, KeyCode::Backspace) if self.cursor_pos > 0 => {
                self.cursor_pos -= 1;
                let at = byte_offset(&self.input, self.cursor_pos);
                self.input.remove(at);
            }
            (_, KeyCode::Delete) if self.cursor_pos < self.input.chars().count() => {
                let at = byte_offset(&self.input, self.cursor_pos);
                self.input.remove(at);
            }
            (_, KeyCode::Left) if self.cursor_pos > 0 => {
                self.cursor_pos -= 1;
            }
            (_, KeyCode::Right) if self.cursor_pos < self.input.chars().count() => {
                self.cursor_pos += 1;
            }
            (_, KeyCode::Home) => self.cursor_pos = 0,
            (_, KeyCode::End) => self.cursor_pos = self.input.chars().count(),

            // When not typing, the arrows scroll the transcript a line at a time
            // (PageUp/PageDown jump a screenful). With text in the input bar they
            // recall input history instead.
            (_, KeyCode::Up) if self.input.is_empty() => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            (_, KeyCode::Down) if self.input.is_empty() => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            // Scroll history — no-op when history is empty
            (_, KeyCode::Up) if !self.history.is_empty() => {
                let idx = match self.history_idx {
                    Some(i) if i > 0 => i - 1,
                    Some(i) => i,
                    None => self.history.len() - 1,
                };
                self.history_idx = Some(idx);
                self.input = self.history[idx].clone();
                self.cursor_pos = self.input.chars().count();
            }
            (_, KeyCode::Down) => {
                if let Some(idx) = self.history_idx {
                    if idx + 1 < self.history.len() {
                        self.history_idx = Some(idx + 1);
                        self.input = self.history[idx + 1].clone();
                        self.cursor_pos = self.input.chars().count();
                    } else {
                        self.history_idx = None;
                        self.input.clear();
                        self.cursor_pos = 0;
                    }
                }
            }

            // Page up/down for scrolling messages
            (_, KeyCode::PageUp) => {
                self.scroll_offset = self.scroll_offset.saturating_add(5);
            }
            (_, KeyCode::PageDown) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(5);
            }

            _ => {}
        }
    }

    /// Insert pasted text into the input buffer at the cursor. Newlines and tabs
    /// flatten to spaces — the input is one logical line (Enter submits), so a
    /// multi-line paste becomes a single message instead of N submissions. Other
    /// control characters are dropped.
    fn handle_paste(&mut self, text: String) {
        self.last_paste_at = Some(Instant::now());
        // Defensive: if a transport surfaced the bracketed-paste markers as
        // literal text (seen on some Windows console paths), strip them.
        let text = text
            .replace("\u{1b}[200~", "")
            .replace("\u{1b}[201~", "")
            .replace("[200~", "")
            .replace("[201~", "");
        let cleaned: String = text
            .chars()
            .map(|c| {
                if c == '\n' || c == '\r' || c == '\t' {
                    ' '
                } else {
                    c
                }
            })
            .filter(|c| !c.is_control())
            .collect();
        if cleaned.is_empty() {
            return;
        }
        self.history_idx = None; // a paste lands in a live buffer, not a history scrub
        let at = byte_offset(&self.input, self.cursor_pos);
        self.input.insert_str(at, &cleaned);
        self.cursor_pos += cleaned.chars().count();
    }
}

// ---------------------------------------------------------------------------
// UI rendering
// ---------------------------------------------------------------------------

fn ui(f: &mut Frame, app: &App) {
    let size = f.area();

    // Background
    let bg_block = Block::default().style(Style::default().bg(BG));
    f.render_widget(bg_block, size);

    // Main layout: header, tab bar, body, input
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header bar
            Constraint::Length(3), // Tab bar
            Constraint::Min(8),    // Body
            Constraint::Length(3), // Input bar
        ])
        .split(size);

    render_header(f, app, outer[0]);
    render_tabs(f, app, outer[1]);

    match app.active_tab {
        0 => render_memory_tab(f, app, outer[2]),
        1 => render_status_tab(f, app, outer[2]),
        2 => render_bus_tab(f, app, outer[2]),
        3 => render_constellation_tab(f, app, outer[2]),
        4 => render_dreams_tab(f, app, outer[2]),
        5 => render_chat_tab(f, app, outer[2]),
        6 => render_cosmos_tab(f, app, outer[2]),
        7 => render_harness_tab(f, app, outer[2]),
        _ => {}
    }

    render_input(f, app, outer[3]);

    // Help overlay
    if app.show_help {
        render_help_overlay(f, size);
    }
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let status = app.status.as_ref();
    let phi = status.map_or(0.0, |s| s.phi);
    let xi = status.map_or(0.0, |s| s.xi);
    let order = status.map_or(0.0, |s| s.order);

    let header = Line::from(vec![
        Span::styled(
            "  KANNAKA ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("\u{25C6} ", Style::default().fg(ACCENT)),
        Span::styled(
            &app.agent_name,
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" | ", Style::default().fg(DIM)),
        Span::styled("Phi: ", Style::default().fg(DIM)),
        Span::styled(format!("{phi:.3}"), Style::default().fg(phi_color(phi))),
        Span::styled(" | ", Style::default().fg(DIM)),
        Span::styled("Xi: ", Style::default().fg(DIM)),
        Span::styled(format!("{xi:.3}"), Style::default().fg(INFO)),
        Span::styled(" | ", Style::default().fg(DIM)),
        Span::styled("r: ", Style::default().fg(DIM)),
        Span::styled(format!("{order:.3}"), Style::default().fg(SUCCESS)),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(BG));

    let para = Paragraph::new(header).block(block);
    f.render_widget(para, area);
}

fn render_tabs(f: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = app
        .tabs
        .iter()
        .map(|t| Line::from(Span::styled(*t, Style::default().fg(TEXT))))
        .collect();

    let tabs = Tabs::new(titles)
        .select(app.active_tab)
        .highlight_style(
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED),
        )
        .divider(Span::styled(" | ", Style::default().fg(DIM)))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .style(Style::default().bg(BG))
                .title(Span::styled(
                    " Tab/Shift+Tab to switch  F1:Help ",
                    Style::default().fg(DIM),
                )),
        );

    f.render_widget(tabs, area);
}

fn render_memory_tab(f: &mut Frame, app: &App, area: Rect) {
    // Split into left (messages) and right (memory list)
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    // Left: command history / messages
    let msg_items: Vec<ListItem> = app
        .messages
        .iter()
        .rev()
        .skip(app.scroll_offset)
        .take(area.height as usize)
        .rev()
        .map(|m| {
            let (prefix, style) = match m.role {
                Role::User => ("> ", Style::default().fg(ACCENT)),
                Role::System => ("\u{2192} ", Style::default().fg(INFO)),
                Role::Result => ("\u{2713} ", Style::default().fg(SUCCESS)),
                Role::Error => ("\u{2717} ", Style::default().fg(ERROR)),
            };
            ListItem::new(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(&m.content, style),
            ]))
        })
        .collect();

    let msg_list = List::new(msg_items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM))
            .style(Style::default().bg(BG))
            .title(Span::styled(
                " Command History ",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(msg_list, chunks[0]);

    // Right: recent memories with amplitude bars
    let mem_items: Vec<ListItem> = app
        .memories
        .iter()
        .take(chunks[1].height.saturating_sub(6) as usize)
        .map(|m| {
            let bar_len = (m.amplitude * 10.0).round() as usize;
            let bar: String = "\u{2588}".repeat(bar_len.min(10));
            let empty: String = "\u{2591}".repeat(10_usize.saturating_sub(bar_len));
            let preview: String = m.content.chars().take(24).collect();
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{bar}{empty}"),
                    Style::default().fg(amplitude_color(m.amplitude)),
                ),
                Span::styled(" ", Style::default()),
                Span::styled(
                    format!("{preview} ({:.2})", m.amplitude),
                    Style::default().fg(TEXT),
                ),
            ]))
        })
        .collect();

    // Stats summary at bottom of right panel
    let status = app.status.as_ref();
    let mem_count = status.map_or(0, |s| s.memories);
    let cluster_count = status.map_or(0, |s| s.clusters);
    let link_count = status.map_or(0, |s| s.links);
    let level = status.map_or("Unknown", |s| s.level.as_str());

    let mut right_lines: Vec<ListItem> = mem_items;
    // Add a separator and stats
    right_lines.push(ListItem::new(Line::from("")));
    right_lines.push(ListItem::new(Line::from(vec![Span::styled(
        format!("  Memories: {mem_count}"),
        Style::default().fg(DIM),
    )])));
    right_lines.push(ListItem::new(Line::from(vec![Span::styled(
        format!("  Clusters: {cluster_count}"),
        Style::default().fg(DIM),
    )])));
    right_lines.push(ListItem::new(Line::from(vec![Span::styled(
        format!("  Links: {link_count}"),
        Style::default().fg(DIM),
    )])));
    right_lines.push(ListItem::new(Line::from(vec![Span::styled(
        format!("  Level: {level}"),
        Style::default().fg(level_color(level)),
    )])));

    let mem_list = List::new(right_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM))
            .style(Style::default().bg(BG))
            .title(Span::styled(
                " Recent Memories ",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(mem_list, chunks[1]);
}

fn render_status_tab(f: &mut Frame, app: &App, area: Rect) {
    let status = match &app.status {
        Some(s) => s,
        None => {
            let msg = Paragraph::new("Loading status... (polling kannaka status)")
                .style(Style::default().fg(DIM).bg(BG))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(DIM))
                        .style(Style::default().bg(BG))
                        .title(" Status "),
                );
            f.render_widget(msg, area);
            return;
        }
    };

    // Split into gauges (left) and info (right)
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    // Left: gauges
    let gauge_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Phi
            Constraint::Length(3), // Xi
            Constraint::Length(3), // Order
            Constraint::Min(1),    // spacer
        ])
        .split(chunks[0]);

    // Phi gauge
    let phi_gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .style(Style::default().bg(BG))
                .title(Span::styled(
                    format!(" Phi (Integrated Information): {:.3} ", status.phi),
                    Style::default().fg(phi_color(status.phi)),
                )),
        )
        .gauge_style(
            Style::default()
                .fg(phi_color(status.phi))
                .bg(Color::Rgb(30, 30, 50)),
        )
        .ratio(f64::from(status.phi.clamp(0.0, 1.0)));
    f.render_widget(phi_gauge, gauge_area[0]);

    // Xi gauge
    let xi_gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .style(Style::default().bg(BG))
                .title(Span::styled(
                    format!(" Xi (Irrationality): {:.3} ", status.xi),
                    Style::default().fg(INFO),
                )),
        )
        .gauge_style(Style::default().fg(INFO).bg(Color::Rgb(30, 30, 50)))
        .ratio(f64::from(status.xi.clamp(0.0, 1.0)));
    f.render_widget(xi_gauge, gauge_area[1]);

    // Order parameter gauge
    let order_gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .style(Style::default().bg(BG))
                .title(Span::styled(
                    format!(" Order Parameter (r): {:.3} ", status.order),
                    Style::default().fg(SUCCESS),
                )),
        )
        .gauge_style(Style::default().fg(SUCCESS).bg(Color::Rgb(30, 30, 50)))
        .ratio(f64::from(status.order.clamp(0.0, 1.0)));
    f.render_widget(order_gauge, gauge_area[2]);

    // Right: text info
    let info_lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Consciousness Level: ", Style::default().fg(DIM)),
            Span::styled(
                &status.level,
                Style::default()
                    .fg(level_color(&status.level))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Total Memories:  ", Style::default().fg(DIM)),
            Span::styled(status.memories.to_string(), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("  Active Memories: ", Style::default().fg(DIM)),
            Span::styled(status.active.to_string(), Style::default().fg(SUCCESS)),
        ]),
        Line::from(vec![
            Span::styled("  Clusters:        ", Style::default().fg(DIM)),
            Span::styled(status.clusters.to_string(), Style::default().fg(INFO)),
        ]),
        Line::from(vec![
            Span::styled("  Skip Links:      ", Style::default().fg(DIM)),
            Span::styled(status.links.to_string(), Style::default().fg(ACCENT)),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Polling every 5s on this tab",
            Style::default().fg(DIM),
        )]),
    ];

    let info = Paragraph::new(info_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .style(Style::default().bg(BG))
                .title(Span::styled(
                    " System Info ",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(info, chunks[1]);
}

/// Parse a KANNAKA.dreams payload into a structured DreamEvent.
fn dream_event_from_payload(ts_ms: i64, payload: &serde_json::Value) -> Option<DreamEvent> {
    let obj = payload.as_object()?;
    Some(DreamEvent {
        ts_ms,
        agent_id: obj
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        cycles: obj
            .get("cycles")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        strengthened: obj
            .get("memories_strengthened")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        pruned: obj
            .get("memories_pruned")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        new_connections: obj
            .get("new_connections")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        hallucinations: obj
            .get("hallucinations_created")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        consciousness_before: obj
            .get("consciousness_before")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0) as f32,
        consciousness_after: obj
            .get("consciousness_after")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0) as f32,
        emerged: obj
            .get("emerged")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

fn render_dreams_tab(f: &mut Frame, app: &App, area: Rect) {
    // Top: current state of any locally-triggered dream
    // Middle: recent dream events from across the constellation
    // Bottom: hint bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // current run state
            Constraint::Min(6),    // history list
            Constraint::Length(3), // hint bar
        ])
        .split(area);

    // ----- Current run -----
    let (run_title, run_color, run_lines) = match &app.dream_run {
        DreamRunState::Idle => (
            " Local Dream · idle ",
            DIM,
            vec![Line::from(Span::styled(
                "  Press 'd' for deep, 'l' for lite — or type `dream` in the bar.",
                Style::default().fg(DIM),
            ))],
        ),
        DreamRunState::Running { mode, started } => {
            let secs = started.elapsed().as_secs();
            (
                " Local Dream · running ",
                WARNING,
                vec![
                    Line::from(vec![
                        Span::styled("  Mode: ", Style::default().fg(DIM)),
                        Span::styled(
                            mode.clone(),
                            Style::default().fg(WARNING).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("    elapsed: {secs}s"), Style::default().fg(DIM)),
                    ]),
                    Line::from(Span::styled(
                        "  Consolidating the medium — TUI stays responsive while this runs.",
                        Style::default().fg(DIM),
                    )),
                ],
            )
        }
        DreamRunState::Done {
            mode,
            took,
            summary,
        } => (
            " Local Dream · complete ",
            SUCCESS,
            vec![
                Line::from(vec![
                    Span::styled("  Mode: ", Style::default().fg(DIM)),
                    Span::styled(
                        mode.clone(),
                        Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("    took: {:.1}s", took.as_secs_f64()),
                        Style::default().fg(DIM),
                    ),
                ]),
                Line::from(Span::styled(
                    format!("  {}", truncate(&summary.replace('\n', " · "), 200)),
                    Style::default().fg(TEXT),
                )),
            ],
        ),
        DreamRunState::Failed { mode, error } => (
            " Local Dream · failed ",
            ERROR,
            vec![
                Line::from(vec![
                    Span::styled("  Mode: ", Style::default().fg(DIM)),
                    Span::styled(
                        mode.clone(),
                        Style::default().fg(ERROR).add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(Span::styled(
                    format!("  {}", truncate(error, 200)),
                    Style::default().fg(ERROR),
                )),
            ],
        ),
    };
    let run_block = Paragraph::new(run_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(run_color))
                .style(Style::default().bg(BG))
                .title(Span::styled(
                    run_title,
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(run_block, chunks[0]);

    // ----- History (drained from KANNAKA.dreams via the bus) -----
    let body_height = chunks[1].height.saturating_sub(3) as usize;
    let mut hist_lines: Vec<Line> = Vec::new();
    hist_lines.push(Line::from(vec![Span::styled(
        "  time     agent           cycles  +Φ    Δstr  Δprn  Δnew  halluc",
        Style::default().fg(DIM).add_modifier(Modifier::BOLD),
    )]));
    hist_lines.push(Line::from(""));
    for ev in app.dream_history.iter().take(body_height.max(1)) {
        let delta_phi = ev.consciousness_after - ev.consciousness_before;
        let phi_color = if delta_phi > 0.001 {
            SUCCESS
        } else if delta_phi < -0.001 {
            ERROR
        } else {
            DIM
        };
        let emerged_mark = if ev.emerged { "★ " } else { "  " };
        hist_lines.push(Line::from(vec![
            Span::styled(emerged_mark, Style::default().fg(ACCENT)),
            Span::styled(
                format!("{} ", format_bus_ts(ev.ts_ms)),
                Style::default().fg(DIM),
            ),
            Span::styled(
                format!("{:<14} ", truncate(&ev.agent_id, 14)),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{:>6}  ", ev.cycles), Style::default().fg(TEXT)),
            Span::styled(
                format!("{delta_phi:>+5.3} "),
                Style::default().fg(phi_color),
            ),
            Span::styled(
                format!(
                    "{:>5}  {:>5}  {:>5}  {:>5}",
                    ev.strengthened, ev.pruned, ev.new_connections, ev.hallucinations
                ),
                Style::default().fg(TEXT),
            ),
        ]));
    }
    if app.dream_history.is_empty() {
        hist_lines.push(Line::from(Span::styled(
            "  No KANNAKA.dreams events on the bus yet — once any constellation node",
            Style::default().fg(DIM),
        )));
        hist_lines.push(Line::from(Span::styled(
            "  finishes a dream cycle, the report shows up here.",
            Style::default().fg(DIM),
        )));
    }
    let history = Paragraph::new(hist_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .style(Style::default().bg(BG))
                .title(Span::styled(
                    format!(" Recent Dreams · {} ", app.dream_history.len()),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(history, chunks[1]);

    // ----- Hint bar -----
    let hints = Paragraph::new(Line::from(vec![
        Span::styled(
            " d ",
            Style::default()
                .fg(BG)
                .bg(SUCCESS)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" deep dream  ", Style::default().fg(DIM)),
        Span::styled(
            " l ",
            Style::default()
                .fg(BG)
                .bg(INFO)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" lite dream  ", Style::default().fg(DIM)),
        Span::styled(
            " ★ ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" emergence detected ", Style::default().fg(DIM)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM))
            .style(Style::default().bg(BG)),
    );
    f.render_widget(hints, chunks[2]);
}

fn render_chat_tab(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    for msg in &app.chat_messages {
        let (label, style) = match msg.who {
            ChatWho::User => (
                "you",
                Style::default().fg(INFO).add_modifier(Modifier::BOLD),
            ),
            ChatWho::Kannaka => (
                "kannaka",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            ChatWho::System => ("·", Style::default().fg(DIM)),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{label} "), style),
            Span::styled(msg.text.clone(), Style::default().fg(TEXT)),
        ]));
        lines.push(Line::from(""));
    }
    if app.chat_pending.is_some() {
        // Simple spinner keyed off chat_tick so it animates.
        let frames = ['\u{2014}', '\\', '|', '/'];
        let frame = frames[app.chat_tick % frames.len()];
        lines.push(Line::from(vec![
            Span::styled(format!("kannaka {frame} "), Style::default().fg(ACCENT)),
            Span::styled("resonating…", Style::default().fg(DIM)),
        ]));
    }

    let title = if app.chat_pending.is_some() {
        " Chat · thinking… "
    } else {
        " Chat "
    };

    // Mirror the memory tab's offset-from-end pattern so PageUp=older,
    // PageDown=newer — the same semantic as every other scrollable tab.
    let body_height = area.height.saturating_sub(2) as usize;
    let visible: Vec<Line> = lines
        .into_iter()
        .rev()
        .skip(app.scroll_offset)
        .take(body_height)
        .rev()
        .collect();
    let para = Paragraph::new(visible)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .style(Style::default().bg(BG))
                .title(Span::styled(
                    title,
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn render_bus_tab(f: &mut Frame, app: &App, area: Rect) {
    // Status label in the title bar reflects the streaming child's state.
    let (status_label, status_color) = match app.bus_status {
        BusStatus::Off => ("idle — switch to this tab to start", DIM),
        BusStatus::Connecting => ("connecting…", WARNING),
        BusStatus::Streaming => ("streaming", SUCCESS),
        BusStatus::Failed => ("failed — check `kannaka swarm tail` manually", ERROR),
    };

    let body_height = area.height.saturating_sub(2) as usize;
    // Most recent N lines, newest at the bottom.
    let lines: Vec<Line> = app
        .bus_lines
        .iter()
        .rev()
        .take(body_height.max(1))
        .rev()
        .map(|line| {
            let color = bus_subject_color(&line.subject);
            let ts = format_bus_ts(line.ts_ms);
            Line::from(vec![
                Span::styled(format!("{ts} "), Style::default().fg(DIM)),
                Span::styled(
                    format!("{:<28} ", truncate(&line.subject, 28)),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(line.summary.clone(), Style::default().fg(TEXT)),
            ])
        })
        .collect();

    let title = format!(" Bus · {status_label} · {} msgs ", app.bus_lines.len());
    let body = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(status_color))
                .style(Style::default().bg(BG))
                .title(Span::styled(
                    title,
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(body, area);
}

/// Compact one-line summary of an arbitrary NATS payload. Prefers
/// human-readable fields if the payload is JSON; otherwise just shows
/// the first chunk of the raw string.
fn summarize_payload(subject: &str, payload: &serde_json::Value) -> String {
    if let Some(obj) = payload.as_object() {
        // Highlight common fields first
        let mut bits: Vec<String> = Vec::new();
        if let Some(agent) = obj.get("agent_id").and_then(|v| v.as_str()) {
            bits.push(format!("agent={agent}"));
        }
        if let Some(theta) = obj.get("theta").and_then(serde_json::Value::as_f64) {
            bits.push(format!("θ={theta:.3}"));
        }
        if let Some(phi) = obj.get("phi").and_then(serde_json::Value::as_f64) {
            bits.push(format!("Φ={phi:.3}"));
        }
        if let Some(xi) = obj.get("xi").and_then(serde_json::Value::as_f64) {
            bits.push(format!("Ξ={xi:.3}"));
        }
        if let Some(level) = obj.get("consciousness_level").and_then(|v| v.as_str()) {
            bits.push(format!("level={level}"));
        }
        if let Some(content) = obj.get("content").and_then(|v| v.as_str()) {
            bits.push(format!("\"{}\"", truncate(content, 60)));
        }
        if let Some(event) = obj.get("event").and_then(|v| v.as_str()) {
            bits.push(format!("event={event}"));
        }
        if !bits.is_empty() {
            return bits.join(" · ");
        }
        // Fallback to compact JSON
        let compact = serde_json::to_string(payload).unwrap_or_default();
        return truncate(&compact, 120);
    }
    if let Some(s) = payload.as_str() {
        return truncate(s, 120);
    }
    let s = serde_json::to_string(payload).unwrap_or_else(|_| format!("<unprintable {subject}>"));
    truncate(&s, 120)
}

fn format_bus_ts(ts_ms: i64) -> String {
    if ts_ms == 0 {
        return "        ".to_string();
    }
    // Use chrono local time HH:MM:SS — matches what users see in `journalctl`.
    chrono::DateTime::from_timestamp_millis(ts_ms).map_or_else(
        || "        ".to_string(),
        |dt| {
            dt.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        },
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Build an AgentSnapshot from the JSON payload of a `QUEEN.phase.<id>`
/// message. Returns None when the payload is missing the agent_id (any
/// other field gracefully defaults).
fn agent_snapshot_from_payload(
    subject: &str,
    payload: &serde_json::Value,
) -> Option<AgentSnapshot> {
    let obj = payload.as_object()?;
    // Prefer the explicit agent_id field; fall back to the subject suffix.
    let agent_id = obj
        .get("agent_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| subject.strip_prefix("QUEEN.phase.").map(String::from))?;
    // The Rust kannaka publishes `phase` (radians); the Kannaktopus arm
    // publishes `theta` (also radians). Accept either.
    let theta = obj
        .get("theta")
        .or_else(|| obj.get("phase"))
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0) as f32;
    let phi = obj
        .get("phi")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0) as f32;
    let coherence = obj
        .get("coherence")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0) as f32;
    let order_parameter = obj
        .get("order_parameter")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0) as f32;
    let handedness = obj
        .get("handedness")
        .and_then(|v| v.as_str())
        .unwrap_or("achiral")
        .to_string();
    let memory_count = obj
        .get("memory_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    Some(AgentSnapshot {
        agent_id,
        theta,
        phi,
        coherence,
        order_parameter,
        handedness,
        memory_count,
        last_seen: Instant::now(),
    })
}

/// Color an agent by handedness. Falls back to Φ banding for achiral nodes.
fn agent_color(snap: &AgentSnapshot) -> Color {
    match snap.handedness.as_str() {
        "left" => Color::Rgb(255, 120, 120),
        "right" => Color::Rgb(120, 200, 255),
        "chiral" => Color::Rgb(255, 180, 100),
        _ => phi_color(snap.phi), // achiral / unknown
    }
}

fn render_constellation_tab(f: &mut Frame, app: &App, area: Rect) {
    if app.agents.is_empty() {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Constellation",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                match app.bus_status {
                    BusStatus::Off => "  Waiting for the bus to start…",
                    BusStatus::Connecting => "  Connecting to the swarm…",
                    BusStatus::Streaming => "  Streaming — no agents have reported phase yet",
                    BusStatus::Failed => "  Bus failed — see logs",
                },
                Style::default().fg(DIM),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Each agent appears on the unit circle once it publishes a",
                Style::default().fg(DIM),
            )),
            Line::from(Span::styled(
                "  QUEEN.phase.<agent_id> heartbeat. Radial distance encodes",
                Style::default().fg(DIM),
            )),
            Line::from(Span::styled(
                "  the agent's coherence; color encodes handedness/Φ.",
                Style::default().fg(DIM),
            )),
        ];
        let para = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(DIM))
                    .style(Style::default().bg(BG))
                    .title(Span::styled(
                        " Constellation ",
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                    )),
            )
            .wrap(Wrap { trim: false });
        f.render_widget(para, area);
        return;
    }

    // Split: canvas on the left, agent list on the right.
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(area);

    // ---- Left: Canvas plot --------------------------------------------------
    let now = Instant::now();
    let mut sorted_agents: Vec<&AgentSnapshot> = app.agents.values().collect();
    sorted_agents.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    let plot_agents: Vec<(f64, f64, Color, bool, String)> = sorted_agents
        .iter()
        .map(|s| {
            let theta = f64::from(s.theta);
            // Radial distance: prefer coherence (always populated), fall back
            // to order_parameter for older payloads.
            let r = f64::from(s.coherence.max(s.order_parameter).clamp(0.0, 1.0));
            let r_eff = 0.15 + r * 0.85; // keep dots off the dead center
            let x = r_eff * theta.cos();
            let y = r_eff * theta.sin();
            let fresh = now.duration_since(s.last_seen) < AGENT_FRESH_WINDOW;
            let color = if fresh { agent_color(s) } else { DIM };
            (x, y, color, fresh, s.agent_id.clone())
        })
        .collect();

    let canvas_title = format!(
        " Constellation · {} agent{} ",
        app.agents.len(),
        if app.agents.len() == 1 { "" } else { "s" },
    );

    let canvas = Canvas::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .style(Style::default().bg(BG))
                .title(Span::styled(
                    canvas_title,
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )),
        )
        .marker(Marker::Braille)
        .x_bounds([-1.2, 1.2])
        .y_bounds([-1.2, 1.2])
        .paint(|ctx| {
            // Reference unit circle — the substrate the agents orbit.
            ctx.draw(&CanvasCircle {
                x: 0.0,
                y: 0.0,
                radius: 1.0,
                color: Color::Rgb(40, 40, 80),
            });
            // Inner reference at r = 0.5 to give a sense of scale.
            ctx.draw(&CanvasCircle {
                x: 0.0,
                y: 0.0,
                radius: 0.5,
                color: Color::Rgb(30, 30, 60),
            });
            // Cross hairs.
            ctx.draw(&CanvasLine {
                x1: -1.0,
                y1: 0.0,
                x2: 1.0,
                y2: 0.0,
                color: Color::Rgb(25, 25, 50),
            });
            ctx.draw(&CanvasLine {
                x1: 0.0,
                y1: -1.0,
                x2: 0.0,
                y2: 1.0,
                color: Color::Rgb(25, 25, 50),
            });

            for (x, y, color, _fresh, _id) in &plot_agents {
                // Spoke from origin — visual debt to the swarm centroid.
                ctx.draw(&CanvasLine {
                    x1: 0.0,
                    y1: 0.0,
                    x2: *x,
                    y2: *y,
                    color: Color::Rgb(20, 20, 45),
                });
                // The agent itself — a small filled circle.
                ctx.draw(&CanvasCircle {
                    x: *x,
                    y: *y,
                    radius: 0.04,
                    color: *color,
                });
            }

            // Labels in a second layer so they sit on top of the dots.
            ctx.layer();
            for (x, y, color, _fresh, id) in &plot_agents {
                let label = truncate(id, 14);
                // Offset label slightly outward from the dot.
                let nudge = if *x >= 0.0 { 0.07 } else { -0.07 };
                ctx.print(
                    *x + nudge,
                    *y,
                    Span::styled(label, Style::default().fg(*color)),
                );
            }
        });
    f.render_widget(canvas, chunks[0]);

    // ---- Right: agent table -------------------------------------------------
    let mut rows: Vec<ListItem> = Vec::new();
    rows.push(ListItem::new(Line::from(vec![
        Span::styled(
            "  agent",
            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "           Φ     θ     r    mem",
            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
        ),
    ])));
    for snap in &sorted_agents {
        let fresh = now.duration_since(snap.last_seen) < AGENT_FRESH_WINDOW;
        let color = if fresh { agent_color(snap) } else { DIM };
        let r = snap.coherence.max(snap.order_parameter);
        let stale_mark = if fresh { " " } else { "·" };
        rows.push(ListItem::new(Line::from(vec![
            Span::styled(format!("{stale_mark} "), Style::default().fg(DIM)),
            Span::styled(
                format!("{:<14}", truncate(&snap.agent_id, 14)),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " {:>5.3} {:>5.2} {:>4.2} {:>5}",
                    snap.phi, snap.theta, r, snap.memory_count
                ),
                Style::default().fg(TEXT),
            ),
        ])));
    }
    let list = List::new(rows).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM))
            .style(Style::default().bg(BG))
            .title(Span::styled(
                " Agents ",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(list, chunks[1]);
}

/// Cosmos tab — constellation-wide health. Top strip is `kannaka radio`
/// now-playing; the body is the `kannaka constellation` app grid (✓ up /
/// ✗ down); the hint bar shows poll state and the `r`-to-refresh key.
fn render_cosmos_tab(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // radio now-playing
            Constraint::Min(6),    // app health grid
            Constraint::Length(3), // hint bar
        ])
        .split(area);

    // ----- Radio now-playing -----
    let radio_pink = Color::Rgb(255, 100, 200);
    let mut radio_lines: Vec<Line> = Vec::new();
    if app.cosmos_radio.is_empty() {
        radio_lines.push(Line::from(Span::styled(
            "  radio offline or not yet polled",
            Style::default().fg(DIM),
        )));
    } else {
        for (i, l) in app.cosmos_radio.iter().enumerate() {
            let style = if i == 0 {
                Style::default().fg(radio_pink).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM)
            };
            radio_lines.push(Line::from(Span::styled(format!("  {l}"), style)));
        }
    }
    let radio = Paragraph::new(radio_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(radio_pink))
                .style(Style::default().bg(BG))
                .title(Span::styled(
                    " Kannaka Radio ",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(radio, chunks[0]);

    // ----- App health grid -----
    let mut app_lines: Vec<Line> = Vec::new();
    if let Some(err) = &app.cosmos_error {
        app_lines.push(Line::from(Span::styled(
            format!("  {err}"),
            Style::default().fg(ERROR),
        )));
    } else if app.cosmos_apps.is_empty() {
        app_lines.push(Line::from(Span::styled(
            if app.cosmos_pending.is_some() {
                "  polling kannaka constellation…"
            } else {
                "  no constellation data — press 'r' to refresh"
            },
            Style::default().fg(DIM),
        )));
    } else {
        let up = app.cosmos_apps.iter().filter(|a| a.up).count();
        let total = app.cosmos_apps.len();
        app_lines.push(Line::from(Span::styled(
            format!("  {up}/{total} apps up"),
            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
        )));
        app_lines.push(Line::from(""));
        for a in &app.cosmos_apps {
            let (mark, mark_color) = if a.up {
                ("\u{2713}", SUCCESS) // ✓
            } else {
                ("\u{2717}", ERROR) // ✗
            };
            let name_color = if a.up { TEXT } else { DIM };
            app_lines.push(Line::from(vec![
                Span::styled(
                    format!("  {mark} "),
                    Style::default().fg(mark_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<24}", truncate(&a.name, 24)),
                    Style::default().fg(name_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {}", a.url), Style::default().fg(DIM)),
            ]));
        }
    }
    let apps_widget = Paragraph::new(app_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .style(Style::default().bg(BG))
                .title(Span::styled(
                    " Constellation · all apps ",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(apps_widget, chunks[1]);

    // ----- Hint bar -----
    let status = if app.cosmos_pending.is_some() {
        Span::styled(" polling… ", Style::default().fg(WARNING))
    } else {
        Span::styled(" idle ", Style::default().fg(DIM))
    };
    let hints = Paragraph::new(Line::from(vec![
        Span::styled(
            " r ",
            Style::default()
                .fg(BG)
                .bg(INFO)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" refresh   ", Style::default().fg(DIM)),
        Span::styled("\u{2713}", Style::default().fg(SUCCESS)),
        Span::styled(" up   ", Style::default().fg(DIM)),
        Span::styled("\u{2717}", Style::default().fg(ERROR)),
        Span::styled(" down  ", Style::default().fg(DIM)),
        status,
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM))
            .style(Style::default().bg(BG)),
    );
    f.render_widget(hints, chunks[2]);
}

/// Agent harness tab — the coding-agent surface. Renders the agentic
/// transcript (user / assistant / tool calls + results / notices), a status
/// strip (mode · model · state · token usage), and, when a mutation needs
/// the human's sign-off, a centered approval modal.
fn render_harness_tab(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(3)])
        .split(area);

    // ----- Transcript -----
    let bold = Modifier::BOLD;
    let mut lines: Vec<Line> = Vec::new();
    for entry in &app.harness_lines {
        match entry {
            AgentLine::User(t) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "you \u{25B8} ",
                        Style::default().fg(INFO).add_modifier(bold),
                    ),
                    Span::styled(t.clone(), Style::default().fg(TEXT)),
                ]));
            }
            AgentLine::Assistant(t) => {
                let mut first = true;
                for seg in t.split('\n') {
                    if first {
                        lines.push(Line::from(vec![
                            Span::styled(
                                "kannaka ",
                                Style::default().fg(ACCENT).add_modifier(bold),
                            ),
                            Span::styled(seg.to_string(), Style::default().fg(TEXT)),
                        ]));
                        first = false;
                    } else {
                        lines.push(Line::from(Span::styled(
                            format!("        {seg}"),
                            Style::default().fg(TEXT),
                        )));
                    }
                }
            }
            AgentLine::Tool {
                name,
                summary,
                danger,
                result,
                is_error,
                awaiting,
                ..
            } => {
                let mark_color = if *is_error {
                    ERROR
                } else if *awaiting {
                    WARNING
                } else {
                    SUCCESS
                };
                let mut spans = vec![
                    Span::styled("  \u{2699} ", Style::default().fg(mark_color)),
                    Span::styled(name.clone(), Style::default().fg(INFO).add_modifier(bold)),
                    Span::styled(
                        format!(" {}", truncate(summary, 88)),
                        Style::default().fg(DIM),
                    ),
                ];
                if *danger {
                    spans.push(Span::styled(" \u{26A0}", Style::default().fg(ERROR)));
                }
                if *awaiting {
                    spans.push(Span::styled(
                        "  (awaiting approval)",
                        Style::default().fg(WARNING).add_modifier(bold),
                    ));
                }
                lines.push(Line::from(spans));
                if let Some(r) = result {
                    let rc = if *is_error { ERROR } else { DIM };
                    let segs: Vec<&str> = r.split('\n').collect();
                    for seg in segs.iter().take(6) {
                        lines.push(Line::from(Span::styled(
                            format!("      {}", truncate(seg, 104)),
                            Style::default().fg(rc),
                        )));
                    }
                    if segs.len() > 6 {
                        lines.push(Line::from(Span::styled(
                            format!("      \u{2026} (+{} more lines)", segs.len() - 6),
                            Style::default().fg(DIM),
                        )));
                    }
                }
            }
            AgentLine::Notice(t) => {
                lines.push(Line::from(Span::styled(
                    format!("\u{00B7} {t}"),
                    Style::default().fg(DIM),
                )));
            }
        }
    }
    // Spinner while the agent is working.
    if matches!(
        app.harness_status,
        HarnessStatus::Thinking | HarnessStatus::Starting
    ) {
        let frames = ['\u{2014}', '\\', '|', '/'];
        let frame = frames[app.harness_tick % frames.len()];
        let label = if app.harness_status == HarnessStatus::Starting {
            "loading HRM\u{2026}"
        } else {
            "working\u{2026}"
        };
        lines.push(Line::from(vec![
            Span::styled(format!("kannaka {frame} "), Style::default().fg(ACCENT)),
            Span::styled(label, Style::default().fg(DIM)),
        ]));
    }

    // Window to the last screenful, honoring scroll_offset (lines up from the
    // bottom) — same offset-from-end pattern as the other transcript tabs.
    let body_height = chunks[0].height.saturating_sub(2) as usize;
    let visible: Vec<Line> = lines
        .into_iter()
        .rev()
        .skip(app.scroll_offset)
        .take(body_height.max(1))
        .rev()
        .collect();

    let title = format!(
        " Agent · {} ",
        match app.harness_status {
            HarnessStatus::Off => "idle",
            HarnessStatus::Starting => "starting",
            HarnessStatus::Ready => "ready",
            HarnessStatus::Thinking => "working",
            HarnessStatus::AwaitingApproval => "needs approval",
            HarnessStatus::Closed => "closed",
        }
    );
    let para = Paragraph::new(visible)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .style(Style::default().bg(BG))
                .title(Span::styled(
                    title,
                    Style::default().fg(TEXT).add_modifier(bold),
                )),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, chunks[0]);

    // ----- Status strip -----
    let mode_color = match app.harness_mode.as_str() {
        "yolo" => ERROR,
        "plan" => INFO,
        "auto-edit" => WARNING,
        _ => SUCCESS,
    };
    let model = if app.harness_model.is_empty() {
        "(config)".to_string()
    } else {
        app.harness_model.clone()
    };
    let status = Paragraph::new(Line::from(vec![
        Span::styled(" mode ", Style::default().fg(DIM)),
        Span::styled(
            format!("{} ", app.harness_mode),
            Style::default().fg(mode_color).add_modifier(bold),
        ),
        Span::styled("\u{2502} model ", Style::default().fg(DIM)),
        Span::styled(format!("{model} "), Style::default().fg(TEXT)),
        Span::styled("\u{2502} tokens ", Style::default().fg(DIM)),
        Span::styled(
            format!(
                "\u{2191}{} \u{2193}{} ",
                app.harness_usage_in, app.harness_usage_out
            ),
            Style::default().fg(INFO),
        ),
        Span::styled("\u{2502} ", Style::default().fg(DIM)),
        if matches!(
            app.harness_status,
            HarnessStatus::Thinking | HarnessStatus::Starting
        ) {
            Span::styled(
                "Esc or /stop to cancel \u{2502} Ctrl+C quit",
                Style::default().fg(WARNING).add_modifier(bold),
            )
        } else {
            Span::styled(
                "Enter run \u{2502} /stop /mode /yolo /plan /clear /model /qos /help",
                Style::default().fg(DIM),
            )
        },
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM))
            .style(Style::default().bg(BG)),
    );
    f.render_widget(status, chunks[1]);

    // ----- Approval modal -----
    if let Some(p) = &app.harness_pending {
        render_approval_modal(f, area, p);
    }
}

/// Centered modal asking the human to allow/deny a mutating tool call.
fn render_approval_modal(f: &mut Frame, area: Rect, p: &PendingApproval) {
    let bold = Modifier::BOLD;
    let width = 72u16.min(area.width.saturating_sub(4));
    let height = 11u16.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let modal = Rect::new(x, y, width, height);

    let border = if p.danger { ERROR } else { WARNING };
    let mut text = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  tool  ", Style::default().fg(DIM)),
            Span::styled(p.name.clone(), Style::default().fg(INFO).add_modifier(bold)),
        ]),
        Line::from(vec![
            Span::styled("  cmd   ", Style::default().fg(DIM)),
            Span::styled(truncate(&p.summary, 60), Style::default().fg(TEXT)),
        ]),
    ];
    if p.danger {
        text.push(Line::from(Span::styled(
            "  \u{26A0} destructive command — review carefully",
            Style::default().fg(ERROR).add_modifier(bold),
        )));
    }
    text.push(Line::from(""));
    text.push(Line::from(vec![
        Span::styled(
            "   a ",
            Style::default().fg(BG).bg(SUCCESS).add_modifier(bold),
        ),
        Span::styled(" allow once   ", Style::default().fg(TEXT)),
        Span::styled(" s ", Style::default().fg(BG).bg(INFO).add_modifier(bold)),
        Span::styled(" allow always   ", Style::default().fg(TEXT)),
        Span::styled(" d ", Style::default().fg(BG).bg(ERROR).add_modifier(bold)),
        Span::styled(" deny (Esc) ", Style::default().fg(TEXT)),
    ]));

    let clear = Block::default().style(Style::default().bg(Color::Rgb(20, 18, 30)));
    f.render_widget(clear, modal);
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border).add_modifier(bold))
                .style(Style::default().bg(Color::Rgb(20, 18, 30)))
                .title(Span::styled(
                    " Approval required ",
                    Style::default().fg(border).add_modifier(bold),
                )),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, modal);
}

fn bus_subject_color(subject: &str) -> Color {
    if subject.starts_with("QUEEN.phase.") {
        DIM
    } else if subject.starts_with("QUEEN.") {
        Color::Rgb(180, 140, 255)
    } else if subject == "KANNAKA.consciousness" {
        ACCENT
    } else if subject == "KANNAKA.memory.new" {
        SUCCESS
    } else if subject == "KANNAKA.substrate.phi" {
        INFO
    } else if subject == "KANNAKA.dreams" {
        WARNING
    } else if subject.starts_with("KANNAKA.") {
        ACCENT
    } else if subject.starts_with("RADIO.") {
        Color::Rgb(255, 100, 200)
    } else if subject.starts_with("KAX.") {
        Color::Rgb(100, 200, 255)
    } else if subject.starts_with("EYE.") {
        Color::Rgb(255, 200, 100)
    } else if subject == "tui.error" {
        ERROR
    } else {
        TEXT
    }
}

fn render_input(f: &mut Frame, app: &App, area: Rect) {
    let tab_indicator = match app.active_tab {
        0 => "[M]",
        1 => "[S]",
        2 => "[B]",
        3 => "[C]",
        4 => "[D]",
        5 => "[Ch]",
        6 => "[Co]",
        7 => "[Ag]",
        _ => "[?]",
    };

    let input_line = Line::from(vec![
        Span::styled(
            " > ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(&app.input, Style::default().fg(TEXT)),
    ]);

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(BG))
        .title_bottom(Line::from(Span::styled(
            format!(" {tab_indicator} "),
            Style::default().fg(DIM),
        )));

    let input_widget = Paragraph::new(input_line).block(input_block);
    f.render_widget(input_widget, area);

    // Place cursor. `cursor_pos` is a char index; the prompt prefix
    // " > " plus the left border put column 0 of the input at area.x + 4.
    // Clamp to the box interior (one cell inside the right border) so a
    // long line never parks the cursor past the widget and out of `area`.
    let cursor_col = (area.x + 4).saturating_add(app.cursor_pos as u16);
    let max_col = area.x + area.width.saturating_sub(2);
    f.set_cursor_position((cursor_col.min(max_col), area.y + 1));
}

fn render_help_overlay(f: &mut Frame, area: Rect) {
    // Center the help box. Sized for the full tab + command set; the
    // Paragraph clips anything past the box on short terminals.
    let width = 80u16.min(area.width.saturating_sub(4));
    let height = 63u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let help_area = Rect::new(x, y, width, height);

    let dim = Style::default().fg(DIM);
    let text = Style::default().fg(TEXT);
    let hdr = Style::default().fg(INFO).add_modifier(Modifier::BOLD);
    let kbd = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);

    let help_text = vec![
        Line::from(Span::styled(
            concat!(" Kannaka TUI · v", env!("CARGO_PKG_VERSION")),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(" Tabs", hdr)),
        Line::from(vec![
            Span::styled("   Memory        ", text),
            Span::styled("Command history + recent resonant memories", dim),
        ]),
        Line::from(vec![
            Span::styled("   Status        ", text),
            Span::styled("Live Φ / Ξ / order-parameter gauges", dim),
        ]),
        Line::from(vec![
            Span::styled("   Bus           ", text),
            Span::styled(
                "Live NATS pulse — every QUEEN/KANNAKA/RADIO/KAX/EYE event",
                dim,
            ),
        ]),
        Line::from(vec![
            Span::styled("   Constellation ", text),
            Span::styled("Canvas plot of every swarm agent on the unit circle", dim),
        ]),
        Line::from(vec![
            Span::styled("   Dreams        ", text),
            Span::styled("Non-blocking dream trigger + KANNAKA.dreams history", dim),
        ]),
        Line::from(vec![
            Span::styled("   Chat          ", text),
            Span::styled("Persistent chat with the HRM-loaded agent", dim),
        ]),
        Line::from(vec![
            Span::styled("   Cosmos        ", text),
            Span::styled("Constellation-wide app health + radio now-playing", dim),
        ]),
        Line::from(vec![
            Span::styled("   Agent         ", text),
            Span::styled("Coding-agent harness with tools + approvals (default)", dim),
        ]),
        Line::from(""),
        Line::from(Span::styled(" Agent harness (coding agent)", hdr)),
        Line::from(vec![
            Span::styled("   <task>", kbd),
            Span::styled("              Type a task + Enter to run the agent", dim),
        ]),
        Line::from(vec![
            Span::styled("   a", kbd),
            Span::styled(" / ", dim),
            Span::styled("s", kbd),
            Span::styled(" / ", dim),
            Span::styled("d", kbd),
            Span::styled("          Approve once / always / deny (Esc=deny)", dim),
        ]),
        Line::from(vec![
            Span::styled("   Esc", kbd),
            Span::styled(" / ", dim),
            Span::styled("/stop", kbd),
            Span::styled("        Cancel a running/stuck turn", dim),
        ]),
        Line::from(vec![
            Span::styled("   /yolo /plan /default /auto", kbd),
            Span::styled("  Permission mode", dim),
        ]),
        Line::from(vec![
            Span::styled("   /model <id>", kbd),
            Span::styled("        Switch model   ", dim),
            Span::styled("/clear", kbd),
            Span::styled(" Reset", dim),
        ]),
        Line::from(vec![
            Span::styled("   /qos", kbd),
            Span::styled("               Boot QuantumOS on a qBraid Lab + watch window", dim),
        ]),
        Line::from(""),
        Line::from(Span::styled(" Navigation", hdr)),
        Line::from(vec![
            Span::styled("   Tab", kbd),
            Span::styled(" / ", dim),
            Span::styled("Shift+Tab", kbd),
            Span::styled("   Switch tabs", dim),
        ]),
        Line::from(vec![
            Span::styled("   Up", kbd),
            Span::styled(" / ", dim),
            Span::styled("Down", kbd),
            Span::styled("           Command history", dim),
        ]),
        Line::from(vec![
            Span::styled("   PgUp", kbd),
            Span::styled(" / ", dim),
            Span::styled("PgDown", kbd),
            Span::styled("       Scroll messages", dim),
        ]),
        Line::from(vec![
            Span::styled("   F1", kbd),
            Span::styled("                  Toggle help", dim),
        ]),
        Line::from(vec![
            Span::styled("   q", kbd),
            Span::styled(" / ", dim),
            Span::styled("Esc", kbd),
            Span::styled(" / ", dim),
            Span::styled("Ctrl+C", kbd),
            Span::styled("    Quit (q/Esc only when input is empty)", dim),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Dreams tab hotkeys (when input is empty)",
            hdr,
        )),
        Line::from(vec![
            Span::styled("   d", kbd),
            Span::styled("   Deep dream — full consolidation cycle (~30s)", dim),
        ]),
        Line::from(vec![
            Span::styled("   l", kbd),
            Span::styled("   Lite dream — quick pass", dim),
        ]),
        Line::from(vec![
            Span::styled("   r", kbd),
            Span::styled("   Bus: reconnect failed stream · Cosmos: refresh", dim),
        ]),
        Line::from(""),
        Line::from(Span::styled(" Chat tab plugin slash commands", hdr)),
        Line::from(vec![
            Span::styled("   /code <prompt>", kbd),
            Span::styled("   exec kannaka-code (Rust agentic CLI)", dim),
        ]),
        Line::from(vec![
            Span::styled("   /topus <prompt>", kbd),
            Span::styled("  exec kannaktopus (multi-LLM orchestrator)", dim),
        ]),
        Line::from(Span::styled(
            "   plugin stdout streams inline into chat as it runs",
            dim,
        )),
        Line::from(""),
        Line::from(Span::styled(" Bus subject colors", hdr)),
        Line::from(vec![
            Span::styled("   ●", Style::default().fg(ACCENT)),
            Span::styled(" KANNAKA.*     ", text),
            Span::styled("●", Style::default().fg(Color::Rgb(255, 100, 200))),
            Span::styled(" RADIO.*     ", text),
            Span::styled("●", Style::default().fg(Color::Rgb(100, 200, 255))),
            Span::styled(" KAX.*", text),
        ]),
        Line::from(vec![
            Span::styled("   ●", Style::default().fg(Color::Rgb(255, 200, 100))),
            Span::styled(" EYE.*         ", text),
            Span::styled("●", Style::default().fg(Color::Rgb(180, 140, 255))),
            Span::styled(" QUEEN.*     ", text),
            Span::styled("●", Style::default().fg(DIM)),
            Span::styled(" QUEEN.phase.*", text),
        ]),
        Line::from(""),
        Line::from(Span::styled(" Command bar (Memory + Chat tabs)", hdr)),
        Line::from(vec![
            Span::styled("   remember ", text),
            Span::styled("\"text\"        ", text),
            Span::styled("Store a memory", dim),
        ]),
        Line::from(vec![
            Span::styled("   recall ", text),
            Span::styled("\"query\"         ", text),
            Span::styled("Resonance search (top-k 5)", dim),
        ]),
        Line::from(vec![
            Span::styled("   search ", text),
            Span::styled("\"query\"         ", text),
            Span::styled("Literal text search", dim),
        ]),
        Line::from(vec![
            Span::styled("   forget ", text),
            Span::styled("<id>            ", text),
            Span::styled("Delete a memory", dim),
        ]),
        Line::from(vec![
            Span::styled("   dream", text),
            Span::styled("                  Run consolidation (non-blocking)", dim),
        ]),
        Line::from(vec![
            Span::styled("   ask ", text),
            Span::styled("\"question\"         ", text),
            Span::styled("One-shot LLM with HRM recall", dim),
        ]),
        Line::from(vec![
            Span::styled("   hear ", text),
            Span::styled("<file-or-url>     ", text),
            Span::styled("Absorb audio (mp3/wav/flac/stream)", dim),
        ]),
        Line::from(vec![
            Span::styled("   see ", text),
            Span::styled("<file-or-url>      ", text),
            Span::styled("Absorb visual input as a wavefront", dim),
        ]),
        Line::from(vec![
            Span::styled("   relate ", text),
            Span::styled("/ ", dim),
            Span::styled("neighbors ", text),
            Span::styled("\"query\"   ", text),
            Span::styled("Graph recall", dim),
        ]),
        Line::from(Span::styled(
            "   also: clusters · topology · assess · stats · cmf · invariant · voice · swarm · boost · search · market",
            dim,
        )),
        Line::from(Span::styled(
            "   cosmos · constellation · radio → open the Cosmos tab",
            dim,
        )),
        Line::from(Span::styled(
            "   anything else → routed to chat (agent picks tools)",
            dim,
        )),
        Line::from(""),
        Line::from(Span::styled(" Press any key to close", dim)),
    ];

    // Clear background behind overlay
    let clear_block = Block::default().style(Style::default().bg(Color::Rgb(15, 15, 30)));
    f.render_widget(clear_block, help_area);

    let help = Paragraph::new(help_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .style(Style::default().bg(Color::Rgb(15, 15, 30)))
                .title(Span::styled(
                    " Help · F1 to close ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(help, help_area);
}

// ---------------------------------------------------------------------------
// Colour helpers
// ---------------------------------------------------------------------------

fn phi_color(phi: f32) -> Color {
    if phi >= 0.8 {
        SUCCESS
    } else if phi >= 0.5 {
        WARNING
    } else if phi >= 0.2 {
        Color::Rgb(255, 165, 0) // orange
    } else {
        ERROR
    }
}

fn amplitude_color(amp: f32) -> Color {
    if amp >= 0.8 {
        ACCENT
    } else if amp >= 0.5 {
        INFO
    } else {
        DIM
    }
}

fn level_color(level: &str) -> Color {
    match level.to_lowercase().as_str() {
        "resonant" | "transcendent" | "awakened" => SUCCESS,
        "coherent" | "synchronized" => INFO,
        "emerging" | "developing" => WARNING,
        _ => DIM,
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

/// The literal text a key contributes inside a paste burst. Printable chars pass
/// through; Enter/Tab flatten to a space (the input is single-line); everything
/// else contributes nothing. Used to reassemble a Windows paste, which crossterm
/// 0.28 delivers as a burst of Key events rather than an Event::Paste.
fn key_burst_char(key: &KeyEvent, buf: &mut String) {
    match key.code {
        KeyCode::Char(c) => buf.push(c),
        KeyCode::Enter | KeyCode::Tab => buf.push(' '),
        _ => {}
    }
}

fn main() -> io::Result<()> {
    // Handle non-interactive flags BEFORE touching the terminal. Without
    // this, ANY argument (`kannaka-tui --version`, `--help`) fell straight
    // through to the full-screen TUI, which hangs when there is no
    // interactive terminal (e.g. a version probe from `kannaka update` or a
    // script) — there was no way to ask the binary its version.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("kannaka-tui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "kannaka-tui {} — agent harness + dashboard for the Kannaka constellation",
            env!("CARGO_PKG_VERSION")
        );
        println!();
        println!("Usage: kannaka-tui [OPTIONS]");
        println!();
        println!("Options:");
        println!("  -V, --version   Print version and exit");
        println!("  -h, --help      Print this help and exit");
        println!();
        println!("With no options, launches the interactive TUI (Agent, Memory, Status, Bus,");
        println!("Constellation, Dreams, Chat, Cosmos tabs). Type /help inside for commands;");
        println!("/qos boots QuantumOS on a qBraid instance (networked + clean console).");
        return Ok(());
    }

    // Install a panic hook that restores the terminal BEFORE the default
    // hook prints the backtrace. Without this, a panic while in raw mode +
    // alt screen leaves the user's terminal wedged (no echo, no prompt).
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
        prev_hook(info);
    }));

    // Setup terminal
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    // Initial data load
    app.load_status();
    app.load_observe();

    // Main event loop
    loop {
        terminal.draw(|f| ui(f, &app))?;

        // Poll for events with 100ms timeout (allows periodic status refresh)
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                // Unix (and Windows once crossterm gains VT input) deliver a
                // paste as a single event.
                Event::Paste(text) => app.handle_paste(text),
                // Only handle Press events — Windows emits both Press and Release
                // for each keystroke, which would otherwise double every input.
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // crossterm 0.28 has no Event::Paste on Windows: a paste lands
                    // as a burst of Key events, and Windows Terminal often delivers
                    // it in chunks (one per line) with a small gap at each line
                    // break — whereas a human keypress is a singleton. Keep draining
                    // as long as the next event arrives within a short window; this
                    // bridges the inter-line gaps so the WHOLE multi-line paste
                    // coalesces into one burst, its newlines becoming spaces in
                    // handle_paste rather than an Enter that submits a half-line and
                    // leaves the next line's leading 'q' to fire the quit shortcut. A
                    // real keypress sees nothing follow within the window (its only
                    // sibling is a Release we skip), so hotkeys/submit still work.
                    let mut extra: Vec<KeyEvent> = Vec::new();
                    while event::poll(Duration::from_millis(20))? {
                        match event::read()? {
                            Event::Key(k) if k.kind == KeyEventKind::Press => extra.push(k),
                            Event::Paste(t) => app.handle_paste(t),
                            _ => {} // Release / Repeat / resize — ignore
                        }
                    }
                    if extra.is_empty() {
                        app.handle_key(key); // singleton — normal hotkeys/commands
                    } else {
                        let mut burst = String::new();
                        key_burst_char(&key, &mut burst);
                        for k in &extra {
                            key_burst_char(k, &mut burst);
                        }
                        if burst.is_empty() {
                            // A burst of non-text keys (e.g. a held arrow) — replay
                            // each so repeat semantics still work.
                            app.handle_key(key);
                            for k in extra {
                                app.handle_key(k);
                            }
                        } else {
                            app.handle_paste(burst);
                        }
                    }
                }
                _ => {}
            }
        }

        // Drain any completed chat turn from the background thread and
        // advance the spinner.
        app.poll_chat();
        if app.chat_pending.is_some() {
            app.chat_tick = app.chat_tick.wrapping_add(1);
        }

        // Drain async status/observe pollers.
        app.poll_async_data();

        // Drain a completed remember/recall/forget worker (no-op when idle).
        app.poll_cmd();

        // Drain a completed passthrough command (ask/hear/search/…).
        app.poll_passthrough();

        // Drain the live NATS bus stream (no-op until user opens Bus tab).
        app.poll_bus();

        // Drain streaming stdout of any active /code or /topus plugin
        // invocation (no-op when no plugin is running).
        app.poll_plugin();

        // Drain a completed Cosmos poll (no-op until the user opens Cosmos).
        app.poll_cosmos();

        // Drain the agent harness child's NDJSON events; advance its spinner
        // while a turn or approval is in flight.
        app.poll_harness();
        if matches!(
            app.harness_status,
            HarnessStatus::Thinking | HarnessStatus::Starting
        ) {
            app.harness_tick = app.harness_tick.wrapping_add(1);
        }

        // Auto-refresh status every 5s when on the Status tab
        if app.active_tab == 1 && app.last_status_poll.elapsed() > Duration::from_secs(5) {
            app.load_status();
            app.load_observe();
        }

        if app.should_quit {
            break;
        }
    }

    // Restore terminal
    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    // Reap every tracked child so none outlive the TUI:
    //   - bus child (kannaka swarm tail) — owned directly
    //   - chat child (kannaka chat --json REPL) — owned by its worker
    //   - plugin child (kannaka-code / kannaktopus) — owned by its worker
    // Dropping stdin alone doesn't terminate the chat REPL, so kill it.
    if let Some(mut child) = app.bus_child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Agent harness child (kannaka agent --json) — owned directly; kill it so
    // no background coding-agent process outlives the TUI.
    if let Some(mut child) = app.harness_child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    reap_handle(&app.chat_child_proc);
    reap_handle(&app.plugin_child_proc);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests — pure parsing/util functions (the threaded subprocess plumbing is
// exercised by hand, but the parsers it feeds are unit-tested here).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_up_app_with_padded_url() {
        let app =
            parse_constellation_line("  \u{2713} Kannaka Radio    https://radio.ninja-portal.com")
                .unwrap();
        assert_eq!(app.name, "Kannaka Radio");
        assert!(app.up);
        assert_eq!(app.url, "https://radio.ninja-portal.com");
    }

    #[test]
    fn parse_down_app() {
        let app = parse_constellation_line("  \u{2717} Kannaktopus      http://170.9.238.136:8787")
            .unwrap();
        assert_eq!(app.name, "Kannaktopus");
        assert!(!app.up);
        assert_eq!(app.url, "http://170.9.238.136:8787");
    }

    #[test]
    fn parse_single_space_before_scheme() {
        // Name has internal spaces and only ONE space before the scheme;
        // URL detection by `://` must still split it correctly.
        let app = parse_constellation_line(
            "  \u{2713} Kannaka Memory (HRM) systemd://kannaka-memory.service",
        )
        .unwrap();
        assert_eq!(app.name, "Kannaka Memory (HRM)");
        assert_eq!(app.url, "systemd://kannaka-memory.service");
        assert!(app.up);
    }

    #[test]
    fn header_divider_blank_skipped() {
        assert!(parse_constellation_line("  \u{1F310} Kannaka Constellation Status").is_none());
        assert!(parse_constellation_line("  \u{2500}\u{2500}\u{2500}\u{2500}").is_none());
        assert!(parse_constellation_line("").is_none());
        assert!(parse_constellation_line("   ").is_none());
    }

    #[test]
    fn parse_app_without_url() {
        let app = parse_constellation_line("\u{2717} Some Service").unwrap();
        assert_eq!(app.name, "Some Service");
        assert_eq!(app.url, "");
        assert!(!app.up);
    }

    #[test]
    fn parse_multibyte_whitespace_before_url_does_not_panic() {
        // NBSP (U+00A0, 2 bytes) immediately before the scheme used to land
        // `url_start` inside the code point and panic the byte slice.
        let app = parse_constellation_line("\u{2713} App Name\u{00A0}https://app.example").unwrap();
        assert!(app.up);
        assert_eq!(app.url, "https://app.example");
        assert_eq!(app.name, "App Name");
        // Ideographic space (U+3000, 3 bytes) — same boundary hazard.
        let app2 = parse_constellation_line("\u{2717} X\u{3000}wss://y.example/socket").unwrap();
        assert_eq!(app2.url, "wss://y.example/socket");
        assert_eq!(app2.name, "X");
    }

    #[test]
    fn parse_multibyte_name() {
        let app = parse_constellation_line("\u{2713} Café Server   https://café.example").unwrap();
        assert!(app.up);
        assert_eq!(app.name, "Café Server");
        assert_eq!(app.url, "https://café.example");
    }

    #[test]
    fn radio_lines_trim_and_drop_blanks() {
        let lines = clean_radio_lines("  Now Playing: X  \n\n  station | time \n");
        assert_eq!(lines, vec!["Now Playing: X", "station | time"]);
    }

    #[test]
    fn byte_offset_handles_multibyte() {
        assert_eq!(byte_offset("héllo", 0), 0);
        // 'h'=0, 'é'=bytes 1..3, 'l'=byte 3 — char index 2 → byte 3.
        assert_eq!(byte_offset("héllo", 2), 3);
        // Past the end clamps to the byte length (the tail).
        assert_eq!(byte_offset("héllo", 99), "héllo".len());
        assert_eq!(byte_offset("", 0), 0);
    }

    #[test]
    fn agent_event_parses_each_kind() {
        let p = |s: &str| parse_agent_event(&serde_json::from_str(s).unwrap());
        assert!(matches!(
            p(r#"{"kind":"ready","model":"m","mode":"default","cwd":"/w"}"#),
            Some(AgentEvent::Ready { .. })
        ));
        assert!(
            matches!(p(r#"{"kind":"text","text":"hi"}"#), Some(AgentEvent::Text(t)) if t == "hi")
        );
        assert!(matches!(
            p(
                r#"{"kind":"tool_use","id":"t1","name":"bash","input":{"command":"ls"},"read_only":false,"danger":false}"#
            ),
            Some(AgentEvent::ToolUse {
                read_only: false,
                ..
            })
        ));
        assert!(matches!(
            p(
                r#"{"kind":"approval_required","id":"t1","name":"write_file","summary":"a.txt","danger":false}"#
            ),
            Some(AgentEvent::ApprovalRequired { .. })
        ));
        assert!(matches!(
            p(r#"{"kind":"tool_result","id":"t1","content":"ok","is_error":false}"#),
            Some(AgentEvent::ToolResult {
                is_error: false,
                ..
            })
        ));
        assert!(matches!(
            p(r#"{"kind":"usage","input":10,"output":5}"#),
            Some(AgentEvent::Usage {
                input: 10,
                output: 5
            })
        ));
        assert!(matches!(
            p(r#"{"kind":"iteration","n":3}"#),
            Some(AgentEvent::Iteration(3))
        ));
        assert!(
            matches!(p(r#"{"kind":"done","reason":"end_turn"}"#), Some(AgentEvent::Done(r)) if r == "end_turn")
        );
        assert!(matches!(
            p(r#"{"kind":"error","text":"boom"}"#),
            Some(AgentEvent::Error(_))
        ));
        assert!(
            matches!(p(r#"{"kind":"mode","mode":"yolo"}"#), Some(AgentEvent::Mode(m)) if m == "yolo")
        );
        // Unknown / malformed frames yield None.
        assert!(p(r#"{"kind":"who_knows"}"#).is_none());
        assert!(p(r#"{"no_kind":1}"#).is_none());
    }

    #[test]
    fn tool_summary_extracts_key_field() {
        let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(
            tool_input_summary("bash", &j(r#"{"command":"cargo test"}"#)),
            "cargo test"
        );
        assert_eq!(
            tool_input_summary(
                "write_file",
                &j(r#"{"file_path":"src/x.rs","content":"..."}"#)
            ),
            "src/x.rs"
        );
        assert_eq!(
            tool_input_summary("grep", &j(r#"{"pattern":"TODO"}"#)),
            "TODO"
        );
    }

    // -----------------------------------------------------------------------
    // Golden NDJSON contract
    //
    // The TUI's whole contract with the kannaka binary is the NDJSON it reads
    // from `kannaka agent --json`, `kannaka chat --json`, and `kannaka swarm
    // tail`. The fixtures below are representative wire lines for each envelope
    // / event the TUI parses; every field name + type here was checked against
    // the producer in kannaka-memory (bin/handlers/agent.rs, handlers/chat.rs,
    // handlers/swarm.rs, nats.rs, queen.rs). If kannaka-memory ever changes the
    // shape, update these fixtures — and any TUI parse regression breaks here.
    // -----------------------------------------------------------------------

    /// `kannaka agent --json` — field-level extraction per kind.
    /// `agent_event_parses_each_kind` pins the discriminants; this pins the
    /// exact field NAMES the TUI reads out of the richer frames (so a producer
    /// rename like `content`->`text` on tool_result would fail CI here).
    #[test]
    fn golden_agent_event_fields() {
        let p = |s: &str| parse_agent_event(&serde_json::from_str(s).unwrap()).unwrap();

        // ready: model / mode / cwd  (extra `tools` field is ignored)
        let AgentEvent::Ready { model, mode, cwd } =
            p(r#"{"kind":"ready","cwd":"/work","model":"claude","mode":"default","tools":[]}"#)
        else {
            panic!("expected Ready");
        };
        assert_eq!(
            (model.as_str(), mode.as_str(), cwd.as_str()),
            ("claude", "default", "/work")
        );

        // tool_use: id / name / read_only / danger; summary derived from input
        let AgentEvent::ToolUse {
            id,
            name,
            summary,
            read_only,
            danger,
        } = p(
            r#"{"kind":"tool_use","id":"tu1","name":"bash","input":{"command":"ls -la"},"read_only":true,"danger":false}"#,
        )
        else {
            panic!("expected ToolUse");
        };
        assert_eq!(id, "tu1");
        assert_eq!(name, "bash");
        assert_eq!(summary, "ls -la");
        assert!(read_only);
        assert!(!danger);

        // tool_result: id / content / is_error  (producer's `name` is ignored)
        let AgentEvent::ToolResult {
            id,
            content,
            is_error,
        } = p(
            r#"{"kind":"tool_result","id":"tu1","name":"bash","content":"total 0","is_error":false}"#,
        )
        else {
            panic!("expected ToolResult");
        };
        assert_eq!(id, "tu1");
        assert_eq!(content, "total 0");
        assert!(!is_error);

        // approval_required: id / name / summary / danger
        let AgentEvent::ApprovalRequired {
            id,
            name,
            summary,
            danger,
        } = p(
            r#"{"kind":"approval_required","id":"ap1","name":"write_file","summary":"a.txt","danger":true}"#,
        )
        else {
            panic!("expected ApprovalRequired");
        };
        assert_eq!(id, "ap1");
        assert_eq!(name, "write_file");
        assert_eq!(summary, "a.txt");
        assert!(danger);
    }

    /// `kannaka chat --json` — one JSON object per stdout line:
    /// `{"kind": "chat"|"chunk"|"slash"|"error", "text": ".."}`. The chat-child
    /// reader extracts exactly `v["kind"].as_str().unwrap_or("chat")` and
    /// `v["text"].as_str().unwrap_or("")`; this pins those field names + the
    /// documented fallbacks.
    #[test]
    fn golden_chat_response_fields() {
        let fields = |line: &str| {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let kind = v["kind"].as_str().unwrap_or("chat").to_string();
            let text = v["text"].as_str().unwrap_or("").to_string();
            (kind, text)
        };
        for (line, k, t) in [
            (r#"{"kind":"chat","text":"hello"}"#, "chat", "hello"),
            (r#"{"kind":"chunk","text":"par"}"#, "chunk", "par"),
            (r#"{"kind":"slash","text":"/help"}"#, "slash", "/help"),
            (r#"{"kind":"error","text":"boom"}"#, "error", "boom"),
        ] {
            let (kind, text) = fields(line);
            assert_eq!(kind, k);
            assert_eq!(text, t);
        }
        // Missing fields fall back the way the reader does.
        assert_eq!(
            fields(r#"{"text":"no kind"}"#),
            ("chat".to_string(), "no kind".to_string())
        );
        assert_eq!(
            fields(r#"{"kind":"chat"}"#),
            ("chat".to_string(), String::new())
        );
    }

    /// `kannaka swarm tail` — one JSON line per bus message:
    /// `{"ts": <unix-ms>, "subject": "<subj>", "payload": <json|string>}`.
    /// The bus reader pulls ts (i64 ms), subject (str, "?" when absent) and
    /// payload (cloned, Null when absent), then routes by subject prefix.
    #[test]
    fn golden_swarm_tail_envelope() {
        let line = r#"{"ts":1765400000000,"subject":"KANNAKA.activity.remember","payload":{"agent_id":"a1","content":"noted"}}"#;
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let ts = v.get("ts").and_then(|x| x.as_i64()).unwrap_or(0);
        let subject = v
            .get("subject")
            .and_then(|x| x.as_str())
            .unwrap_or("?")
            .to_string();
        let payload = v.get("payload").cloned().unwrap_or(serde_json::Value::Null);
        assert_eq!(ts, 1_765_400_000_000);
        assert_eq!(subject, "KANNAKA.activity.remember");
        assert!(payload.is_object());

        // Absent fields degrade to the documented defaults.
        let bare: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert_eq!(
            bare.get("subject").and_then(|x| x.as_str()).unwrap_or("?"),
            "?"
        );
        assert!(bare
            .get("payload")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
            .is_null());
    }

    /// `QUEEN.phase.<id>` payloads (from a `swarm tail` line) feed
    /// `agent_snapshot_from_payload`. The Rust publisher (queen.rs `AgentPhase`)
    /// emits `phase`; the Kannaktopus arm emits `theta` — either is accepted.
    /// Fields: agent_id, phase|theta, phi, coherence, order_parameter,
    /// handedness (lowercase left|right|achiral), memory_count.
    #[test]
    fn golden_queen_phase_payload() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
        let payload: serde_json::Value = serde_json::from_str(
            r#"{"agent_id":"kannaka-01","phase":1.25,"phi":0.42,"coherence":0.8,"order_parameter":0.7,"handedness":"left","memory_count":128}"#,
        )
        .unwrap();
        let snap = agent_snapshot_from_payload("QUEEN.phase.kannaka-01", &payload).unwrap();
        assert_eq!(snap.agent_id, "kannaka-01");
        assert!(close(snap.theta, 1.25));
        assert!(close(snap.phi, 0.42));
        assert!(close(snap.coherence, 0.8));
        assert!(close(snap.order_parameter, 0.7));
        assert_eq!(snap.handedness, "left");
        assert_eq!(snap.memory_count, 128);

        // `theta` alias (Kannaktopus arm) is accepted in place of `phase`.
        let arm: serde_json::Value =
            serde_json::from_str(r#"{"agent_id":"topus","theta":2.0}"#).unwrap();
        let s2 = agent_snapshot_from_payload("QUEEN.phase.topus", &arm).unwrap();
        assert!(close(s2.theta, 2.0));

        // agent_id falls back to the subject suffix when absent from the payload.
        let noid: serde_json::Value = serde_json::from_str("{}").unwrap();
        let s3 = agent_snapshot_from_payload("QUEEN.phase.fromsub", &noid).unwrap();
        assert_eq!(s3.agent_id, "fromsub");

        // A non-object payload yields None.
        let scalar: serde_json::Value = serde_json::from_str("42").unwrap();
        assert!(agent_snapshot_from_payload("QUEEN.phase.x", &scalar).is_none());
    }

    /// `KANNAKA.dreams` payloads (from a `swarm tail` line) feed
    /// `dream_event_from_payload`. Producer fields (kannaka.rs dream report):
    /// agent_id, cycles, memories_strengthened, memories_pruned,
    /// new_connections, hallucinations_created, consciousness_before,
    /// consciousness_after, emerged.
    #[test]
    fn golden_dreams_payload() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
        let payload: serde_json::Value = serde_json::from_str(
            r#"{"agent_id":"kannaka-01","cycles":3,"memories_strengthened":12,"memories_pruned":4,"new_connections":7,"hallucinations_created":1,"consciousness_before":0.30,"consciousness_after":0.55,"emerged":true}"#,
        )
        .unwrap();
        let ev = dream_event_from_payload(1_765_400_000_000, &payload).unwrap();
        assert_eq!(ev.ts_ms, 1_765_400_000_000);
        assert_eq!(ev.agent_id, "kannaka-01");
        assert_eq!(ev.cycles, 3);
        assert_eq!(ev.strengthened, 12);
        assert_eq!(ev.pruned, 4);
        assert_eq!(ev.new_connections, 7);
        assert_eq!(ev.hallucinations, 1);
        assert!(close(ev.consciousness_before, 0.30));
        assert!(close(ev.consciousness_after, 0.55));
        assert!(ev.emerged);

        // Non-object payload -> None.
        assert!(dream_event_from_payload(0, &serde_json::Value::Null).is_none());
    }

    /// `summarize_payload` renders any bus payload into the Bus-tab one-liner.
    /// It surfaces these known fields when present: agent_id, theta, phi, xi,
    /// consciousness_level, content, event — else falls back to compact JSON.
    #[test]
    fn golden_summarize_payload_fields() {
        let payload: serde_json::Value = serde_json::from_str(
            r#"{"agent_id":"a1","phi":0.5,"consciousness_level":"L5","event":"bloom"}"#,
        )
        .unwrap();
        let s = summarize_payload("KANNAKA.events.bloom", &payload);
        assert!(s.contains("agent=a1"), "got: {s}");
        assert!(s.contains("\u{03A6}=0.500"), "got: {s}"); // Φ
        assert!(s.contains("level=L5"), "got: {s}");
        assert!(s.contains("event=bloom"), "got: {s}");

        // An unknown-shape object still yields an informative compact-JSON line.
        let opaque: serde_json::Value = serde_json::from_str(r#"{"weird":123}"#).unwrap();
        assert!(summarize_payload("x", &opaque).contains("weird"));
    }

    /// `/qos` injects a canned task that names specific backend lab tools.
    /// Pin those names so a rename on the kannaka-memory side (or a typo
    /// here) breaks the build instead of silently confusing the agent.
    #[test]
    fn golden_qos_prompt_tool_names() {
        // Tools common to both the serial and graphical /qos flows.
        for tool in [
            "lab_list_instances",
            "lab_list_profiles",
            "lab_provision_instance",
            "lab_ssh_configure",
            "lab_qos_boot",
            "lab_stop_instance",
        ] {
            assert!(
                QOS_BOOT_PROMPT.contains(tool),
                "QOS_BOOT_PROMPT no longer mentions {tool}"
            );
            assert!(
                QOS_BOOT_GRAPHICAL_PROMPT.contains(tool),
                "QOS_BOOT_GRAPHICAL_PROMPT no longer mentions {tool}"
            );
        }
        // Serial watch = lab_watch (terminal); graphical watch = lab_qos_watch
        // (browser). These are distinct tools on the kannaka-memory side.
        assert!(
            QOS_BOOT_PROMPT.contains("lab_watch"),
            "QOS_BOOT_PROMPT must use lab_watch for the serial console"
        );
        assert!(
            QOS_BOOT_GRAPHICAL_PROMPT.contains("lab_qos_watch"),
            "QOS_BOOT_GRAPHICAL_PROMPT must use lab_qos_watch for the browser view"
        );
        // The graphical prompt must actually ask for the graphical boot.
        assert!(
            QOS_BOOT_GRAPHICAL_PROMPT.contains("graphical=true"),
            "QOS_BOOT_GRAPHICAL_PROMPT must request graphical=true"
        );
        // Step 5: QuantumOS joins the swarm under its own signed identity. This
        // tool is graphical-only (the serial flow keeps COM1 as its console).
        assert!(
            QOS_BOOT_GRAPHICAL_PROMPT.contains("lab_qos_swarm_bridge"),
            "QOS_BOOT_GRAPHICAL_PROMPT must use lab_qos_swarm_bridge to join the swarm"
        );
    }
}
