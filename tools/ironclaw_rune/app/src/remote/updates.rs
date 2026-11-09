//! "Check for updates" and "Update all machines" (Settings → Computers):
//! every saved computer, and This Mac, through its own backend's operator
//! self-update routes (`zlo_client::update_calls`). Each machine updates
//! itself from its local `[update]` config; ZLO only says "go" and watches.
//!
//! Update all runs the other computers in parallel (start, then poll the
//! status, riding out the backend going away while it restarts and finding
//! it again through its descriptor), then This Mac last: its update
//! reinstalls and reopens ZLO itself.
//!
//! The rows live in `State`, so they survive closing the window, not an
//! app restart.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::future::{BoxFuture, join_all};
use zlo_client::environment_calls::fetch_descriptor;
use zlo_client::environments::{
    RouteKind, SavedEnvironment, THIS_MAC, check_descriptor, check_transport, preferred_routes,
};
use zlo_client::keychain::TokenStore;
use zlo_client::self_update::{UpdateMethod, UpdateRunState, UpdateStatus};
use zlo_client::update_calls::UpdateCallError;
use zlo_client::{Client, Connection, Token};

use crate::state::State;

const DESCRIPTOR_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_EVERY: Duration = Duration::from_secs(3);
/// A release build of IronClaw (or ZLO's bundle) can take a while.
const GIVE_UP_AFTER: Duration = Duration::from_secs(45 * 60);

/// How to turn self-update on, for a computer that has it off.
pub const ENABLE_HINT: &str = "On that computer, add to ~/.ironclaw/reborn/config.toml:  [update]  method = \"source\"  repo = \"/path/to/ironclaw\"  — then restart IronClaw there.";

/// What one computer's row says about updates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowState {
    Checking,
    UpToDate,
    Behind(u32),
    /// `[update] method = "none"` there.
    NotSetUp,
    /// The step in progress.
    Updating(String),
    /// The backend's own outcome line.
    Updated(String),
    Failed(String),
}

impl RowState {
    pub fn label(&self) -> String {
        match self {
            Self::Checking => "Checking…".into(),
            Self::UpToDate => "Up to date".into(),
            Self::Behind(1) => "1 commit behind".into(),
            Self::Behind(n) => format!("{n} commits behind"),
            Self::NotSetUp => "Not set up (method none)".into(),
            Self::Updating(step) => format!("Updating… {step}"),
            Self::Updated(line) => format!("Updated — {line}"),
            Self::Failed(why) => format!("Failed: {why}"),
        }
    }
}

#[derive(Default)]
pub struct UpdatesState {
    /// Per computer key (`this-mac` or an environment id).
    pub rows: HashMap<String, RowState>,
    pub checking: bool,
    pub updating: bool,
}

// ── decisions (pure) ─────────────────────────────────────────────────────

/// A check's answer as a row.
pub fn from_check(result: Result<&UpdateStatus, &UpdateCallError>) -> RowState {
    match result {
        Ok(status) if status.method == UpdateMethod::None => RowState::NotSetUp,
        Ok(status) if status.last_run.state == UpdateRunState::Running => {
            RowState::Updating(status.last_run.message.clone().unwrap_or_default())
        }
        Ok(status) => match &status.available {
            Some(a) if a.behind > 0 => RowState::Behind(a.behind),
            _ => RowState::UpToDate,
        },
        Err(UpdateCallError::NotSetUp(_)) => RowState::NotSetUp,
        Err(UpdateCallError::Running) => RowState::Updating(String::new()),
        Err(e) => RowState::Failed(e.to_string()),
    }
}

/// Keep polling, or the job is over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Poll {
    Continue(RowState),
    Done(RowState),
}

/// One status read while updating. `since`: when the job we started (or
/// found running) began; an older record is the previous job's.
pub fn after_poll(since: Option<DateTime<Utc>>, status: &UpdateStatus) -> Poll {
    let run = &status.last_run;
    let ours = match (since, run.started_at) {
        (Some(since), Some(started)) => started >= since,
        (None, _) => true,
        (Some(_), None) => false,
    };
    let message = run.message.clone().unwrap_or_default();
    if !ours {
        return Poll::Continue(RowState::Updating("Starting".into()));
    }
    match run.state {
        UpdateRunState::Succeeded => Poll::Done(RowState::Updated(message)),
        UpdateRunState::Failed => Poll::Done(RowState::Failed(message)),
        UpdateRunState::Running => Poll::Continue(RowState::Updating(message)),
        UpdateRunState::Idle => Poll::Continue(RowState::Updating("Starting".into())),
    }
}

// ── machines ─────────────────────────────────────────────────────────────

/// One computer's update routes, found again when its backend comes back.
pub(crate) trait Machine: Send + Sync {
    fn key(&self) -> &str;
    fn start(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>>;
    fn status(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>>;
    fn check(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>>;
}

pub(crate) type Report = Arc<dyn Fn(String, RowState) + Send + Sync>;

#[derive(Clone, Copy)]
pub(crate) struct Timing {
    pub poll_every: Duration,
    pub give_up_after: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll_every: POLL_EVERY,
            give_up_after: GIVE_UP_AFTER,
        }
    }
}

/// Check every machine at once.
pub(crate) async fn check_all(machines: Vec<Arc<dyn Machine>>, report: Report) {
    join_all(machines.into_iter().map(|m| {
        let report = report.clone();
        async move {
            report(m.key().to_string(), RowState::Checking);
            let result = m.check().await;
            report(m.key().to_string(), from_check(result.as_ref()));
        }
    }))
    .await;
}

/// The other computers in parallel, then This Mac.
pub(crate) async fn update_all(
    remotes: Vec<Arc<dyn Machine>>,
    this_mac: Option<Arc<dyn Machine>>,
    report: Report,
    timing: Timing,
) {
    join_all(
        remotes
            .into_iter()
            .map(|m| update_one(m, report.clone(), timing)),
    )
    .await;
    if let Some(mac) = this_mac {
        update_one(mac, report, timing).await;
    }
}

async fn update_one(m: Arc<dyn Machine>, report: Report, timing: Timing) {
    let key = m.key().to_string();
    let say = |state: RowState| report(key.clone(), state);
    say(RowState::Updating("Starting".into()));
    let since = match m.start().await {
        Ok(status) => status.last_run.started_at,
        // Someone else started one: watch that job.
        Err(UpdateCallError::Running) => match m.status().await {
            Ok(status) => status.last_run.started_at,
            Err(_) => None,
        },
        Err(UpdateCallError::NotSetUp(_)) => return say(RowState::NotSetUp),
        Err(e) => return say(RowState::Failed(e.to_string())),
    };
    let deadline = tokio::time::Instant::now() + timing.give_up_after;
    loop {
        tokio::time::sleep(timing.poll_every).await;
        match m.status().await {
            Ok(status) => match after_poll(since, &status) {
                Poll::Continue(state) => say(state),
                Poll::Done(state) => return say(state),
            },
            Err(UpdateCallError::Unauthorized) => {
                return say(RowState::Failed(UpdateCallError::Unauthorized.to_string()));
            }
            // Gone while it restarts: keep asking.
            Err(_) => say(RowState::Updating(
                "Restarting; waiting for it to answer".into(),
            )),
        }
        if tokio::time::Instant::now() >= deadline {
            return say(RowState::Failed(format!(
                "no outcome after {} minutes",
                timing.give_up_after.as_secs() / 60
            )));
        }
    }
}

/// A computer as ZLO reaches it: tailnet or direct routes found through the
/// descriptor (with the Keychain token), else the live connection when it
/// is the one in use (an SSH-only computer), else This Mac's local backend.
enum Reach {
    ThisMac,
    Saved(SavedEnvironment),
}

struct LiveMachine {
    key: String,
    reach: Reach,
    tokens: Arc<dyn TokenStore>,
    /// The connection in use when this computer is the active one.
    live: Option<Client>,
    cached: Mutex<Option<Client>>,
}

impl LiveMachine {
    async fn resolve(&self) -> Result<Client, UpdateCallError> {
        match &self.reach {
            Reach::ThisMac => match Connection::discover() {
                Ok(conn) => Client::new(conn).map_err(|e| UpdateCallError::Other(e.to_string())),
                Err(_) => self.live.clone().ok_or_else(|| {
                    UpdateCallError::Unreachable("IronClaw isn't running on this Mac".into())
                }),
            },
            Reach::Saved(env) => match resolve_saved(env, &self.tokens).await {
                Ok(client) => Ok(client),
                Err(e) => self.live.clone().ok_or(e),
            },
        }
    }

    async fn client(&self, fresh: bool) -> Result<Client, UpdateCallError> {
        if !fresh && let Some(c) = self.cached.lock().ok().and_then(|g| g.clone()) {
            return Ok(c);
        }
        let client = self.resolve().await?;
        if let Ok(mut slot) = self.cached.lock() {
            *slot = Some(client.clone());
        }
        Ok(client)
    }

    /// One call; after a miss, find the backend again (it may have come
    /// back on another route) and retry once.
    async fn call<F>(&self, f: F) -> Result<UpdateStatus, UpdateCallError>
    where
        F: for<'c> Fn(&'c Client) -> BoxFuture<'c, Result<UpdateStatus, UpdateCallError>>
            + Send
            + Sync,
    {
        let client = self.client(false).await?;
        match f(&client).await {
            Err(UpdateCallError::Unreachable(_)) => {
                let client = self.client(true).await?;
                f(&client).await
            }
            other => other,
        }
    }
}

async fn resolve_saved(
    env: &SavedEnvironment,
    tokens: &Arc<dyn TokenStore>,
) -> Result<Client, UpdateCallError> {
    let id = env.environment_id.clone();
    let label = env.label.clone();
    let lookup = {
        let (tokens, id) = (tokens.clone(), id.clone());
        tokio::task::spawn_blocking(move || tokens.get(&id))
            .await
            .map_err(|e| UpdateCallError::Other(format!("Keychain: {e}")))?
    };
    let token = match lookup {
        Ok(Some(t)) => Token::new(t).map_err(|e| UpdateCallError::Other(e.to_string()))?,
        Ok(None) => return Err(UpdateCallError::Unauthorized),
        Err(e) => return Err(UpdateCallError::Other(e.to_string())),
    };
    let mut last = UpdateCallError::Unreachable(format!(
        "{label} has no tailnet or direct address (connect to it to update over SSH)"
    ));
    // SSH routes need the one tunnel the connection in use owns.
    for route in preferred_routes(&env.routes) {
        if route.kind == RouteKind::Ssh {
            continue;
        }
        let Ok(origin) = check_transport(&route.url) else {
            continue;
        };
        match fetch_descriptor(&origin, DESCRIPTOR_TIMEOUT).await {
            Ok(d) => {
                check_descriptor(&d, Some((&id, &label)))
                    .map_err(|e| UpdateCallError::Other(e.to_string()))?;
                return Client::new(Connection {
                    base_url: origin,
                    token,
                    token_source: "Keychain".into(),
                })
                .map_err(|e| UpdateCallError::Other(e.to_string()));
            }
            Err(e) => last = UpdateCallError::Unreachable(e.to_string()),
        }
    }
    Err(last)
}

impl Machine for LiveMachine {
    fn key(&self) -> &str {
        &self.key
    }

    fn start(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>> {
        Box::pin(self.call(|c| Box::pin(c.start_update())))
    }

    fn status(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>> {
        Box::pin(self.call(|c| Box::pin(c.update_status())))
    }

    fn check(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>> {
        Box::pin(self.call(|c| Box::pin(c.check_update())))
    }
}

// ── the page's actions ───────────────────────────────────────────────────

/// The machines to act on: the saved computers, and This Mac unless this is
/// a Light bundle.
/// The other computers, and This Mac (absent in a Light bundle).
struct Fleet {
    remotes: Vec<Arc<dyn Machine>>,
    this_mac: Option<Arc<dyn Machine>>,
}

fn machines(s: &State) -> Fleet {
    let live = s.rt.backend.as_ref().map(|b| b.client().clone());
    let active = s.onboarding.computer.key();
    let remote = super::backend_is_remote(s);
    let remotes = super::saved(s)
        .iter()
        .map(|env| {
            Arc::new(LiveMachine {
                key: env.environment_id.clone(),
                reach: Reach::Saved(env.clone()),
                tokens: s.remote.tokens.clone(),
                live: (remote && active == env.environment_id)
                    .then(|| live.clone())
                    .flatten(),
                cached: Mutex::new(None),
            }) as Arc<dyn Machine>
        })
        .collect();
    let this_mac = (!s.remote.app_only).then(|| {
        Arc::new(LiveMachine {
            key: THIS_MAC.into(),
            reach: Reach::ThisMac,
            tokens: s.remote.tokens.clone(),
            live: (!remote).then(|| live.clone()).flatten(),
            cached: Mutex::new(None),
        }) as Arc<dyn Machine>
    });
    Fleet { remotes, this_mac }
}

fn reporter(s: &State) -> Report {
    let handle = s.rt.handle.clone();
    Arc::new(move |key: String, state: RowState| {
        handle.update(move |s, _| {
            s.remote.updates.rows.insert(key, state);
        });
    })
}

/// "Check for updates".
pub fn check(s: &mut State) {
    if s.fixture || s.remote.updates.checking || s.remote.updates.updating {
        return;
    }
    let Some(net) = super::net() else {
        return;
    };
    let Fleet {
        remotes: mut all,
        this_mac,
    } = machines(s);
    all.extend(this_mac);
    s.remote.updates.checking = true;
    let report = reporter(s);
    let handle = s.rt.handle.clone();
    net.spawn(async move {
        check_all(all, report).await;
        handle.update(|s, _| s.remote.updates.checking = false);
    });
}

/// "Update all machines".
pub fn update_everything(s: &mut State) {
    if s.fixture || s.remote.updates.updating {
        return;
    }
    let Some(net) = super::net() else {
        return;
    };
    let Fleet { remotes, this_mac } = machines(s);
    s.remote.updates.updating = true;
    let report = reporter(s);
    let handle = s.rt.handle.clone();
    net.spawn(async move {
        update_all(remotes, this_mac, report, Timing::default()).await;
        handle.update(|s, _| s.remote.updates.updating = false);
    });
}

#[cfg(test)]
mod tests {
    use zlo_client::self_update::{AvailableUpdate, BuildInfo, UpdateRun};

    use super::*;

    fn at(min: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 11, 8, 21, min, 0).unwrap()
    }

    fn status(state: UpdateRunState, started: Option<u32>, message: &str) -> UpdateStatus {
        UpdateStatus {
            method: UpdateMethod::Source,
            current: BuildInfo {
                version: "1.2.0".into(),
                commit: None,
            },
            available: None,
            last_run: UpdateRun {
                state,
                started_at: started.map(at),
                finished_at: None,
                message: Some(message.into()),
                log_tail: String::new(),
            },
        }
    }

    #[test]
    fn check_rows() {
        let mut behind = status(UpdateRunState::Idle, None, "");
        behind.available = Some(AvailableUpdate {
            commit: "b".repeat(40),
            behind: 3,
            subjects: vec![],
            checked_at: at(0),
        });
        assert_eq!(from_check(Ok(&behind)), RowState::Behind(3));
        assert_eq!(
            from_check(Ok(&status(UpdateRunState::Succeeded, Some(0), "x"))),
            RowState::UpToDate
        );
        let mut off = status(UpdateRunState::Idle, None, "");
        off.method = UpdateMethod::None;
        assert_eq!(from_check(Ok(&off)), RowState::NotSetUp);
        assert_eq!(
            from_check(Err(&UpdateCallError::NotSetUp("add [update]".into()))),
            RowState::NotSetUp
        );
        assert_eq!(
            from_check(Err(&UpdateCallError::Refused(
                "The checkout has local changes".into()
            ))),
            RowState::Failed("The checkout has local changes".into())
        );
        assert_eq!(RowState::Behind(1).label(), "1 commit behind");
        assert_eq!(RowState::Behind(4).label(), "4 commits behind");
        assert_eq!(RowState::NotSetUp.label(), "Not set up (method none)");
        assert_eq!(RowState::Failed("dirty".into()).label(), "Failed: dirty");
    }

    #[test]
    fn polls_ignore_the_previous_job_and_end_on_ours() {
        let since = Some(at(10));
        assert_eq!(
            after_poll(since, &status(UpdateRunState::Succeeded, Some(1), "old")),
            Poll::Continue(RowState::Updating("Starting".into())),
            "an earlier job's success is not ours"
        );
        assert_eq!(
            after_poll(
                since,
                &status(UpdateRunState::Running, Some(10), "Building")
            ),
            Poll::Continue(RowState::Updating("Building".into()))
        );
        assert_eq!(
            after_poll(
                since,
                &status(UpdateRunState::Succeeded, Some(11), "Updated to b")
            ),
            Poll::Done(RowState::Updated("Updated to b".into()))
        );
        assert_eq!(
            after_poll(since, &status(UpdateRunState::Failed, Some(11), "dirty")),
            Poll::Done(RowState::Failed("dirty".into()))
        );
    }

    /// Scripted machine: `start` answers, then `status` answers in turn
    /// (the last one repeats). Every call goes into the shared log.
    struct Fake {
        key: String,
        log: Arc<Mutex<Vec<String>>>,
        start: Result<UpdateStatus, UpdateCallError>,
        statuses: Mutex<Vec<Result<UpdateStatus, UpdateCallError>>>,
    }

    impl Fake {
        fn machine(
            key: &str,
            log: &Arc<Mutex<Vec<String>>>,
            start: Result<UpdateStatus, UpdateCallError>,
            statuses: Vec<Result<UpdateStatus, UpdateCallError>>,
        ) -> Arc<dyn Machine> {
            Arc::new(Self {
                key: key.into(),
                log: log.clone(),
                start,
                statuses: Mutex::new(statuses),
            })
        }

        fn note(&self, what: &str) {
            self.log
                .lock()
                .unwrap()
                .push(format!("{what}:{}", self.key));
        }
    }

    impl Machine for Fake {
        fn key(&self) -> &str {
            &self.key
        }

        fn start(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>> {
            self.note("start");
            let answer = self.start.clone();
            Box::pin(async move { answer })
        }

        fn status(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>> {
            let mut q = self.statuses.lock().unwrap();
            let answer = if q.len() > 1 {
                q.remove(0)
            } else {
                q[0].clone()
            };
            if let Ok(s) = &answer
                && s.last_run.state != UpdateRunState::Running
            {
                self.note("done");
            }
            Box::pin(async move { answer })
        }

        fn check(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>> {
            self.note("check");
            let answer = self.start.clone();
            Box::pin(async move { answer })
        }
    }

    type Seen = Arc<Mutex<Vec<(String, RowState)>>>;

    fn recorder() -> (Report, Seen) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        (
            Arc::new(move |k: String, s: RowState| sink.lock().unwrap().push((k, s))),
            seen,
        )
    }

    fn fast() -> Timing {
        Timing {
            poll_every: Duration::from_millis(1),
            give_up_after: Duration::from_secs(5),
        }
    }

    fn last(seen: &[(String, RowState)], key: &str) -> RowState {
        seen.iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, s)| s.clone())
            .expect("reported")
    }

    #[tokio::test]
    async fn remotes_update_in_parallel_and_this_mac_goes_last() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let running = |m| Ok(status(UpdateRunState::Running, Some(m), "Building"));
        let studio = Fake::machine(
            "studio",
            &log,
            running(10),
            vec![
                running(10),
                Err(UpdateCallError::Unreachable("restarting".into())),
                Ok(status(
                    UpdateRunState::Succeeded,
                    Some(10),
                    "Updated to b; IronClaw restarted.",
                )),
            ],
        );
        let mini = Fake::machine(
            "mini",
            &log,
            running(10),
            vec![Ok(status(
                UpdateRunState::Failed,
                Some(10),
                "The checkout has local changes",
            ))],
        );
        let off = Fake::machine(
            "off",
            &log,
            Err(UpdateCallError::NotSetUp("add [update]".into())),
            vec![],
        );
        let mac = Fake::machine(
            THIS_MAC,
            &log,
            running(12),
            vec![Ok(status(
                UpdateRunState::Succeeded,
                Some(12),
                "Updated to b; ZLO reopened.",
            ))],
        );
        let (report, seen) = recorder();
        update_all(vec![studio, mini, off], Some(mac), report, fast()).await;

        let log = log.lock().unwrap().clone();
        let mac_start = log
            .iter()
            .position(|e| e == "start:this-mac")
            .expect("mac started");
        for remote in ["studio", "mini", "off"] {
            let started = log
                .iter()
                .position(|e| *e == format!("start:{remote}"))
                .expect("started");
            assert!(
                started < mac_start,
                "{remote} starts before This Mac: {log:?}"
            );
        }
        for remote in ["studio", "mini"] {
            let done = log
                .iter()
                .position(|e| *e == format!("done:{remote}"))
                .expect("finished");
            assert!(
                done < mac_start,
                "{remote} finishes before This Mac starts: {log:?}"
            );
        }
        assert_eq!(
            log[..3],
            ["start:studio", "start:mini", "start:off"],
            "all at once"
        );

        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.contains(&(
                "studio".into(),
                RowState::Updating("Restarting; waiting for it to answer".into())
            )),
            "the restart is ridden out"
        );
        assert_eq!(
            last(&seen, "studio"),
            RowState::Updated("Updated to b; IronClaw restarted.".into())
        );
        assert_eq!(
            last(&seen, "mini"),
            RowState::Failed("The checkout has local changes".into())
        );
        assert_eq!(last(&seen, "off"), RowState::NotSetUp);
        assert_eq!(
            last(&seen, THIS_MAC),
            RowState::Updated("Updated to b; ZLO reopened.".into())
        );
    }

    #[tokio::test]
    async fn an_update_already_running_is_watched_not_restarted() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let busy = Fake::machine(
            "studio",
            &log,
            Err(UpdateCallError::Running),
            vec![
                Ok(status(UpdateRunState::Running, Some(5), "Building")),
                Ok(status(UpdateRunState::Succeeded, Some(5), "Updated")),
            ],
        );
        let (report, seen) = recorder();
        update_all(vec![busy], None, report, fast()).await;
        assert_eq!(
            last(&seen.lock().unwrap(), "studio"),
            RowState::Updated("Updated".into())
        );
    }

    #[tokio::test]
    async fn a_machine_that_never_comes_back_fails_after_the_deadline() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let gone = Fake::machine(
            "studio",
            &log,
            Ok(status(UpdateRunState::Running, Some(1), "Building")),
            vec![Err(UpdateCallError::Unreachable("down".into()))],
        );
        let (report, seen) = recorder();
        update_all(
            vec![gone],
            None,
            report,
            Timing {
                poll_every: Duration::from_millis(1),
                give_up_after: Duration::from_millis(20),
            },
        )
        .await;
        assert!(matches!(
            last(&seen.lock().unwrap(), "studio"),
            RowState::Failed(why) if why.contains("no outcome")
        ));
    }

    #[tokio::test]
    async fn check_all_reports_every_machine() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut behind = status(UpdateRunState::Idle, None, "");
        behind.available = Some(AvailableUpdate {
            commit: "b".repeat(40),
            behind: 2,
            subjects: vec![],
            checked_at: at(0),
        });
        let a = Fake::machine("a", &log, Ok(behind), vec![]);
        let b = Fake::machine(
            "b",
            &log,
            Err(UpdateCallError::NotSetUp("x".into())),
            vec![],
        );
        let (report, seen) = recorder();
        check_all(vec![a, b], report).await;
        let seen = seen.lock().unwrap().clone();
        assert_eq!(last(&seen, "a"), RowState::Behind(2));
        assert_eq!(last(&seen, "b"), RowState::NotSetUp);
        assert!(seen.contains(&("a".into(), RowState::Checking)));
    }
}
