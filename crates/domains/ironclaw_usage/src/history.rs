//! Token history for the Usage page: what each coding CLI spent, by day,
//! provider and model, priced at list rates. Ported from t3code (MIT,
//! `apps/server/src/usage`): the CLIs' own transcripts are the record, read
//! line by line, and only lines that can carry usage are parsed.
//!
//! - Claude CLI writes one assistant record per content block, each repeating
//!   the message's full `usage`; records are de-duplicated by message and
//!   request id across files, keeping the first.
//! - Codex writes `token_count` events without a model; the model comes from
//!   the latest `turn_context`. Cumulative counters drop repeated events, and
//!   a forked rollout's copied parent history (written in one burst right
//!   after `session_meta`) is skipped.
//!
//! Files are memoised by size and mtime, and a grown file resumes from where
//! the last scan stopped when the bytes before that point are unchanged.
//! Prices come from models.dev's public catalog, cached on disk for a day.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{Local, TimeZone};
use ironclaw_network::NetworkHttpEgress;
use ironclaw_product_contracts::usage::{
    UsageBucket, UsageCostSource, UsageHistory, UsageHistorySource, UsagePricing,
    UsagePricingStatus, UsageSourceStatus, UsageTokenTotals,
};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::http;

const PRICES_HOST: &str = "models.dev";
const PRICES_URL: &str = "https://models.dev/api.json";
const PRICES_TTL: Duration = Duration::from_secs(24 * 3600);
const PRICES_BODY_LIMIT: u64 = 16 * 1024 * 1024;
/// A forked rollout's copied history lands in one burst (observed gaps under
/// 40 ms); the child's own first event only follows a real model turn.
const FORK_COPY_MAX_GAP_MS: i64 = 1000;
/// Bytes before a resume point that must still match for a grown file to
/// resume instead of being read again.
const GUARD: u64 = 64;
pub(crate) const MAX_DAYS: u32 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Claude,
    Codex,
}

/// One CLI login's transcript roots.
pub(crate) struct HistorySource {
    pub provider: String,
    pub label: String,
    pub format: Format,
    pub roots: Vec<PathBuf>,
}

impl HistorySource {
    /// Both Claude logins and Codex under `home`, matching the limit cards.
    pub(crate) fn defaults(home: &Path) -> Vec<Self> {
        vec![
            Self {
                provider: "claude_cli".into(),
                label: "Claude CLI".into(),
                format: Format::Claude,
                roots: vec![home.join(".claude").join("projects")],
            },
            Self {
                provider: "claude_cli_2".into(),
                label: "Claude CLI 2".into(),
                format: Format::Claude,
                roots: vec![home.join(".claude-account2").join("projects")],
            },
            Self {
                provider: "codex".into(),
                label: "Codex".into(),
                format: Format::Codex,
                roots: vec![
                    home.join(".codex").join("sessions"),
                    home.join(".codex").join("archived_sessions"),
                ],
            },
        ]
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Totals {
    uncached: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
    /// A subset of `output`, never added on top.
    reasoning: u64,
}

impl Totals {
    fn sum(&self) -> u64 {
        self.uncached + self.cached + self.cache_write + self.output
    }

    fn add(&mut self, other: &Totals) {
        self.uncached += other.uncached;
        self.cached += other.cached;
        self.cache_write += other.cache_write;
        self.output += other.output;
        self.reasoning += other.reasoning;
    }
}

#[derive(Debug, Clone)]
struct Record {
    at_ms: i64,
    model: Arc<str>,
    session: Arc<str>,
    totals: Totals,
    reported_cost: Option<f64>,
    dedupe: Option<Box<str>>,
}

/// What earlier lines of a Codex rollout established.
#[derive(Debug, Clone, Default)]
struct CodexState {
    model: Arc<str>,
    session: Arc<str>,
    saw_meta: bool,
    last_signature: Option<[u64; 5]>,
    last_cumulative: Option<[u64; 5]>,
    suppressing_fork: bool,
    fork_anchor_ms: i64,
}

#[derive(Debug, Clone, Default)]
struct FileMemo {
    len: u64,
    mtime: Option<SystemTime>,
    records: Vec<Record>,
    /// Where the last complete line ended, a hash of the bytes before it,
    /// and the Codex state at that point.
    resume: u64,
    guard: u64,
    codex: CodexState,
}

struct RateTable {
    rates: HashMap<String, Rate>,
    status: UsagePricingStatus,
    fetched_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rate {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

pub(crate) struct HistoryScanner {
    sources: Vec<HistorySource>,
    files: Arc<std::sync::Mutex<HashMap<PathBuf, FileMemo>>>,
    prices: Mutex<Option<RateTable>>,
    price_file: Option<PathBuf>,
}

impl HistoryScanner {
    pub(crate) fn new(sources: Vec<HistorySource>, price_file: Option<PathBuf>) -> Self {
        Self {
            sources,
            files: Arc::default(),
            prices: Mutex::new(None),
            price_file,
        }
    }

    /// The last `days` days (today included) in the host's time zone.
    pub(crate) async fn history(&self, egress: &dyn NetworkHttpEgress, days: u32) -> UsageHistory {
        let started = std::time::Instant::now();
        let days = days.clamp(1, MAX_DAYS);
        let today = Local::now().date_naive();
        let since = today - chrono::Days::new(u64::from(days - 1));
        let since_ms = Local
            .from_local_datetime(&since.and_hms_opt(0, 0, 0).unwrap_or_default())
            .earliest()
            .map_or(0, |t| t.timestamp_millis());

        let jobs: Vec<(String, String, Format, Vec<PathBuf>)> = self
            .sources
            .iter()
            .map(|s| {
                (
                    s.provider.clone(),
                    s.label.clone(),
                    s.format,
                    s.roots.clone(),
                )
            })
            .collect();
        let files = Arc::clone(&self.files);
        // silent-ok: a panicked scan reports every source as failed below.
        let scanned = tokio::task::spawn_blocking(move || {
            jobs.into_iter()
                .map(|(provider, label, format, roots)| {
                    let scan = scan_source(&files, format, &roots, since_ms);
                    (provider, label, scan)
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();

        let prices = self.prices(egress).await;
        let mut buckets: HashMap<(String, String, String), Bucket> = HashMap::new();
        let mut sources = Vec::new();
        for (provider, label, scan) in scanned {
            let mut sessions = HashSet::new();
            for record in &scan.records {
                let day = Local
                    .timestamp_millis_opt(record.at_ms)
                    .single()
                    .map(|t| t.date_naive());
                let Some(day) = day.filter(|d| *d >= since && *d <= today) else {
                    continue;
                };
                sessions.insert(record.session.clone());
                let key = (
                    day.format("%Y-%m-%d").to_string(),
                    provider.clone(),
                    record.model.to_string(),
                );
                buckets
                    .entry(key)
                    .or_default()
                    .add(record, prices.as_ref().map(|p| &p.rates));
            }
            sources.push(UsageHistorySource {
                provider,
                label,
                status: scan.status,
                scanned_files: scan.scanned_files,
                distinct_sessions: u32::try_from(sessions.len()).unwrap_or(u32::MAX),
                message: scan.message,
            });
        }
        let mut buckets: Vec<UsageBucket> = buckets
            .into_iter()
            .map(|((day, provider, model), b)| b.into_wire(day, provider, model))
            .collect();
        buckets
            .sort_by(|a, b| (&a.day, &a.provider, &a.model).cmp(&(&b.day, &b.provider, &b.model)));

        UsageHistory {
            since_day: since.format("%Y-%m-%d").to_string(),
            until_day: today.format("%Y-%m-%d").to_string(),
            read_at: chrono::Utc::now().timestamp_millis(),
            scan_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            buckets,
            sources,
            pricing: match prices.as_ref() {
                Some(p) => UsagePricing {
                    status: p.status,
                    fetched_at: p.fetched_at,
                    known_models: u32::try_from(p.rates.len()).unwrap_or(u32::MAX),
                },
                None => UsagePricing {
                    status: UsagePricingStatus::Unavailable,
                    fetched_at: None,
                    known_models: 0,
                },
            },
        }
    }

    /// The rate table: in memory for a day, else the disk copy if still
    /// fresh, else fetched; a stale disk copy beats nothing.
    async fn prices(
        &self,
        egress: &dyn NetworkHttpEgress,
    ) -> tokio::sync::MutexGuard<'_, Option<RateTable>> {
        let mut held = self.prices.lock().await;
        let now = chrono::Utc::now().timestamp_millis();
        let fresh = |at: Option<i64>| {
            at.is_some_and(|at| now - at < i64::try_from(PRICES_TTL.as_millis()).unwrap_or(0))
        };
        if held.as_ref().is_some_and(|t| fresh(t.fetched_at)) {
            return held;
        }
        let disk = match &self.price_file {
            Some(file) => read_price_file(file).await,
            None => None,
        };
        if let Some((body, at)) = &disk
            && fresh(Some(*at))
        {
            *held = Some(RateTable {
                rates: parse_rates(body),
                status: UsagePricingStatus::Cached,
                fetched_at: Some(*at),
            });
            return held;
        }
        match fetch_prices(egress).await {
            Some(body) => {
                if let Some(file) = &self.price_file {
                    if let Some(dir) = file.parent() {
                        let _ = tokio::fs::create_dir_all(dir).await;
                    }
                    if let Err(error) = tokio::fs::write(file, &body).await {
                        tracing::debug!(%error, "could not cache the price table");
                    }
                }
                *held = Some(RateTable {
                    rates: parse_rates(&body),
                    status: UsagePricingStatus::Fresh,
                    fetched_at: Some(now),
                });
            }
            None => {
                if let Some((body, at)) = disk {
                    *held = Some(RateTable {
                        rates: parse_rates(&body),
                        status: UsagePricingStatus::Cached,
                        fetched_at: Some(at),
                    });
                }
            }
        }
        held
    }
}

async fn read_price_file(file: &Path) -> Option<(Vec<u8>, i64)> {
    // silent-ok: a missing or unreadable cache just means fetching again.
    let meta = tokio::fs::metadata(file).await.ok()?;
    let at = meta
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis();
    let body = tokio::fs::read(file).await.ok()?;
    Some((body, i64::try_from(at).ok()?))
}

async fn fetch_prices(egress: &dyn NetworkHttpEgress) -> Option<Vec<u8>> {
    let response = http::get_with_limit(
        egress,
        PRICES_HOST,
        PRICES_URL.to_string(),
        Vec::new(),
        PRICES_BODY_LIMIT,
    )
    .await
    .ok()?;
    if response.status != 200 {
        tracing::debug!(status = response.status, "price table request failed");
        return None;
    }
    Some(response.body)
}

/// models.dev: `{provider: {models: {id: {cost: {input, output, cache_read,
/// cache_write}}}}}`, dollars per million tokens. Each model is keyed by its
/// lowercased id; when providers disagree on a bare id the first listed
/// wins, and a model without both input and output prices is left unpriced
/// (half a price would under-report).
fn parse_rates(body: &[u8]) -> HashMap<String, Rate> {
    #[derive(Deserialize)]
    struct Provider {
        #[serde(default)]
        models: HashMap<String, Model>,
    }
    #[derive(Deserialize)]
    struct Model {
        cost: Option<Cost>,
    }
    #[derive(Deserialize)]
    struct Cost {
        input: Option<f64>,
        output: Option<f64>,
        cache_read: Option<f64>,
        cache_write: Option<f64>,
    }
    let mut rates = HashMap::new();
    let Ok(catalog) = serde_json::from_slice::<HashMap<String, Provider>>(body) else {
        tracing::debug!("price table did not parse");
        return rates;
    };
    // Vendors first, so a bare id takes its maker's price over a reseller's.
    let mut providers: Vec<(String, Provider)> = catalog.into_iter().collect();
    providers.sort_by_key(|(id, _)| (!matches!(id.as_str(), "anthropic" | "openai"), id.clone()));
    for (_, provider) in providers {
        for (id, model) in provider.models {
            let Some(cost) = model.cost else { continue };
            let (Some(input), Some(output)) = (cost.input, cost.output) else {
                continue;
            };
            let per = 1e-6;
            let rate = Rate {
                input: input * per,
                output: output * per,
                cache_read: cost.cache_read.unwrap_or(input) * per,
                cache_write: cost.cache_write.unwrap_or(input) * per,
            };
            let key = id.trim().to_ascii_lowercase();
            let bare = key.rsplit('/').next().unwrap_or(&key).to_string();
            rates.entry(key).or_insert(rate);
            rates.entry(bare).or_insert(rate);
        }
    }
    rates
}

/// `None` for ids that name no single model: `<synthetic>` placeholders and
/// bare family names. A bracketed tier suffix (`[1m]`) prices at the base.
fn lookup<'a>(rates: &'a HashMap<String, Rate>, model: &str) -> Option<&'a Rate> {
    let key = model.trim().to_ascii_lowercase();
    let key = key.split('[').next().unwrap_or(&key);
    let bare = key.rsplit('/').next().unwrap_or(key);
    if bare.is_empty()
        || matches!(
            bare,
            "<synthetic>" | "synthetic" | "opus" | "sonnet" | "haiku" | "fable"
        )
    {
        return None;
    }
    rates.get(key).or_else(|| rates.get(bare))
}

#[derive(Default)]
struct Bucket {
    totals: Totals,
    cost: f64,
    savings: f64,
    records: u32,
    unpriced: u32,
    reported: u32,
    sessions: HashSet<Arc<str>>,
}

impl Bucket {
    fn add(&mut self, record: &Record, rates: Option<&HashMap<String, Rate>>) {
        self.totals.add(&record.totals);
        self.records += 1;
        self.sessions.insert(record.session.clone());
        let rate = rates.and_then(|r| lookup(r, &record.model));
        let t = &record.totals;
        match (record.reported_cost, rate) {
            (Some(cost), _) => {
                self.cost += cost;
                self.reported += 1;
            }
            (None, Some(r)) => {
                self.cost += t.uncached as f64 * r.input
                    + t.cached as f64 * r.cache_read
                    + t.cache_write as f64 * r.cache_write
                    + t.output as f64 * r.output;
            }
            (None, None) => self.unpriced += 1,
        }
        if let Some(r) = rate {
            self.savings += t.cached as f64 * (r.input - r.cache_read);
        }
    }

    fn into_wire(self, day: String, provider: String, model: String) -> UsageBucket {
        UsageBucket {
            day,
            provider,
            model,
            totals: UsageTokenTotals {
                uncached_input_tokens: self.totals.uncached,
                cached_input_tokens: self.totals.cached,
                cache_creation_tokens: self.totals.cache_write,
                output_tokens: self.totals.output,
                reasoning_tokens: self.totals.reasoning,
            },
            cost_usd: self.cost,
            cache_savings_usd: self.savings,
            cost_source: if self.unpriced == self.records {
                UsageCostSource::Unpriced
            } else if self.reported > 0 {
                UsageCostSource::ProviderReported
            } else {
                UsageCostSource::ModelPriced
            },
            records: self.records,
            unpriced_records: self.unpriced,
            sessions: u32::try_from(self.sessions.len()).unwrap_or(u32::MAX),
        }
    }
}

#[derive(Default)]
struct SourceScan {
    records: Vec<Record>,
    status: UsageSourceStatus,
    scanned_files: u32,
    message: Option<String>,
}

fn scan_source(
    memo: &std::sync::Mutex<HashMap<PathBuf, FileMemo>>,
    format: Format,
    roots: &[PathBuf],
    since_ms: i64,
) -> SourceScan {
    let mut paths = Vec::new();
    let mut failed = 0u32;
    let mut any_root = false;
    for root in roots {
        if root.is_dir() {
            any_root = true;
            walk(root, &mut paths, &mut failed);
        }
    }
    if !any_root {
        return SourceScan {
            status: UsageSourceStatus::Missing,
            message: Some("No transcripts on this machine.".into()),
            ..SourceScan::default()
        };
    }
    let since =
        SystemTime::UNIX_EPOCH + Duration::from_millis(u64::try_from(since_ms).unwrap_or(0));
    let mut records = Vec::new();
    let mut scanned = 0u32;
    let mut seen_keys: HashSet<Box<str>> = HashSet::new();
    for path in paths {
        let Ok(meta) = std::fs::metadata(&path) else {
            failed += 1;
            continue;
        };
        let mtime = meta.modified().ok();
        // A file last written before the window holds nothing in it.
        if mtime.is_some_and(|m| m < since) {
            continue;
        }
        scanned += 1;
        let previous = memo
            .lock()
            .ok()
            .and_then(|m| m.get(&path).cloned())
            .unwrap_or_default();
        let current =
            if previous.len == meta.len() && previous.mtime == mtime && previous.mtime.is_some() {
                previous
            } else {
                match read_file(&path, format, previous, meta.len(), mtime) {
                    Some(memo) => memo,
                    None => {
                        failed += 1;
                        continue;
                    }
                }
            };
        for record in current.records.iter().filter(|r| r.at_ms >= since_ms) {
            if let Some(key) = &record.dedupe
                && !seen_keys.insert(key.clone())
            {
                continue;
            }
            records.push(record.clone());
        }
        if let Ok(mut m) = memo.lock() {
            m.insert(path, current);
        }
    }
    SourceScan {
        records,
        status: if failed > 0 {
            UsageSourceStatus::Partial
        } else {
            UsageSourceStatus::Ok
        },
        scanned_files: scanned,
        message: (failed > 0).then(|| format!("{failed} transcript files could not be read.")),
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, failed: &mut u32) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        *failed += 1;
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&path, out, failed),
            Ok(_) if path.extension().is_some_and(|e| e == "jsonl") => out.push(path),
            _ => {}
        }
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Read `path` from its last resume point when the file only grew and the
/// bytes before that point are unchanged, else from the start. `None` when
/// the file could not be read (not memoised, so the next scan retries).
fn read_file(
    path: &Path,
    format: Format,
    previous: FileMemo,
    len: u64,
    mtime: Option<SystemTime>,
) -> Option<FileMemo> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut memo = FileMemo {
        len,
        mtime,
        ..FileMemo::default()
    };
    if previous.resume > 0 && len >= previous.resume {
        let guard_len = previous.resume.min(GUARD);
        let mut window = vec![0u8; usize::try_from(guard_len).ok()?];
        file.seek(SeekFrom::Start(previous.resume - guard_len))
            .ok()?;
        file.read_exact(&mut window).ok()?;
        if fnv1a(&window) == previous.guard {
            memo.records = previous.records;
            memo.codex = previous.codex;
            memo.resume = previous.resume;
        }
    }
    file.seek(SeekFrom::Start(memo.resume)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let mut start = 0usize;
    while let Some(nl) = bytes[start..].iter().position(|b| *b == b'\n') {
        let line = &bytes[start..start + nl];
        match format {
            Format::Claude => memo.records.extend(parse_claude(line)),
            Format::Codex => memo.records.extend(parse_codex(line, &mut memo.codex)),
        }
        start += nl + 1;
    }
    // The unterminated tail is left for the next scan, once its writer ends
    // the line.
    memo.resume += start as u64;
    let guard_len = usize::try_from(memo.resume.min(GUARD)).ok()?;
    memo.guard = if start >= guard_len {
        fnv1a(&bytes[start - guard_len..start])
    } else {
        // Fewer new bytes than the guard: hash across the old resume point.
        let mut window = vec![0u8; guard_len];
        file.seek(SeekFrom::Start(memo.resume - guard_len as u64))
            .ok()?;
        file.read_exact(&mut window).ok()?;
        fnv1a(&window)
    };
    Some(memo)
}

fn contains(line: &[u8], marker: &[u8]) -> bool {
    line.windows(marker.len()).any(|w| w == marker)
}

fn parse_ms(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn parse_claude(line: &[u8]) -> Option<Record> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Line {
        #[serde(rename = "type")]
        kind: Option<String>,
        timestamp: Option<String>,
        request_id: Option<String>,
        session_id: Option<String>,
        #[serde(rename = "costUSD")]
        cost_usd: Option<f64>,
        message: Option<Message>,
    }
    #[derive(Deserialize)]
    struct Message {
        id: Option<String>,
        model: Option<String>,
        usage: Option<Usage>,
    }
    #[derive(Deserialize)]
    struct Usage {
        input_tokens: Option<u64>,
        cache_read_input_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    }
    if !contains(line, b"\"usage\"") {
        return None;
    }
    let parsed: Line = serde_json::from_slice(line).ok()?;
    if parsed.kind.as_deref() != Some("assistant") {
        return None;
    }
    let message = parsed.message?;
    let usage = message.usage?;
    let model = message
        .model
        .filter(|m| !m.is_empty() && m != "<synthetic>")?;
    let at_ms = parse_ms(parsed.timestamp.as_deref()?)?;
    let dedupe = (message.id.is_some() || parsed.request_id.is_some()).then(|| {
        format!(
            "{}:{}",
            message.id.unwrap_or_default(),
            parsed.request_id.unwrap_or_default()
        )
        .into_boxed_str()
    });
    let totals = Totals {
        uncached: usage.input_tokens.unwrap_or(0),
        cached: usage.cache_read_input_tokens.unwrap_or(0),
        cache_write: usage.cache_creation_input_tokens.unwrap_or(0),
        output: usage.output_tokens.unwrap_or(0),
        // Anthropic folds thinking into output.
        reasoning: 0,
    };
    (totals.sum() > 0).then(|| Record {
        at_ms,
        model: model.into(),
        session: parsed.session_id.unwrap_or_default().into(),
        totals,
        reported_cost: parsed.cost_usd.filter(|c| c.is_finite()),
        dedupe,
    })
}

fn parse_codex(line: &[u8], state: &mut CodexState) -> Option<Record> {
    #[derive(Deserialize)]
    struct Line {
        #[serde(rename = "type")]
        kind: Option<String>,
        timestamp: Option<String>,
        payload: Option<serde_json::Value>,
    }
    #[derive(Deserialize, Clone, Copy)]
    struct Usage {
        input_tokens: u64,
        #[serde(default)]
        cached_input_tokens: u64,
        #[serde(default)]
        cache_write_input_tokens: u64,
        output_tokens: u64,
        #[serde(default)]
        reasoning_output_tokens: u64,
    }
    impl Usage {
        fn fields(&self) -> [u64; 5] {
            [
                self.input_tokens,
                self.cached_input_tokens,
                self.cache_write_input_tokens,
                self.output_tokens,
                self.reasoning_output_tokens,
            ]
        }
    }
    let relevant = [
        b"\"token_count\"".as_slice(),
        b"\"turn_context\"",
        b"\"session_meta\"",
    ];
    if !relevant.iter().any(|m| contains(line, m)) {
        return None;
    }
    let parsed: Line = serde_json::from_slice(line).ok()?;
    let payload = parsed.payload?;
    let at_ms = parsed.timestamp.as_deref().and_then(parse_ms);
    match parsed.kind.as_deref() {
        Some("session_meta") => {
            if state.saw_meta {
                return None;
            }
            state.saw_meta = true;
            if let Some(id) = payload
                .get("id")
                .or_else(|| payload.get("session_id"))
                .and_then(|v| v.as_str())
            {
                state.session = id.into();
            }
            let forked = payload.get("forked_from_id").is_some_and(|v| v.is_string())
                || payload
                    .pointer("/source/subagent/thread_spawn/parent_thread_id")
                    .is_some_and(|v| v.is_string());
            if forked && let Some(at) = at_ms {
                state.suppressing_fork = true;
                state.fork_anchor_ms = at;
            }
            return None;
        }
        Some("turn_context") => {
            if let Some(model) = payload.get("model").and_then(|v| v.as_str()) {
                state.model = model.into();
            }
            return None;
        }
        _ => {}
    }
    if payload.get("type").and_then(|v| v.as_str()) != Some("token_count") {
        return None;
    }
    let at_ms = at_ms?;
    if state.model.is_empty() {
        return None;
    }
    let info = payload.get("info")?;
    let last: Usage = serde_json::from_value(info.get("last_token_usage")?.clone()).ok()?;
    let cumulative: Option<Usage> = info
        .get("total_token_usage")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let previous = state.last_cumulative;
    if let Some(c) = cumulative {
        state.last_cumulative = Some(c.fields());
    }
    if state.suppressing_fork {
        if at_ms - state.fork_anchor_ms < FORK_COPY_MAX_GAP_MS {
            state.fork_anchor_ms = at_ms;
            state.last_signature = Some(last.fields());
            return None;
        }
        state.suppressing_fork = false;
        state.last_signature = None;
    }
    if last.input_tokens + last.output_tokens == 0 {
        state.last_signature = None;
        return None;
    }
    // A repeated event: the running total did not move.
    if let (Some(c), Some(p)) = (cumulative, previous)
        && c.fields() == p
    {
        return None;
    }
    if cumulative.is_none() && state.last_signature == Some(last.fields()) {
        return None;
    }
    state.last_signature = Some(last.fields());
    let cached = last.cached_input_tokens.min(last.input_tokens);
    let cache_write = last
        .cache_write_input_tokens
        .min(last.input_tokens - cached);
    Some(Record {
        at_ms,
        model: state.model.clone(),
        session: state.session.clone(),
        totals: Totals {
            uncached: last.input_tokens - cached - cache_write,
            cached,
            cache_write,
            output: last.output_tokens,
            reasoning: last.reasoning_output_tokens.min(last.output_tokens),
        },
        reported_cost: None,
        dedupe: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_records_repeat_per_block_and_dedupe_by_message() {
        let line = br#"{"type":"assistant","timestamp":"2026-10-24T10:00:00Z","requestId":"r1","sessionId":"s1","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":5,"output_tokens":20}}}"#;
        let record = parse_claude(line).unwrap();
        assert_eq!(record.totals.sum(), 135);
        assert_eq!(record.dedupe.as_deref(), Some("m1:r1"));
        let synthetic = br#"{"type":"assistant","timestamp":"2026-10-24T10:00:00Z","message":{"model":"<synthetic>","usage":{"output_tokens":1}}}"#;
        assert!(parse_claude(synthetic).is_none());
    }

    #[test]
    fn codex_takes_the_model_from_turn_context_and_drops_repeats() {
        let mut state = CodexState::default();
        let lines: [&[u8]; 4] = [
            br#"{"type":"session_meta","timestamp":"2026-10-24T10:00:00Z","payload":{"id":"s1"}}"#,
            br#"{"type":"turn_context","timestamp":"2026-10-24T10:00:01Z","payload":{"model":"gpt-6.1-sol"}}"#,
            br#"{"type":"event_msg","timestamp":"2026-10-24T10:00:05Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":10,"reasoning_output_tokens":4},"total_token_usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":10,"reasoning_output_tokens":4}}}}"#,
            br#"{"type":"event_msg","timestamp":"2026-10-24T10:00:06Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":10,"reasoning_output_tokens":4},"total_token_usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":10,"reasoning_output_tokens":4}}}}"#,
        ];
        let records: Vec<Record> = lines
            .iter()
            .filter_map(|l| parse_codex(l, &mut state))
            .collect();
        assert_eq!(records.len(), 1, "the repeated event is dropped");
        let r = &records[0];
        assert_eq!(&*r.model, "gpt-6.1-sol");
        assert_eq!(&*r.session, "s1");
        assert_eq!(
            (r.totals.uncached, r.totals.cached, r.totals.output),
            (40, 60, 10)
        );
    }

    #[test]
    fn a_forked_rollout_skips_its_copied_history() {
        let mut state = CodexState::default();
        let lines: [&[u8]; 4] = [
            br#"{"type":"session_meta","timestamp":"2026-10-24T10:00:00.000Z","payload":{"id":"child","forked_from_id":"parent"}}"#,
            br#"{"type":"turn_context","timestamp":"2026-10-24T10:00:00.010Z","payload":{"model":"gpt-6.1-sol"}}"#,
            br#"{"type":"event_msg","timestamp":"2026-10-24T10:00:00.020Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":500,"output_tokens":50}}}}"#,
            br#"{"type":"event_msg","timestamp":"2026-10-24T10:00:09.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":30,"output_tokens":3}}}}"#,
        ];
        let records: Vec<Record> = lines
            .iter()
            .filter_map(|l| parse_codex(l, &mut state))
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].totals.sum(), 33);
    }

    #[test]
    fn prices_cover_cache_rates_and_skip_family_names() {
        let body = br#"{"anthropic":{"models":{"claude-opus-5-5":{"cost":{"input":5,"output":25,"cache_read":0.5,"cache_write":6.25}}}},"reseller":{"models":{"anthropic/claude-opus-5-5":{"cost":{"input":9,"output":9}}}}}"#;
        let rates = parse_rates(body);
        let rate = lookup(&rates, "claude-opus-5-5[1m]").unwrap();
        assert!(
            (rate.input - 5e-6).abs() < 1e-12,
            "the maker's price wins the bare id"
        );
        assert!(lookup(&rates, "opus").is_none());
    }

    #[test]
    fn a_grown_file_resumes_and_a_rewritten_one_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let line = |id: &str| {
            format!(
                r#"{{"type":"assistant","timestamp":"2026-10-24T10:00:00Z","sessionId":"s","message":{{"id":"{id}","model":"m","usage":{{"input_tokens":1,"output_tokens":1}}}}}}"#
            ) + "\n"
        };
        std::fs::write(&path, line("a")).unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let first = read_file(&path, Format::Claude, FileMemo::default(), len, None).unwrap();
        assert_eq!(first.records.len(), 1);
        std::fs::write(&path, line("a") + &line("b")).unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let grown = read_file(&path, Format::Claude, first.clone(), len, None).unwrap();
        assert_eq!(grown.records.len(), 2);
        std::fs::write(&path, line("c") + &line("d") + &line("e")).unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let rewritten = read_file(&path, Format::Claude, first, len, None).unwrap();
        let ids: Vec<_> = rewritten.records.iter().map(|r| r.dedupe.clone()).collect();
        assert_eq!(
            ids.len(),
            3,
            "a changed prefix reads from the start: {ids:?}"
        );
    }
}
