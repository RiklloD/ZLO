//! Driven against a fake MCP server (a POSIX shell script), never the real
//! binary: nothing here touches the screen.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ironclaw_host_api::ids::{CapabilityId, ExtensionId, InvocationId, TenantId, ThreadId, UserId};
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_mcp::{McpClient, McpClientRequest};
use serde_json::json;

use ironclaw_product_contracts::computer_use::{
    ComputerUseControlAction, ComputerUseSession, ComputerUseSessions,
};

use crate::{
    ComputerUse, ComputerUseConfig, ComputerUseOutcome, RefusalCode, SessionState,
    computer_use_tools,
};

/// Logs every line it reads to `$1`. `press_key` never answers, `set_value`
/// answers with a JSON-RPC error, every other call with a tree header and a
/// screenshot (after a stray non-JSON line and a notification).
const FAKE_SERVER: &str = r#"log="$1"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"capabilities":{},"instructions":"fake"}}\n' "$id" ;;
    *'"name":"press_key"'*) sleep 5 ;;
    *'"name":"set_value"'*) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"no such element"}}\n' "$id" ;;
    *'"method":"tools/call"'*) printf 'not json\n{"jsonrpc":"2.0","method":"notifications/progress"}\n{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"App=com.apple.calculator (pid 42)\\nWindow: \\"Calculator\\", App: Calculator.\\n0 standard window"},{"type":"image","mimeType":"image/png","data":"iVBORw0KGgo="}]}}\n' "$id" ;;
  esac
done
"#;

struct Fake {
    _dir: tempfile::TempDir,
    log: PathBuf,
    computer_use: Arc<ComputerUse>,
}

fn fake(tune: impl FnOnce(&mut ComputerUseConfig)) -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake_server.sh");
    std::fs::write(&script, FAKE_SERVER).unwrap();
    let log = dir.path().join("calls.log");
    let mut config = ComputerUseConfig::new("/bin/sh");
    config.args = vec![script.display().to_string(), log.display().to_string()];
    config.stop_hold = Duration::from_millis(300);
    config.call_timeout = Duration::from_secs(5);
    config.cursor_dir = dir.path().join("cursor");
    config.trace_path = Some(dir.path().join("trace.jsonl"));
    tune(&mut config);
    Fake {
        _dir: dir,
        log,
        computer_use: Arc::new(ComputerUse::new(config)),
    }
}

fn starts(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .matches("\"method\":\"initialize\"")
        .count()
}

fn key(user: &str) -> ComputerUseSession {
    ComputerUseSession {
        tenant_id: TenantId::new("tenant-cu").unwrap(),
        user_id: UserId::new(user).unwrap(),
        thread_id: ThreadId::new("thread-cu").unwrap(),
    }
}

async fn click(computer_use: &ComputerUse, key: &ComputerUseSession) -> ComputerUseOutcome {
    computer_use
        .call(
            key,
            "click",
            json!({"app": "Calculator", "element_index": "0"}),
            64 * 1024,
        )
        .await
}

/// Regression: a model re-read the same Outlook tree 8 times without acting.
/// A repeat read now says nothing changed; an action makes the next read fresh.
#[tokio::test]
async fn an_unchanged_state_read_says_so_instead_of_repeating_the_tree() {
    let fake = fake(|_| {});
    let cu = &fake.computer_use;
    let alice = key("alice");
    let read = || {
        cu.call(
            &alice,
            "get_app_state",
            json!({"app": "Calculator"}),
            64 * 1024,
        )
    };
    let text = |outcome: ComputerUseOutcome| match outcome {
        ComputerUseOutcome::Completed { text, .. } => text,
        _ => panic!("call did not complete"),
    };

    assert!(text(read().await).contains("0 standard window"));
    assert!(text(read().await).starts_with("(no accessibility tree changes"));
    click(cu, &alice).await;
    assert!(text(read().await).contains("0 standard window"));
}

#[tokio::test]
async fn a_call_returns_the_tree_keeps_the_frame_and_reuses_the_process() {
    let fake = fake(|_| {});
    let cu = &fake.computer_use;
    let alice = key("alice");

    let ComputerUseOutcome::Completed {
        text,
        is_error,
        frame_seq,
    } = click(cu, &alice).await
    else {
        panic!("click did not complete");
    };
    assert!(!is_error);
    assert!(text.contains("0 standard window"));
    assert_eq!(frame_seq, Some(1));

    let status = cu.status(&alice);
    assert_eq!(status.state, SessionState::Idle);
    assert!(status.process_running);
    assert_eq!(status.app.as_deref(), Some("com.apple.calculator"));
    assert_eq!(status.window.as_deref(), Some("Calculator"));
    let frame = cu.latest_frame(&alice, None).expect("frame kept");
    assert_eq!((frame.seq, frame.data.as_str()), (1, "iVBORw0KGgo="));
    assert!(cu.latest_frame(&alice, Some(1)).is_none(), "nothing newer");

    // The model gets each result's own screenshot, the newest MODEL_FRAMES.
    cu.bind_result_frame(&alice, 1, "result:1");
    assert_eq!(cu.result_frame(&alice, "result:1").map(|f| f.seq), Some(1));
    for n in 2..=crate::session::MODEL_FRAMES as u64 + 1 {
        let ComputerUseOutcome::Completed { frame_seq, .. } = click(cu, &alice).await else {
            panic!("click did not complete");
        };
        let seq = frame_seq.expect("a frame per click");
        cu.bind_result_frame(&alice, seq, &format!("result:{n}"));
    }
    assert!(
        cu.result_frame(&alice, "result:1").is_none(),
        "oldest dropped"
    );
    assert!(cu.result_frame(&alice, "result:2").is_some());

    // A server-side error leaves the process (and its element indices) alive.
    let rejected = cu
        .call(&alice, "set_value", json!({"app": "Calculator"}), 1024)
        .await;
    assert_eq!(
        rejected.to_json(),
        json!({"ok": false, "code": "action_failed", "error": "no such element"})
    );
    assert!(matches!(
        click(cu, &alice).await,
        ComputerUseOutcome::Completed {
            frame_seq: Some(5),
            ..
        }
    ));
    assert_eq!(starts(&fake.log), 1, "one process for the whole session");

    // Another user on the same thread id has a separate, empty session.
    assert_eq!(cu.status(&key("bob")).state, SessionState::None);
    assert!(cu.latest_frame(&key("bob"), None).is_none());

    cu.turn_ended(&alice.tenant_id, &alice.thread_id).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let log = std::fs::read_to_string(&fake.log).unwrap();
    assert!(log.contains("notifications/turn-ended"));
    assert_eq!(
        cu.call(&alice, "no_such_tool", json!({}), 1024).await,
        ComputerUseOutcome::Refused(RefusalCode::UnknownTool)
    );
}

#[tokio::test]
async fn stop_aborts_the_call_in_flight_kills_the_process_and_holds() {
    let fake = fake(|_| {});
    let cu = Arc::clone(&fake.computer_use);
    let alice = key("alice");
    let running = {
        let cu = Arc::clone(&cu);
        let alice = alice.clone();
        tokio::spawn(async move {
            cu.call(
                &alice,
                "press_key",
                json!({"app": "Calculator", "key": "Return"}),
                1024,
            )
            .await
        })
    };
    for _ in 0..100 {
        if cu.status(&alice).state == SessionState::Running && starts(&fake.log) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(cu.status(&alice).state, SessionState::Running);

    let stopped = cu.stop(&alice).await;
    assert_eq!(stopped.state, SessionState::Stopped);
    let outcome = tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("Stop ends the call promptly")
        .unwrap();
    assert_eq!(
        outcome,
        ComputerUseOutcome::Refused(RefusalCode::CancelledInFlight)
    );
    assert_eq!(outcome.to_json()["code"], "cancelled");
    assert!(
        outcome.to_json()["error"]
            .as_str()
            .unwrap()
            .contains("Whether it took effect is unknown")
    );
    assert!(!cu.status(&alice).process_running, "process killed");
    assert_eq!(
        click(&cu, &alice).await,
        ComputerUseOutcome::Refused(RefusalCode::Cancelled),
        "new actions are refused during the hold"
    );

    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(matches!(
        click(&cu, &alice).await,
        ComputerUseOutcome::Completed { .. }
    ));
    assert_eq!(starts(&fake.log), 2, "a fresh process after Stop");

    // The thread's own Stop ends an idle session too.
    cu.stop_thread(&alice.tenant_id, &alice.thread_id).await;
    assert!(!cu.status(&alice).process_running);
}

#[tokio::test]
async fn take_over_refuses_actions_until_hand_back() {
    let fake = fake(|_| {});
    let cu = &fake.computer_use;
    let alice = key("alice");
    assert_eq!(cu.take_over(&alice).unwrap().state, SessionState::TakenOver);
    let refused = click(cu, &alice).await;
    assert_eq!(
        refused,
        ComputerUseOutcome::Refused(RefusalCode::HumanHasControl)
    );
    assert_eq!(refused.to_json()["code"], "human_has_control");
    assert_eq!(
        starts(&fake.log),
        0,
        "nothing started while the user holds the desktop"
    );

    assert_eq!(cu.hand_back(&alice).state, SessionState::Idle);
    assert!(matches!(
        click(cu, &alice).await,
        ComputerUseOutcome::Completed { .. }
    ));
}

#[tokio::test]
async fn a_timeout_kills_the_process_and_sessions_are_bounded() {
    let fake = fake(|config| {
        config.call_timeout = Duration::from_millis(300);
        config.max_sessions = 1;
    });
    let cu = &fake.computer_use;
    let alice = key("alice");
    assert_eq!(
        cu.call(
            &alice,
            "press_key",
            json!({"app": "Calculator", "key": "a"}),
            1024
        )
        .await,
        ComputerUseOutcome::Refused(RefusalCode::Timeout)
    );
    assert!(matches!(
        click(cu, &alice).await,
        ComputerUseOutcome::Completed { .. }
    ));
    assert_eq!(starts(&fake.log), 2, "the wedged process was replaced");
    assert_eq!(
        click(cu, &key("bob")).await,
        ComputerUseOutcome::Refused(RefusalCode::Busy)
    );
    cu.shutdown().await;
    assert_eq!(cu.status(&alice).state, SessionState::None);
}

#[tokio::test]
async fn the_mcp_client_seam_needs_a_thread_and_a_known_capability() {
    let fake = fake(|_| {});
    let mut scope =
        ResourceScope::local_default(UserId::new("alice").unwrap(), InvocationId::new()).unwrap();
    let request = |scope: ResourceScope, capability: &str| McpClientRequest {
        provider: ExtensionId::new("computer_use").unwrap(),
        capability_id: CapabilityId::new(capability).unwrap(),
        scope,
        transport: crate::COMPUTER_USE_TRANSPORT.to_string(),
        command: Some("/bin/false".to_string()),
        args: Vec::new(),
        url: None,
        input: json!({"app": "Calculator"}),
        max_output_bytes: 64 * 1024,
    };
    let client: &dyn McpClient = fake.computer_use.as_ref();
    let no_thread = client
        .call_tool(request(scope.clone(), "computer_use.click"))
        .await
        .unwrap_err();
    assert_eq!(no_thread.stable_reason(), "computer_use_requires_thread");

    scope.thread_id = Some(ThreadId::new("thread-cu").unwrap());
    let unknown = client
        .call_tool(request(scope.clone(), "computer_use.format_disk"))
        .await
        .unwrap_err();
    assert_eq!(unknown.stable_reason(), "computer_use_unknown_tool");

    // The request's command is ignored: the host-configured binary runs.
    let output = client
        .call_tool(request(scope, "computer_use.click"))
        .await
        .unwrap();
    assert_eq!(output.output["ok"], true);
    assert!(
        output.output["result"]
            .as_str()
            .unwrap()
            .contains("0 standard window")
    );
    assert!(
        output.output.get("screenshot_frame").is_none(),
        "screenshots stay host-side"
    );
}

#[test]
fn config_follows_the_bundled_engine_and_fails_closed_when_half_set() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    };
    let bundled = || Some(std::path::PathBuf::from("/bin/sh"));
    assert!(
        ComputerUseConfig::from_lookup(env(&[]), None)
            .unwrap()
            .is_none()
    );
    let auto = ComputerUseConfig::from_lookup(env(&[]), bundled())
        .unwrap()
        .unwrap();
    assert_eq!(auto.binary, std::path::PathBuf::from("/bin/sh"));
    assert!(
        ComputerUseConfig::from_lookup(env(&[("COMPUTER_USE_ENABLED", "false")]), bundled())
            .unwrap()
            .is_none()
    );
    assert!(
        ComputerUseConfig::from_lookup(env(&[("COMPUTER_USE_ENABLED", "true")]), None).is_err()
    );
    assert!(
        ComputerUseConfig::from_lookup(
            env(&[
                ("COMPUTER_USE_ENABLED", "true"),
                ("COMPUTER_USE_BINARY", "relative/OpenComputerUse"),
            ]),
            None,
        )
        .is_err()
    );
    assert!(
        ComputerUseConfig::from_lookup(env(&[("COMPUTER_USE_ENABLED", "maybe")]), None).is_err()
    );
    let on = ComputerUseConfig::from_lookup(
        env(&[
            ("COMPUTER_USE_ENABLED", "true"),
            ("COMPUTER_USE_BINARY", "/bin/sh"),
            ("COMPUTER_USE_CALL_TIMEOUT_SECS", "30"),
        ]),
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(on.args, ["mcp"]);
    assert_eq!(on.call_timeout, Duration::from_secs(30));
    assert_eq!(
        on.env,
        [(
            "OPEN_COMPUTER_USE_DISABLE_APP_AGENT_PROXY".to_string(),
            "1".to_string()
        )]
    );
}

#[test]
fn the_embedded_catalog_is_the_servers_tool_list() {
    let names: Vec<&str> = computer_use_tools()
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    for expected in [
        "list_apps",
        "get_app_state",
        "click",
        "perform_secondary_action",
        "scroll",
        "drag",
        "type_text",
        "press_key",
        "set_value",
    ] {
        assert!(names.contains(&expected), "{expected} missing");
    }
    let click = crate::computer_use_tool("click").unwrap();
    assert_eq!(click.input_schema["required"], json!(["app"]));
    for tool in computer_use_tools() {
        CapabilityId::new(format!("computer_use.{}", tool.name)).unwrap();
    }
}

#[tokio::test]
async fn the_product_port_drives_controls_and_the_session_view() {
    let fake = fake(|_| {});
    let port: &dyn ComputerUseSessions = fake.computer_use.as_ref();
    let alice = key("alice");
    let mut scope =
        ResourceScope::local_default(alice.user_id.clone(), InvocationId::new()).unwrap();
    scope.tenant_id = alice.tenant_id.clone();
    scope.thread_id = Some(alice.thread_id.clone());

    let taken = port
        .control(&alice, ComputerUseControlAction::TakeOver, Some("viewer-1"))
        .await;
    assert_eq!(taken["session"]["state"], "taken_over");
    let refused = port
        .call_tool(scope.clone(), "click", json!({"app": "Calculator"}))
        .await;
    assert_eq!(refused["code"], "human_has_control");
    port.control(&alice, ComputerUseControlAction::HandBack, None)
        .await;

    // Take over holds the desktop: another thread of the same user waits too.
    port.control(&alice, ComputerUseControlAction::TakeOver, Some("viewer-1"))
        .await;
    let mut other = scope.clone();
    other.thread_id = Some(ThreadId::new("thread-other").unwrap());
    let refused = port
        .call_tool(other, "click", json!({"app": "Calculator"}))
        .await;
    assert_eq!(refused["code"], "human_has_control");
    port.control(&alice, ComputerUseControlAction::HandBack, None)
        .await;

    let done = port
        .call_tool(scope, "click", json!({"app": "Calculator"}))
        .await;
    assert_eq!(done["ok"], true);

    // The live view: the session, its label and the outcome, for this user only.
    let state = port.viewer_state(&alice.tenant_id, &alice.user_id, Some("viewer-1"));
    let session = state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["thread_id"] == "thread-cu")
        .expect("the session is listed");
    assert_eq!(session["label"], "Clicking in Calculator");
    assert_eq!(session["status"], "ok");
    assert_eq!(session["inflight"], 0);
    assert_eq!(session["human_has_control"], false);
    let bob = key("bob");
    assert!(
        port.viewer_state(&bob.tenant_id, &bob.user_id, None)["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Trace: tool, ok and code, never arguments.
    let trace = std::fs::read_to_string(fake._dir.path().join("trace.jsonl")).unwrap();
    assert!(trace.contains("\"code\":\"human_has_control\""));
    assert!(trace.lines().last().unwrap().contains("\"ok\":true"));
    assert!(!trace.contains("Calculator"), "no arguments in the trace");

    let view = port.session_view(&alice, None);
    assert_eq!(view["session"]["state"], "idle");
    assert_eq!(view["frame"]["seq"], 1);
    assert_eq!(view["frame"]["mimeType"], "image/png");
    assert!(port.session_view(&alice, Some(1))["frame"].is_null());

    port.thread_stopped(&alice.tenant_id, &alice.thread_id)
        .await;
    let stopped = port.session_view(&alice, Some(1));
    assert_eq!(stopped["session"]["state"], "stopped");
    assert_eq!(stopped["session"]["processRunning"], false);
}

#[test]
fn the_prompt_section_is_the_servers_instructions_made_true_for_ironclaw() {
    let plain = crate::prompt::instructions(false);
    assert!(plain.starts_with("Computer Use tools let you interact with macOS apps"));
    assert!(plain.contains("IronClaw ends the computer use session after each assistant turn"));
    assert!(!plain.contains("Codex will automatically stop"));
    // The server's "use tool_search" hint is gone; ours says not to.
    assert!(!plain.contains("use tool_search"));
    assert!(plain.contains("do not call `tool_search`"));
    assert!(plain.contains("approves computer use once per task"));
    assert!(plain.contains("`computer_use_get_app_state`"));
    assert!(!plain.contains("tree diff"));
    let fork = crate::prompt::instructions(true);
    assert!(fork.contains("Find a control with `query`"));
    assert!(fork.contains("type_text method=\"paste\""));
}

#[test]
fn a_tree_diff_keeps_unchanged_labels_and_a_full_tree_replaces_them() {
    let mut target = crate::target::TargetState::default();
    let full = "App=com.apple.mail (pid 7)\nWindow: \"Inbox\", App: Mail.\n\t1 button Send ID: send\n\t2 text field To Help: who ID: to, Secondary Actions: Raise\n\t3 button Description: Reply, ID: r";
    assert!(target.absorb(full, None));
    assert_eq!(
        (
            target.bundle.as_str(),
            target.app.as_str(),
            target.title.as_str()
        ),
        ("com.apple.mail", "Mail", "Inbox")
    );
    assert_eq!(target.pid, Some(7));
    assert_eq!(target.elements["1"], "button Send");
    assert_eq!(target.elements["2"], "text field To");
    assert_eq!(target.elements["3"], "button Reply");
    assert!(target.same_app(Some(&json!("mail"))));
    assert!(target.same_app(Some(&json!("com.apple.mail"))));
    let diff = "App=com.apple.mail (pid 7)\n(tree diff: 1 changed)\n+ 2 text field Cc";
    assert!(target.absorb(diff, None));
    assert_eq!(target.elements["1"], "button Send", "unchanged line kept");
    assert_eq!(target.elements["2"], "text field Cc");
    assert!(!target.absorb("no header here", None));
    let png = [
        &b"\x89PNG\r\n\x1a\n"[..],
        &[0, 0, 0, 13],
        b"IHDR",
        &640u32.to_be_bytes(),
        &400u32.to_be_bytes(),
    ]
    .concat();
    assert_eq!(crate::target::png_size(&png), Some((640, 400)));
}

#[test]
fn viewer_labels_read_like_the_hermes_viewer() {
    use crate::live::describe;
    assert_eq!(
        describe(
            "click",
            &json!({"app": "Mail", "element_index": "4"}),
            Some("button Send")
        ),
        "Clicking \u{201c}button Send\u{201d}"
    );
    assert_eq!(
        describe(
            "click",
            &json!({"app": "Mail", "click_count": 2, "x": 1, "y": 2}),
            None
        ),
        "Double-clicking in Mail"
    );
    assert_eq!(
        describe(
            "click",
            &json!({"app": "Mail", "mouse_button": "right", "element_index": 9}),
            None
        ),
        "Right-clicking #9"
    );
    assert_eq!(
        describe("type_text", &json!({"text": "a".repeat(45)}), None),
        format!("Typing \u{201c}{}\u{2026}\u{201d}", "a".repeat(40))
    );
    assert_eq!(
        describe("press_key", &json!({"key": "super+c"}), None),
        "Pressing super+c"
    );
    assert_eq!(
        describe("get_app_state", &json!({}), None),
        "Looking at the app"
    );
    assert_eq!(
        describe("scroll", &json!({"element_index": "3"}), None),
        "Scrolling down #3"
    );
    assert_eq!(describe("select_text", &json!({}), None), "Select text");
}

#[test]
fn weak_model_arguments_are_repaired_against_the_tool_schema() {
    let schema = &crate::computer_use_tool("get_app_state")
        .unwrap()
        .input_schema;
    // What GLM sent on 26.10.2026: the server rejected text_limit "500".
    let mut args = json!({
        "app": "Calculator",
        "include_ax": "true",
        "max_tree_depth": "64",
        "text_limit": "500",
        "window_placement": "keep",
        "window_title": "null",
        "window_id": 0,
    });
    crate::args::coerce(schema, &mut args);
    assert_eq!(
        args,
        json!({
            "app": "Calculator",
            "include_ax": true,
            "max_tree_depth": 64,
            "text_limit": 500,
            "window_placement": "keep",
        })
    );
    // Values the schema accepts pass unchanged, enum strings included.
    let mut ok = json!({"app": "Mail", "text_limit": "max", "window_title": "Inbox"});
    crate::args::coerce(schema, &mut ok);
    assert_eq!(
        ok,
        json!({"app": "Mail", "text_limit": "max", "window_title": "Inbox"})
    );
}
