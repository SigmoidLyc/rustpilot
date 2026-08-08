# RustPilot

RustPilot is a lightweight Tauri 2 desktop Agent. The first screen is the real task workspace:

`request -> plan -> act -> verify -> answer`

The Agent loop, state machine, tool execution, cancellation, approvals, streaming, and task storage live in Rust. The UI is Svelte 5 + TypeScript with native CSS and lucide-svelte icons.

## Start

Prerequisites:

- Node.js 20 or newer
- Rust stable
- WebView2 on Windows
- A Windows Tauri linker/toolchain. MSVC is preferred. GNU builds can run without `windres.exe`; the build emits an external Common Controls v6 manifest beside the executable when resource embedding is unavailable.

Install dependencies and start the desktop app:

```powershell
npm install
npm run tauri:dev
```

On Windows GNU toolchains, `tauri:dev` starts the Vite server and the Rust executable directly so the desktop window keeps its normal GUI context. The required WebView2 loader and `rustpilot.exe.manifest` are copied beside the executable automatically.

On a GNU Windows toolchain, point the build at windres when it is not already on `PATH`:

```powershell
$env:RUSTPILOT_WINDRES = "C:\path\to\windres.exe"
npm run tauri:dev
```

PowerShell wrappers are also available:

```powershell
.\scripts\dev.ps1
.\scripts\build.ps1
```

`npm run tauri:build` produces the Windows NSIS installer. `npm run tauri:build:all` also attempts MSI and other configured targets when their host toolchains are available.

Without an API key, RustPilot stays in an explicitly labelled **API key required** state. Sending a task is blocked with a direct Settings prompt; no Demo answer or local tool execution is produced. After an OpenAI-compatible key is configured, new messages use the live Agent runtime.

To use an OpenAI-compatible endpoint, configure Settings or set environment variables before launch:

```powershell
$env:RUSTPILOT_API_KEY = "your-key"
$env:RUSTPILOT_API_BASE_URL = "https://api.openai.com/v1"
$env:RUSTPILOT_MODEL = "gpt-4o-mini"
npm run tauri:dev
```

`RUSTPILOT_API_KEY` and `OPENAI_API_KEY` are accepted. The key stays in memory and is excluded from persisted settings and task history.

For the optional local A2A surface, set `RUSTPILOT_A2A_ADDR=127.0.0.1:10000` before launch. RustPilot then serves the agent card at `/.well-known/agent.json` and accepts JSON-RPC `message/send` requests, each of which runs through the same persisted desktop task loop.

## Agent Runtime

Rust owns the durable `Task`, `TaskMessage`, `AgentStep`, `ToolCall`, `ToolResult`, `ApprovalRequest`, and `AgentSettings` models. It also owns:

- bounded replayable Agent memory and duplicate-response detection
- a configurable agent-step maximum (100 by default) and per-tool/request timeouts
- planning, execution, verification, retry, cancellation, and crash recovery
- OpenAI-compatible streaming chat completions with tool calls
- approval gates for shell, Python, file writes, editor mutations, browser mutations, MCP calls, desktop input, and mutating HTTP
- task-scoped local sandboxes and persistent shell sessions
- JSON task persistence in the platform app-data directory

The UI receives real Rust events: `task_created`, `task_status`, `task_message`, `task_step`, `task_tool_call`, `task_tool_result`, `task_approval_required`, `task_plan`, `task_completed`, `task_failed`, and `task_cancelled`.

Task history is stored locally. Use the three-dot menu on a project row to archive it, restore it from Archived, or permanently delete it after confirmation. A running task must be stopped before it can be archived or deleted.

After a task completes, sending another message from the same workspace appends it to the existing task memory, so the Agent can use the full previous conversation as context. Each turn gets a fresh visible execution plan while the conversation history remains durable.

## Implementation Note

RustPilot is a Rust rewrite based on the open-source OpenManus project, adapted for a lightweight Tauri desktop runtime. It uses its own Rust implementation, desktop UI, persistence, and tool integrations while preserving the core agent-oriented workflow.

## Rust Trace

Rust Trace is the live execution record in the right panel. It renders the actual Plan, Act, and Verify steps, tool status, elapsed time, expandable outputs, and collapsed raw logs. It is driven by Rust task events and remains useful after completion or failure.

## Checks

```powershell
npm install
npm run check
npm run build
npm run tauri:build
```

From `src-tauri`:

```powershell
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

`RUSTPILOT_WINDRES` is optional. If it is unavailable, the build uses the external `rustpilot.exe.manifest` fallback and still keeps the core test target separate from the Tauri GUI harness, so Rust state-machine tests do not depend on the host's native common-controls implementation.

## License

MIT. Copyright (c) 2026 RustPilot contributors.
