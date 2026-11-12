//! The desktop's schedule surface: list by owner, preview, a server-made
//! signing secret revealed only on request, rotation, and the last webhook
//! delivery and run thread on each row.
// untriaged discards: predate the workspace-wide deny (Cargo.toml [workspace.lints.clippy]).
#![allow(clippy::let_underscore_must_use)]

use hmac::{Hmac, KeyInit, Mac};

use super::*;
use crate::schedule_webhooks::{WebhookRefusal, WebhookRequest};

fn signed(secret: &str, body: &str) -> WebhookRequest {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    WebhookRequest {
        method: "POST".into(),
        path: "/".into(),
        headers: vec![(
            "x-zlo-signature".into(),
            format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
        )],
        query: vec![],
        body: body.as_bytes().to_vec(),
    }
}

#[tokio::test]
async fn previews_list_reveal_rotate_and_record_the_last_delivery() {
    let h = harness().await;
    h.engine.set_webhook_base("http://127.0.0.1:9".into());
    h.engine.set_schedule_planner(Arc::new(FakePlanner));
    let a = tid("a");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();

    // Preview: the canonical spec, its label and the next three slots.
    let spec = json!({"type": "interval", "everyMs": 3_600_000});
    let preview = h
        .engine
        .schedule_preview(&spec, Some("check mail"))
        .unwrap();
    let runs = preview["nextRuns"].as_array().unwrap();
    assert_eq!(runs.len(), 3, "{preview}");
    assert_eq!(
        runs[1].as_i64().unwrap() - runs[0].as_i64().unwrap(),
        3_600_000
    );
    assert_eq!(preview["describe"], "test");
    assert!(h.engine.schedule_preview(&spec, Some("evil")).is_err());
    let hook_preview = h
        .engine
        .schedule_preview(&json!({"type": "webhook"}), None)
        .unwrap();
    assert_eq!(hook_preview["nextRuns"], json!([]));

    // A webhook whose signing secret the backend makes.
    let created = h
        .engine
        .schedule_action(
            &ctx("a"),
            json!({"action": "create", "title": "Hook", "prompt": "Push: {{body}}",
                   "schedule": {"type": "webhook"},
                   "signature": {"header": "X-Zlo-Signature", "prefix": "sha256=", "generate": true}}),
        )
        .await
        .unwrap();
    let id = created["schedule"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["schedule"]["signature"]["hasSecret"], true);
    let sid = ScheduleId::new(&id).unwrap();

    // Listing by owner masks the URL's token and never carries the secret.
    let list = h.engine.schedule_list_for(&scope()).await.unwrap();
    let row = &list["schedules"][0];
    assert_eq!(row["id"], id.as_str());
    assert!(row.get("webhookUrl").is_none(), "{row}");
    assert!(
        row["webhookUrlMasked"]
            .as_str()
            .unwrap()
            .ends_with(&format!("{id}.••••")),
        "{row}"
    );
    let mut stranger = scope();
    stranger.user_id = UserId::new("u2").unwrap();
    assert_eq!(
        h.engine.schedule_list_for(&stranger).await.unwrap()["schedules"],
        json!([])
    );

    // Reveal: the full URL and the secret, for the owner only.
    let revealed = h.engine.schedule_reveal(&scope(), &sid).await.unwrap();
    let secret = revealed["signature"]["secret"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(secret.len() >= 32, "{revealed}");
    assert_eq!(revealed["signature"]["header"], "x-zlo-signature");
    let url = revealed["webhookUrl"].as_str().unwrap().to_string();
    let key = url
        .strip_prefix("http://127.0.0.1:9/api/orchestrator/webhooks/")
        .unwrap()
        .to_string();
    assert!(h.engine.schedule_reveal(&stranger, &sid).await.is_err());
    assert!(!list.to_string().contains(&secret));
    assert!(!list.to_string().contains(&key));

    // A forged request is recorded as unauthorized; a signed one runs.
    let mut forged = signed(&secret, "x");
    forged.body = b"y".to_vec();
    assert_eq!(
        h.engine.fire_webhook(&key, &forged).await,
        Err(WebhookRefusal::Unauthorized)
    );
    let row = h.engine.schedule_list_for(&scope()).await.unwrap()["schedules"][0].clone();
    assert_eq!(row["lastDelivery"]["status"], "unauthorized", "{row}");
    assert!(row["lastRunThreadId"].is_null());

    h.engine
        .fire_webhook(&key, &signed(&secret, "hello"))
        .await
        .unwrap();
    let row = h.engine.schedule_list_for(&scope()).await.unwrap()["schedules"][0].clone();
    assert_eq!(row["lastDelivery"]["status"], "accepted", "{row}");
    assert!(row["lastDelivery"]["at"].is_i64());
    assert_eq!(row["lastRunThreadId"], "a");

    // A wrong token touches nothing.
    let (hook_id, _) = key.split_once('.').unwrap();
    let _ = h
        .engine
        .fire_webhook(&format!("{hook_id}.nope"), &forged)
        .await;
    let row = h.engine.schedule_list_for(&scope()).await.unwrap()["schedules"][0].clone();
    assert_eq!(row["lastDelivery"]["status"], "accepted");

    // Rotation: a new secret; the old one stops working.
    h.engine
        .schedule_rotate_secret(&scope(), &sid)
        .await
        .unwrap();
    let rotated = h.engine.schedule_reveal(&scope(), &sid).await.unwrap();
    let fresh = rotated["signature"]["secret"].as_str().unwrap();
    assert_ne!(fresh, secret);
    assert_eq!(rotated["webhookUrl"], url.as_str(), "the URL stays");
    assert_eq!(
        h.engine.fire_webhook(&key, &signed(&secret, "z")).await,
        Err(WebhookRefusal::Unauthorized)
    );
    h.engine
        .fire_webhook(&key, &signed(fresh, "z"))
        .await
        .unwrap();
    assert!(
        h.engine
            .schedule_rotate_secret(&stranger, &sid)
            .await
            .is_err()
    );

    // Clock schedules carry no secret to rotate.
    let clock = h
        .engine
        .schedule_action(
            &ctx("a"),
            json!({"action": "create", "prompt": "digest", "schedule": spec}),
        )
        .await
        .unwrap();
    let clock_id = ScheduleId::new(clock["schedule"]["id"].as_str().unwrap()).unwrap();
    assert!(
        h.engine
            .schedule_rotate_secret(&scope(), &clock_id)
            .await
            .is_err()
    );
}

async fn signed_hook(h: &Harness) -> (ScheduleId, String) {
    h.engine.set_webhook_base("http://127.0.0.1:9".into());
    h.engine.set_schedule_planner(Arc::new(FakePlanner));
    let a = tid("a");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    let created = h
        .engine
        .schedule_action(
            &ctx("a"),
            json!({"action": "create", "prompt": "hook", "schedule": {"type": "webhook"},
                   "signature": {"header": "x-zlo-signature", "prefix": "sha256=", "generate": true}}),
        )
        .await
        .unwrap();
    let id = ScheduleId::new(created["schedule"]["id"].as_str().unwrap()).unwrap();
    let secret = h.engine.schedule_reveal(&scope(), &id).await.unwrap()["signature"]["secret"]
        .as_str()
        .unwrap()
        .to_string();
    (id, secret)
}

/// The model's `t3_schedule` may not replace, regenerate or strip an
/// existing signing secret: rotation and removal are the owner's, on the
/// Automations page.
#[tokio::test]
async fn model_updates_cannot_replace_or_strip_an_existing_signature() {
    let h = harness().await;
    let (id, secret) = signed_hook(&h).await;
    for signature in [
        json!({"header": "x-zlo-signature", "generate": true}),
        json!({"header": "x-zlo-signature", "secret": "chosen-by-the-model"}),
        serde_json::Value::Null,
    ] {
        let answer = h
            .engine
            .call_tool(
                "t3_schedule",
                &ctx("a"),
                &json!({"action": "update", "id": id.as_str(), "signature": signature}),
            )
            .await;
        assert_eq!(answer["ok"], false, "{answer}");
        let now = h.engine.schedule_reveal(&scope(), &id).await.unwrap();
        assert_eq!(now["signature"]["secret"], secret.as_str(), "{signature}");
    }
    // Other fields still update through the tool.
    let renamed = h
        .engine
        .call_tool(
            "t3_schedule",
            &ctx("a"),
            &json!({"action": "update", "id": id.as_str(), "title": "Renamed"}),
        )
        .await;
    assert_eq!(renamed["schedule"]["title"], "Renamed", "{renamed}");
    assert_eq!(renamed["schedule"]["signature"]["hasSecret"], true);
}

/// The owner turning signing off: `signature: null` removes it (absent keeps it).
#[tokio::test]
async fn owner_null_signature_removes_it_and_absent_keeps_it() {
    let h = harness().await;
    let (id, _) = signed_hook(&h).await;
    let kept = h
        .engine
        .schedule_action(
            &ctx("a"),
            json!({"action": "update", "id": id.as_str(), "title": "t"}),
        )
        .await
        .unwrap();
    assert_eq!(kept["schedule"]["signature"]["hasSecret"], true, "{kept}");
    let off = h
        .engine
        .schedule_action(
            &ctx("a"),
            json!({"action": "update", "id": id.as_str(), "signature": null}),
        )
        .await
        .unwrap();
    assert!(off["schedule"]["signature"].is_null(), "{off}");
    assert!(h.engine.schedule_reveal(&scope(), &id).await.unwrap()["signature"].is_null());
}

/// A schedule created paused is paused from its first write: a tick right
/// after the insert fires nothing (FakePlanner makes it due immediately).
#[tokio::test]
async fn paused_create_is_paused_from_the_first_write() {
    let h = harness().await;
    h.engine.set_schedule_planner(Arc::new(FakePlanner));
    let a = tid("a");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    let created = h
        .engine
        .schedule_action(
            &ctx("a"),
            json!({"action": "create", "prompt": "digest", "enabled": false,
                   "schedule": {"type": "interval", "everyMs": 3_600_000}}),
        )
        .await
        .unwrap();
    assert_eq!(created["schedule"]["enabled"], false, "{created}");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    h.engine.tick().await;
    assert!(h.engine.store().queue(&a, false).await.unwrap().is_empty());
}
