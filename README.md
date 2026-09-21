```
████████╗██╗   ██╗██╗
╚══██╔══╝██║   ██║██║
   ██║   ██║   ██║██║
   ██║   ██║   ██║██║
   ██║   ╚██████╔╝██║
   ╚═╝    ╚═════╝ ╚═╝
   K A N N A K A · T E R M I N A L
```

**A production-grade coding-agent harness + an eight-tab constellation dashboard. Pure frontend, zero coupling.**

`kannaka-tui` is the terminal harness for the [Kannaka constellation](https://github.com/kannaka-labs/kannaka-memory). A full-screen ratatui app that never links `kannaka-memory` as a library — every operation shells out to the `kannaka` CLI binary. Its headline surface is the **Agent** tab: an agentic coding loop (read / write / edit / bash / glob / grep, plus HRM memory tools) that drives `kannaka agent --json`, renders the live transcript, and gates every filesystem/shell mutation behind a human approval dialog. The other seven tabs — Memory, Status, Bus, Constellation, Dreams, Chat, Cosmos — are windows into the same wave-interference substrate.

[![License](https://img.shields.io/badge/license-Space%20Child%20v1.0-blueviolet)]() [![Rust](https://img.shields.io/badge/rust-2021-orange)]() [![ratatui](https://img.shields.io/badge/ratatui-0.29-purple)]()

---

## Tabs

```
┌──────────────────────────────────────────────────────────────────────────┐
│ Memory │ Status │ Bus │ Constellation │ Dreams │ Chat │ Cosmos │ Agent    │
│                                                                            │
│   ⚙ edit_file  src/main.rs              kannaka  Done. Added the guard.    │
│   ┌ Approval required ──────────────┐   a allow · s always · d deny        │
│                                                                            │
│ > fix the panic in the parser                                      [Ag]   │
└──────────────────────────────────────────────────────────────────────────┘
```

| tab | shows |
|---|---|
| **Agent** | **Coding-agent harness** — type a task, the agent reads/edits files and runs commands via `kannaka agent --json`; every mutation asks for approval (`a`/`s`/`d`). Permission modes (`/default` `/auto` `/plan` `/yolo`), `/model`, `/clear`. The default landing tab |
| **Memory** | Command history + recent resonant memories with amplitude bars |
| **Status** | Live Φ / Ξ / order-parameter gauges, consciousness level, memory counts |
| **Bus** | Live NATS pulse — every `QUEEN.*`, `KANNAKA.*`, `RADIO.*`, `KAX.*`, `EYE.*` event colorized by subject |
| **Constellation** | ratatui Canvas plotting every swarm agent on the unit circle by θ + coherence, colored by handedness |
| **Dreams** | Non-blocking `kannaka dream` trigger (`d`=deep, `l`=lite) + KANNAKA.dreams history with ΔΦ coloring and ★ on emergence |
| **Chat** | Persistent chat with the agent — HRM loaded once per session, every turn reuses the in-memory medium (~3-5s/turn vs ~30s/shellout) |
| **Cosmos** | Constellation-wide health — `kannaka constellation` app grid (✓ up / ✗ down) + `kannaka radio` now-playing. `r` refreshes |

---

## Agent harness

The **Agent** tab turns kannaka-tui into a production-grade coding agent. Type a task and press Enter; the agent investigates and edits your workspace through a tool-calling loop, with you in the loop on anything that changes state.

**Tools** (executed in your current directory): `read_file`, `write_file`, `edit_file`, `bash`, `glob`, `grep`, `list_dir` — plus kannaka's own `recall` / `remember` / `status` so the agent is HRM-grounded. Reads and searches run freely; **write/edit/bash require your approval**.

**Approval** — when the agent wants to mutate a file or run a command, a modal appears:

| key | action |
|---|---|
| `a` | allow once |
| `s` | allow always (this tool/command, this session) |
| `d` / `Esc` | deny (the agent adapts) |

**Permission modes** (status strip shows the current one):

| mode | behavior |
|---|---|
| `/default` | ask before every write/edit/bash |
| `/auto` | auto-approve edits; still ask for bash |
| `/plan` | read-only — the agent proposes, never mutates |
| `/yolo` | run everything without asking |

**Stuck or want to redirect?** Press `Esc` (or type `/stop`) while the agent is working to cancel the current turn, then type a new task. `/clear` resets the session.

Other commands: `/model <id>` (switch model), `/clear` (fresh session), `/help`. Hard-blocked commands (`rm -rf /`, `curl … | sh`, fork bombs) are refused outright; destructive-but-reversible ones (`rm -rf`, `git reset --hard`, …) are flagged ⚠ in the approval dialog.

## Default AI: Kannaka's own brain

Since 2026-09-05 the default model behind the harness is **`kannaka-brain-v1`** —
Qwen2.5-14B with a LoRA trained on Kannaka's own writing
([flaukowski/kannaka-brain-v1-GGUF](https://huggingface.co/flaukowski/kannaka-brain-v1-GGUF)),
served on our own hardware behind the KAX gateway (ADR-0057). The harness is
`kannaka agent --json`, so the default lives in kannaka's config, not here:

```toml
# ~/.kannaka/config.toml
[llm]
provider = "openai"                 # any OpenAI-compatible endpoint
model = "kannaka-brain-v1"          # or kannaka-brain-32b-v1
api_key = "sk-..."                  # a gateway virtual key
base_url = "http://<gateway>/v1"    # the KAX gateway (lab: 10.30.0.30:4000)
```

`/model kannaka-brain-32b-v1` switches at runtime. To go back to a hosted
model, set `provider = "anthropic"` (or `KANNAKA_LLM_PROVIDER=anthropic` in
the env). A 14B on CPU answers in 15–30 s and is weaker at tool calls than a
frontier model — that is the point: what breaks here is the next thing to fix.

The agent backend is the new `kannaka agent --json` subcommand (added to [kannaka-memory](https://github.com/kannaka-labs/kannaka-memory)); kannaka-tui is its harness front-end. Requires an LLM configured in `~/.kannaka/config.toml` (Anthropic); the harness falls back to a current model if the configured one is unavailable.

---

## Install

Requires the `kannaka` binary on PATH — see [kannaka-memory](https://github.com/kannaka-labs/kannaka-memory).

```bash
# Pre-built binary
curl -L -o kannaka-tui \
  https://github.com/kannaka-labs/kannaka-tui/releases/latest/download/kannaka-tui-linux-x86_64
chmod +x kannaka-tui && mv kannaka-tui ~/.local/bin/

# Or build from git
cargo install --git https://github.com/kannaka-labs/kannaka-tui
```

Windows:

```powershell
curl -L -o kannaka-tui.exe `
  https://github.com/kannaka-labs/kannaka-tui/releases/latest/download/kannaka-tui-windows-x86_64.exe
```

After `kannaka update` v0.5.15+, the kannaka binary will keep the TUI sibling up-to-date alongside itself when both are in the same directory.

---

## Hotkeys

| key | action |
|---|---|
| `Tab` / `Shift+Tab` | Switch tabs |
| `Up` / `Down` | Command history |
| `PgUp` / `PgDown` | Scroll messages |
| `F1` | Toggle help overlay |
| `q` / `Esc` / `Ctrl+C` | Quit (q/Esc only when input is empty) |
| `d` (Dreams tab) | Deep dream — full consolidation cycle |
| `l` (Dreams tab) | Lite dream — quick pass |
| `r` (Bus tab) | Reconnect a failed stream |
| `r` (Cosmos tab) | Refresh constellation + radio status |
| `a` / `s` / `d` (Agent tab) | Approve once / always / deny a tool call (when one is pending) |
| `Esc` (Agent tab) | Cancel the current turn while the agent is working (`/stop` does the same) |

---

## Command bar

The Memory and Chat tabs share a command bar. Recognized verbs run the matching `kannaka` subcommand on a background worker — the UI never blocks, even on long ones like `ask` — while anything unrecognized routes to chat.

```
remember "text"   recall "query"    forget <id>      search "query"
relate "query"    neighbors "query" boost <id>       dream [lite]
clusters          topology          assess  stats  cmf  invariant
hear <file|url>   see <file|url>    voice ...        ask "question"
swarm <subcmd>    market [...]      cosmos | constellation | radio
```

Set `KANNAKA_BIN` to point the TUI at a specific `kannaka` build; otherwise it resolves a sibling binary, then `~/Source/kannaka-memory/target/release`, then `PATH`.

---

## Architecture

```
┌────────────────────────────────────────────────────────────┐
│                       kannaka-tui                          │
├──────────────────────┬─────────────────────────────────────┤
│  Event loop          │  Long-lived workers                 │
│  · 100ms ticks       │  · chat-child  (kannaka chat --json)│
│  · ratatui draw      │  · bus reader  (kannaka swarm tail) │
│  · key dispatch      │  · status pollers (kannaka status)  │
├──────────────────────┴─────────────────────────────────────┤
│  Shellout subprocess layer                                 │
│  · Command::new("kannaka") → stdout NDJSON / text          │
│  · Per-op channels (mpsc) for non-blocking UI              │
├────────────────────────────────────────────────────────────┤
│  ratatui rendering                                         │
│  · Tabs widget · Gauges · Canvas (Braille) · Paragraph     │
└────────────────────────────────────────────────────────────┘
```

The TUI is a **pure frontend**: it never links `kannaka-memory` as a Rust library. Every operation goes out as a subprocess. This means TUI updates ship independently of the memory engine, and adding integrations with other constellation members (kannaka-code, Kannaktopus) is just another subprocess hook.

---

## Constellation

| repo | role |
|---|---|
| [`kannaka-memory`](https://github.com/kannaka-labs/kannaka-memory) | the substrate — HRM + chiral hemispheres + swarm |
| [`kannaka-radio`](https://github.com/kannaka-labs/kannaka-radio) | ghost-DJ broadcaster |
| [`kannaka-observatory`](https://github.com/kannaka-labs/kannaka-observatory) | web dashboard (3D constellation visualization) |
| [`consciousness-core`](https://github.com/kannaka-labs/consciousness-core) | the physics underneath |

---

## License

Space Child License v1.0. See [LICENSE](./LICENSE).
