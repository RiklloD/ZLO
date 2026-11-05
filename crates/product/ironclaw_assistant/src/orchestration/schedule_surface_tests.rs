//! The desktop's schedule operations through the orchestrating surface:
//! create/update round-trips on the routines grammar (raw cron included),
//! previews, the webhook secret revealed and rotated for its owner only,
//! and nobody else reaching any of it.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::ids::{ActivityId, CapabilityId, TenantId, ThreadId, UserId};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_product_contracts::orchestration::*;
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use serde_json::{Value, json};

use super::schedules::RoutineSchedulePlanner;
use super::surface::{OrchestratedProductSurface, scope_of};

struct Inner;

#[async_trait]
impl ProductSurface for Inner {
    async fn invoke(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
    async fn query(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
    async fn stream_events(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
}

fn caller(user: &str) -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("t").unwrap(),
        UserId::new(user).unwrap(),
        None,
        None,
    )
}

struct Fixture {
    _dir: tempfile::TempDir,
    surface: OrchestratedProductSurface,
}

impl Fixture {
    async fn mutate(&self, who: &str, body: Value) -> Result<Value, ProductSurfaceError> {
        let request = ProductSurfaceInvokeRequest {
            operation_id: CapabilityId::new(ORCHESTRATOR_SCHEDULE_MUTATE_COMMAND_ID).unwrap(),
            input: body,
            activity_id: ActivityId::new(),
        };
        Ok(self.surface.invoke(caller(who), request).await?.output)
    }

    async fn view(
        &self,
        who: &str,
        view: &str,
        input: Value,
    ) -> Result<Value, ProductSurfaceError> {
        let request = ProductSurfaceQueryRequest {
            view_id: view.to_string(),
            input,
            cursor: None,
            limit: None,
        };
        let page = self.surface.query(caller(who), request).await?;
        Ok(page.items.into_iter().next().unwrap_or(Value::Null))
    }
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    engine.set_schedule_planner(Arc::new(RoutineSchedulePlanner::local()));
    engine.set_webhook_base("http://127.0.0.1:9".into());
    for (thread, user) in [("home", "owner"), ("theirs", "intruder")] {
        engine
            .ensure_thread(
                &ThreadId::new(thread).unwrap(),
                &scope_of(&caller(user)),
                None,
            )
            .await
            .unwrap();
    }
    Fixture {
        _dir: dir,
        surface: OrchestratedProductSurface::new(Arc::new(Inner), engine, None),
    }
}

#[tokio::test]
async fn schedules_round_trip_through_the_surface_for_their_owner_only() {
    let f = fixture().await;
    let cron =
        json!({"type": "cron", "expression": "0 9 * * MON-FRI", "timezone": "Europe/Zurich"});

    // Preview: three weekday 09:00 Zurich slots, nothing stored.
    let preview = f
        .view(
            "owner",
            ORCHESTRATOR_SCHEDULE_PREVIEW_VIEW.id,
            json!({"schedule": cron, "prompt": "summarize my inbox"}),
        )
        .await
        .unwrap();
    let runs: Vec<i64> = preview["nextRuns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert_eq!(runs.len(), 3, "{preview}");
    assert!(runs.windows(2).all(|w| w[1] > w[0]));
    assert_eq!(preview["describe"], "cron 0 9 * * MON-FRI (Europe/Zurich)");
    let bad = f
        .view(
            "owner",
            ORCHESTRATOR_SCHEDULE_PREVIEW_VIEW.id,
            json!({"schedule": {"type": "cron", "expression": "every day"}}),
        )
        .await
        .unwrap();
    assert_eq!(bad["ok"], false, "{bad}");
    let listed = f
        .view("owner", ORCHESTRATOR_SCHEDULES_VIEW.id, json!({}))
        .await
        .unwrap();
    assert_eq!(listed["schedules"], json!([]));

    // Create (paused), read back, update, read back.
    let created = f
        .mutate(
            "owner",
            json!({"action": "create", "thread_id": "home", "title": "Inbox",
                   "prompt": "summarize my inbox", "schedule": cron,
                   "bind_to_current_thread": false, "enabled": false}),
        )
        .await
        .unwrap();
    assert_eq!(created["ok"], true, "{created}");
    assert_eq!(created["schedule"]["enabled"], false);
    assert_eq!(created["schedule"]["bindToCurrentThread"], false);
    let id = created["schedule"]["id"].as_str().unwrap().to_string();
    let updated = f
        .mutate(
            "owner",
            json!({"action": "update", "schedule_id": id, "title": "Morning inbox",
                   "enabled": true,
                   "schedule": {"type": "fixed_time", "timeOfDay": "08:30", "weekdays": [1, 2, 3, 4, 5],
                                "timezone": "Europe/Zurich"}}),
        )
        .await
        .unwrap();
    assert_eq!(updated["schedule"]["title"], "Morning inbox", "{updated}");
    assert_eq!(updated["schedule"]["enabled"], true);
    assert!(updated["schedule"]["nextRunAt"].is_i64());
    let listed = f
        .view("owner", ORCHESTRATOR_SCHEDULES_VIEW.id, json!({}))
        .await
        .unwrap();
    assert_eq!(listed["schedules"][0]["title"], "Morning inbox");
    assert_eq!(
        listed["schedules"][0]["describe"],
        "weekdays at 08:30 (Europe/Zurich)"
    );

    // Correctable validation comes back as the error document.
    let invalid = f
        .mutate(
            "owner",
            json!({"action": "update", "schedule_id": id,
                   "schedule": {"type": "interval", "everyMs": 10}}),
        )
        .await
        .unwrap();
    assert_eq!(invalid["ok"], false, "{invalid}");

    // A webhook with a backend-made secret: masked in lists, revealed on request.
    let hook = f
        .mutate(
            "owner",
            json!({"action": "create", "thread_id": "home", "title": "Deploys",
                   "prompt": "A deploy happened: {{body}}", "schedule": {"type": "webhook"},
                   "signature": {"header": "x-zlo-signature", "prefix": "sha256=", "generate": true}}),
        )
        .await
        .unwrap();
    let hook_id = hook["schedule"]["id"].as_str().unwrap().to_string();
    let secret_req = json!({"schedule_id": hook_id});
    let revealed = f
        .view(
            "owner",
            ORCHESTRATOR_SCHEDULE_SECRET_VIEW.id,
            secret_req.clone(),
        )
        .await
        .unwrap();
    let secret = revealed["signature"]["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let listed = f
        .view("owner", ORCHESTRATOR_SCHEDULES_VIEW.id, json!({}))
        .await
        .unwrap();
    assert!(!listed.to_string().contains(&secret), "{listed}");
    assert!(listed["schedules"][1]["webhookUrlMasked"].is_string());
    let rotated = f
        .mutate(
            "owner",
            json!({"action": "rotate_secret", "schedule_id": hook_id}),
        )
        .await
        .unwrap();
    assert!(!rotated.to_string().contains(&secret));
    let again = f
        .view(
            "owner",
            ORCHESTRATOR_SCHEDULE_SECRET_VIEW.id,
            secret_req.clone(),
        )
        .await
        .unwrap();
    assert_ne!(again["signature"]["secret"].as_str().unwrap(), secret);

    // Turning signing off: an explicit null reaches the engine (absent keeps it).
    let kept = f
        .mutate(
            "owner",
            json!({"action": "update", "schedule_id": hook_id, "title": "Deploys!"}),
        )
        .await
        .unwrap();
    assert_eq!(kept["schedule"]["signature"]["hasSecret"], true, "{kept}");
    let unsigned = f
        .mutate(
            "owner",
            json!({"action": "update", "schedule_id": hook_id, "signature": null}),
        )
        .await
        .unwrap();
    assert!(unsigned["schedule"]["signature"].is_null(), "{unsigned}");

    // Strangers: no list, no reveal, no edit, no schedule on someone's thread.
    let theirs = f
        .view("intruder", ORCHESTRATOR_SCHEDULES_VIEW.id, json!({}))
        .await
        .unwrap();
    assert_eq!(theirs["schedules"], json!([]));
    assert!(
        f.view("intruder", ORCHESTRATOR_SCHEDULE_SECRET_VIEW.id, secret_req)
            .await
            .is_err()
    );
    for body in [
        json!({"action": "update", "schedule_id": id, "title": "pwned"}),
        json!({"action": "delete", "schedule_id": id}),
        json!({"action": "rotate_secret", "schedule_id": hook_id}),
        json!({"action": "create", "thread_id": "home", "prompt": "x",
               "schedule": {"type": "webhook"}}),
    ] {
        assert!(f.mutate("intruder", body).await.is_err());
    }

    // Delete.
    let deleted = f
        .mutate("owner", json!({"action": "delete", "schedule_id": id}))
        .await
        .unwrap();
    assert_eq!(deleted["ok"], true);
    let listed = f
        .view("owner", ORCHESTRATOR_SCHEDULES_VIEW.id, json!({}))
        .await
        .unwrap();
    assert_eq!(listed["schedules"].as_array().unwrap().len(), 1);
}
