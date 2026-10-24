# Computer use: what a full Rust rewrite of the engine would take

Status 24.10.2026: not planned. Today IronClaw runs the Open Computer Use
binary (Swift, MIT, `iFurySt/open-codex-computer-use`) as a stdio MCP server
through `crates/lanes/ironclaw_computer_use`. This note sizes the step after
that: replacing the Swift engine with Rust inside IronClaw.

## What the engine is

Measured on the patched fork at `~/src/open-computer-use` (branch
`hermes/precision-and-speed`): about 19,400 lines of Swift in the shipping
package and app (36,700 with experiments). The biggest parts:

| Part | Swift lines | What it does |
|---|---|---|
| `AccessibilitySnapshot` | 2,700 | Walks the AX tree of the key window, numbers elements, renders the text tree the model reads |
| `ComputerUseService` + `ToolDispatcher` + `ToolDefinitions` | 3,460 | The eleven tools, argument rules, result headers, screenshots |
| Cursor (`CursorMotionModel`, `SoftwareCursorOverlay`, glyph renderer, window anchor) | 2,970 | The Codex-style animated cursor drawn over the target window |
| Input (`InputSimulation`, `SkyClickSimulation`, `SkyKeyboardSimulation`, `SkyLightSPI`) | 1,400 | Clicks and keys: accessibility actions, app-posted events, SkyLight private API, global fallback |
| App agent (`MacOSAppAgentProxy`, `PermissionOnboardingApp`, `Permissions`) | 2,730 | The signed `.app` that owns Accessibility and Screen Recording, the socket the CLI proxies to, onboarding |
| Locked use (broker, state machine, IPC) | 1,100 | Driving apps while the screen is locked (experimental) |
| Discovery, display, CLI | 1,430 | `list_apps`, display geometry, `mcp` entry point |

## What a Rust port needs

1. **macOS bindings.** `objc2` + `objc2-app-kit` / `objc2-application-services`
   cover AppKit and the AX C API; `core-graphics` covers CGEvent and window
   lists; ScreenCaptureKit needs `objc2` bindings (`screencapturekit` crate or
   hand-written). SkyLight is private API: hand-declared `extern "C"`
   signatures, as the Swift code does.
2. **A signed helper app.** macOS grants Accessibility and Screen Recording
   per code signature. IronClaw would need its own signed, notarised helper
   bundle (the GPUI app could be it) and the same onboarding flow. Without
   this nothing works; it is release-engineering work, not code.
3. **The accessibility snapshot and its text format.** The model's behaviour
   depends on the exact tree text (indices, roles, "Secondary Actions"). The
   port must reproduce it byte for byte or the prompts and the plugin's
   evals stop matching.
4. **Input paths.** Four click methods with different trade-offs (background
   vs. moving the real pointer). The SkyLight paths are the fragile part.
5. **Cursor overlay.** A transparent always-on-top window with the animated
   cursor. Optional for function, important for trust (the user sees what
   the agent does). GPUI could draw it.

## Estimate and trade-offs

| Option | Effort | Gain | Risk |
|---|---|---|---|
| Keep the Swift binary (today) | none | works now, upstream fixes for free | a second toolchain and app bundle to install; stdio hop per call |
| Port tools + AX snapshot + input to Rust, keep cursor and onboarding in the Swift app | ~6-8 weeks | one process, no JSON-RPC hop, typed results | text-format drift; still two bundles |
| Full port incl. app agent, cursor, onboarding | ~3-4 months | single Rust codebase and one signed bundle with the GPUI app | private-API breakage on macOS updates now lands on us; signing/notarisation pipeline needed |

Recommendation: stay on the binary until computer use is used daily and the
GPUI app has its own signed bundle. Then port in the order snapshot → tools →
input, behind the same `ComputerUseSessions` port, so the approval gate,
Stop, take over and frame routes do not change.
