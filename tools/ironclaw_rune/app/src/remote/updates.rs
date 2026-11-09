//! Updates for every computer: a quiet background read that drives the
//! "Update available" prompt in the status bar, "Turn on" for a computer
//! that has updates off, and "Check for updates" / "Update all machines"
//! (Settings → Computers):
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

/// How often the rows are refreshed in the background. Each backend checks
/// its own source every couple of minutes; this only reads the answer.
pub const POLL: Duration = Duration::from_secs(60);

/// How to turn self-update on, for a computer ZLO cannot turn it on for
/// (it was not built from a source checkout that is still there).
pub const ENABLE_HINT: &str = "Where \"Turn on\" is not offered, run on that computer:  zlo update enable --repo /path/to/checkout";

/// What one computer's row says about updates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowState {
    Checking,
    UpToDate,
    Behind(u32),
    /// Updates are off there. `can_enable`: that backend knows the source
    /// checkout it was built from, so ZLO can turn them on in one click.
    NotSetUp {
        can_enable: bool,
    },
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
            Self::NotSetUp { can_enable: true } => "Updates are off".into(),
            Self::NotSetUp { can_enable: false } => {
                "Updates are off (run `zlo update enable` there)".into()
            }
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
        Ok(status) if status.method == UpdateMethod::None => RowState::NotSetUp {
            can_enable: status.can_enable,
        },
        Ok(status) if status.last_run.state == UpdateRunState::Running => {
            RowState::Updating(status.last_run.message.clone().unwrap_or_default())
        }
        Ok(status) => match &status.available {
            Some(a) if a.behind > 0 => RowState::Behind(a.behind),
            _ => RowState::UpToDate,
        },
        Err(UpdateCallError::NotSetUp(_)) => RowState::NotSetUp { can_enable: false },
        Err(UpdateCallError::Running) => RowState::Updating(String::new()),
        Err(e) => RowState::Failed(e.to_string()),
    }
}

/// A background status read as a row, or `None` to leave the row alone: a
/// computer that is asleep or offline keeps what it last said, and so does
/// a row showing why the last update failed while that is still the last
/// thing that happened there.
pub fn from_poll(
    shown: Option<&RowState>,
    result: Result<&UpdateStatus, &UpdateCallError>,
) -> Option<RowState> {
    let status = match result {
        Ok(status) => status,
        Err(UpdateCallError::NotSetUp(_)) => return Some(RowState::NotSetUp { can_enable: false }),
        Err(_) => return None,
    };
    let still_failed = matches!(shown, Some(RowState::Failed(_)))
        && status.last_run.state == UpdateRunState::Failed;
    let just_updated = matches!(shown, Some(RowState::Updated(_)))
        && status.last_run.state == UpdateRunState::Succeeded
        && status.available.is_none();
    if still_failed || just_updated {
        return None;
    }
    Some(from_check(Ok(status))).filter(|row| shown != Some(row))
}

/// How many computers have an update waiting (the status bar's prompt).
pub fn waiting(updates: &UpdatesState) -> usize {
    updates
        .rows
        .values()
        .filter(|row| matches!(row, RowState::Behind(_)))
        .count()
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
    fn enable(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>>;
    fn disable(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>>;
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
        Err(UpdateCallError::NotSetUp(_)) => {
            return say(RowState::NotSetUp { can_enable: false });
        }
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

    fn enable(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>> {
        Box::pin(self.call(|c| Box::pin(c.enable_update())))
    }

    fn disable(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>> {
        Box::pin(self.call(|c| Box::pin(c.disable_update())))
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

/// Whether `key` is still one of this Mac's computers.
fn is_member(s: &State, key: &str) -> bool {
    if key == THIS_MAC {
        return !s.remote.app_only;
    }
    super::saved(s).iter().any(|env| env.environment_id == key)
}

/// Record `key`'s row, unless that computer was removed while its answer
/// was on the way (its row would count as "Update available" forever).
pub(crate) fn set_row(s: &mut State, key: String, row: RowState) {
    if is_member(s, &key) {
        s.remote.updates.rows.insert(key, row);
    }
}

/// A computer was removed from this Mac: its row goes with it.
pub fn forget(s: &mut State, key: &str) {
    s.remote.updates.rows.remove(key);
}

fn reporter(s: &State) -> Report {
    let handle = s.rt.handle.clone();
    Arc::new(move |key: String, state: RowState| {
        handle.update(move |s, _| set_row(s, key, state));
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

/// The background refresh: read every computer's update status (no fetch
/// on this side; each backend keeps its own answer fresh) and change only
/// the rows that have something new to say.
pub fn poll(s: &mut State) {
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
    let handle = s.rt.handle.clone();
    net.spawn(async move {
        join_all(all.into_iter().map(|m| {
            let handle = handle.clone();
            async move {
                let result = m.status().await;
                let key = m.key().to_string();
                handle.update(move |s, _| {
                    let u = &s.remote.updates;
                    if u.checking || u.updating {
                        return;
                    }
                    if let Some(row) = from_poll(u.rows.get(&key), result.as_ref()) {
                        set_row(s, key, row);
                    }
                });
            }
        }))
        .await;
    });
}

/// "Turn on" for one computer: its backend turns self-update on from the
/// checkout it was built from, then checks.
pub fn enable(s: &mut State, key: &str) {
    if let Some((net, machine, report)) = one(s, key) {
        net.spawn(enable_one(machine, report));
    }
}

/// "Turn off" for one computer: it stops checking and is left out of
/// "Update all machines" until turned on again.
pub fn disable(s: &mut State, key: &str) {
    if let Some((net, machine, report)) = one(s, key) {
        net.spawn(disable_one(machine, report));
    }
}

/// The machine `key` names, with what a single-computer action needs.
fn one(
    s: &State,
    key: &str,
) -> Option<(&'static tokio::runtime::Runtime, Arc<dyn Machine>, Report)> {
    if s.fixture || s.remote.updates.updating {
        return None;
    }
    let net = super::net()?;
    let Fleet { remotes, this_mac } = machines(s);
    let machine = remotes
        .into_iter()
        .chain(this_mac)
        .find(|m| m.key() == key)?;
    Some((net, machine, reporter(s)))
}

/// Whether "Turn off" is offered on a row: updates are on there and
/// nothing is in flight.
pub fn can_turn_off(row: &RowState) -> bool {
    matches!(
        row,
        RowState::UpToDate | RowState::Behind(_) | RowState::Updated(_)
    )
}

pub(crate) async fn disable_one(m: Arc<dyn Machine>, report: Report) {
    let key = m.key().to_string();
    let before = m.status().await;
    report(key.clone(), RowState::Checking);
    let row = match m.disable().await {
        Ok(status) => from_check(Ok(&status)),
        // A backend without the route: nothing changed there.
        Err(UpdateCallError::TooOld) => match before {
            Ok(status) => from_check(Ok(&status)),
            Err(e) => RowState::Failed(e.to_string()),
        },
        Err(e) => RowState::Failed(e.to_string()),
    };
    report(key, row);
}

pub(crate) async fn enable_one(m: Arc<dyn Machine>, report: Report) {
    let key = m.key().to_string();
    report(key.clone(), RowState::Checking);
    let row = match m.enable().await {
        // On now: see whether anything is waiting.
        Ok(_) => from_check(m.check().await.as_ref()),
        Err(UpdateCallError::TooOld) => RowState::NotSetUp { can_enable: false },
        Err(e) => RowState::Failed(e.to_string()),
    };
    report(key, row);
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
            can_enable: false,
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
        assert_eq!(
            from_check(Ok(&off)),
            RowState::NotSetUp { can_enable: false }
        );
        off.can_enable = true;
        assert_eq!(
            from_check(Ok(&off)),
            RowState::NotSetUp { can_enable: true }
        );
        assert_eq!(
            from_check(Err(&UpdateCallError::NotSetUp("add [update]".into()))),
            RowState::NotSetUp { can_enable: false }
        );
        assert_eq!(
            from_check(Err(&UpdateCallError::Refused(
                "The checkout has local changes".into()
            ))),
            RowState::Failed("The checkout has local changes".into())
        );
        assert_eq!(RowState::Behind(1).label(), "1 commit behind");
        assert_eq!(RowState::Behind(4).label(), "4 commits behind");
        assert_eq!(
            RowState::NotSetUp { can_enable: true }.label(),
            "Updates are off"
        );
        assert_eq!(RowState::Failed("dirty".into()).label(), "Failed: dirty");
    }

    fn behind(n: u32) -> UpdateStatus {
        let mut status = status(UpdateRunState::Idle, None, "");
        status.available = Some(AvailableUpdate {
            commit: "b".repeat(40),
            behind: n,
            subjects: vec![],
            checked_at: at(0),
        });
        status
    }

    #[test]
    fn the_background_read_only_changes_rows_with_something_new() {
        // Nothing shown yet: the first read fills the row.
        assert_eq!(from_poll(None, Ok(&behind(2))), Some(RowState::Behind(2)));
        // The same answer again changes nothing (no repaint).
        assert_eq!(from_poll(Some(&RowState::Behind(2)), Ok(&behind(2))), None);
        assert_eq!(
            from_poll(Some(&RowState::Behind(2)), Ok(&behind(3))),
            Some(RowState::Behind(3))
        );
        // Asleep or offline: the row keeps what it last said.
        let asleep = UpdateCallError::Unreachable("no route".into());
        assert_eq!(from_poll(Some(&RowState::Behind(2)), Err(&asleep)), None);
        assert_eq!(from_poll(None, Err(&asleep)), None);
        // Why the last update failed stays until something else happens there.
        let mut failed = behind(2);
        failed.last_run.state = UpdateRunState::Failed;
        assert_eq!(
            from_poll(Some(&RowState::Failed("dirty".into())), Ok(&failed)),
            None
        );
        assert_eq!(
            from_poll(Some(&RowState::Failed("dirty".into())), Ok(&behind(2))),
            Some(RowState::Behind(2)),
            "a newer check replaces an old failure"
        );
        // "Updated" stays until there is a newer update.
        let done = status(UpdateRunState::Succeeded, Some(1), "Updated to b");
        assert_eq!(
            from_poll(Some(&RowState::Updated("Updated to b".into())), Ok(&done)),
            None
        );
        let mut newer = behind(1);
        newer.last_run.state = UpdateRunState::Succeeded;
        assert_eq!(
            from_poll(Some(&RowState::Updated("Updated to b".into())), Ok(&newer)),
            Some(RowState::Behind(1))
        );

        let mut u = UpdatesState::default();
        assert_eq!(waiting(&u), 0);
        u.rows.insert("a".into(), RowState::Behind(2));
        u.rows.insert("b".into(), RowState::UpToDate);
        u.rows.insert("c".into(), RowState::Behind(1));
        assert_eq!(waiting(&u), 2);
    }

    #[tokio::test]
    async fn turn_off_reads_as_off_and_an_old_backend_keeps_its_row() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (report, seen) = recorder();
        let studio = Arc::new(Fake {
            key: "studio".into(),
            log: log.clone(),
            start: Ok(behind(4)),
            statuses: Mutex::new(vec![Ok(behind(4))]),
            enabled: Ok(status(UpdateRunState::Idle, None, "")),
        });
        disable_one(studio, report.clone()).await;
        assert_eq!(
            last(&seen.lock().unwrap(), "studio"),
            RowState::NotSetUp { can_enable: true },
            "off, and Turn on is offered again"
        );
        assert!(log.lock().unwrap().contains(&"disable:studio".to_string()));
        // Predates the route: still on there, so the row says what it is.
        let old = Fake::machine("old", &log, Ok(behind(2)), vec![Ok(behind(2))]);
        disable_one(old, report).await;
        assert_eq!(last(&seen.lock().unwrap(), "old"), RowState::Behind(2));
        // Offered only where updates are on and nothing is in flight.
        assert!(can_turn_off(&RowState::Behind(2)));
        assert!(!can_turn_off(&RowState::NotSetUp { can_enable: true }));
        assert!(!can_turn_off(&RowState::Updating(String::new())));
        assert!(!can_turn_off(&RowState::Checking));
    }

    #[tokio::test]
    async fn turn_on_enables_then_checks_and_an_old_backend_keeps_the_hint() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let studio = Arc::new(Fake {
            key: "studio".into(),
            log: log.clone(),
            start: Ok(behind(4)),
            statuses: Mutex::new(vec![Ok(behind(4))]),
            enabled: Ok(status(UpdateRunState::Idle, None, "")),
        });
        let (report, seen) = recorder();
        enable_one(studio, report.clone()).await;
        assert_eq!(*log.lock().unwrap(), vec!["enable:studio", "check:studio"]);
        assert_eq!(last(&seen.lock().unwrap(), "studio"), RowState::Behind(4));

        // A backend from before the enable route.
        let old = Fake::machine("old", &log, Ok(behind(1)), vec![Ok(behind(1))]);
        enable_one(old, report).await;
        assert_eq!(
            last(&seen.lock().unwrap(), "old"),
            RowState::NotSetUp { can_enable: false }
        );
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
        /// What "Turn on" answers.
        enabled: Result<UpdateStatus, UpdateCallError>,
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
                enabled: Err(UpdateCallError::TooOld),
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

        fn enable(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>> {
            self.note("enable");
            let answer = self.enabled.clone();
            Box::pin(async move { answer })
        }

        fn disable(&self) -> BoxFuture<'_, Result<UpdateStatus, UpdateCallError>> {
            self.note("disable");
            // The same shape of answer as enable: off, or too old.
            let answer = self.enabled.clone().map(|mut status| {
                status.method = UpdateMethod::None;
                status.can_enable = true;
                status
            });
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
        assert_eq!(last(&seen, "off"), RowState::NotSetUp { can_enable: false });
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
        assert_eq!(last(&seen, "b"), RowState::NotSetUp { can_enable: false });
        assert!(seen.contains(&("a".into(), RowState::Checking)));
    }
}
