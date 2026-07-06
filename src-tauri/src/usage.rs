// Usage analytics — aggregation semantics ported from ccusage (github.com/ccusage/ccusage),
// scoped to the two agents ccbud fronts: Claude Code and Codex.
//
// Per active work dir, two session trees contribute:
//
//   Claude Code `projects/**/*.jsonl` (recursive, any depth — sessions, nested session dirs,
//   subagent transcripts all included by construction):
//     - every line whose `message.usage` carries numeric input/output tokens counts — no
//       `type=="assistant"` gate (ccusage parity);
//     - a line without a parseable RFC3339 `timestamp` is DROPPED (never guessed);
//     - cache-creation prefers the nested `cache_creation.ephemeral_{5m,1h}_input_tokens`
//       breakdown over the flat `cache_creation_input_tokens`;
//     - `<synthetic>` models keep their tokens but get no model attribution; `usage.speed=="fast"`
//       appends a `-fast` suffix to the model;
//     - global de-dup by (message.id, requestId) — entries without a message.id are never
//       de-duped; a sidechain replay that reuses the parent's message.id under a NEW requestId
//       collapses onto the parent (non-sidechain wins, then higher token total).
//
//   Codex `sessions/**/*.jsonl` + `archived_sessions/**/*.jsonl` (an archived copy of the same
//   relative path is skipped — the active sessions/ copy wins):
//     - `token_count` events: prefer `info.last_token_usage` (the turn delta); fall back to
//       diffing consecutive `info.total_token_usage` snapshots; the cumulative baseline always
//       advances so either form stays correct;
//     - `thread_spawn` subagent files replay the parent's history as a leading burst of
//       token_count lines sharing one timestamp-second — those are skipped (baseline still
//       advances) so parent turns aren't counted twice;
//     - identical (timestamp, model, tokens) events across files (resumed/forked sessions)
//       de-dup globally;
//     - model comes from the event payload/info, else the last `turn_context`, else "gpt-5";
//       `input_tokens` is INCLUSIVE of `cached_input_tokens` — the cached part is split out into
//       cacheRead and the remainder becomes input.
//
// Day bucketing is local-timezone (chrono::Local), matching ccusage's system-timezone default.

#![allow(dead_code)]

use chrono::{Datelike, Local, TimeZone, Timelike};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

const DAY_MS: i64 = 86_400_000;
const HEATMAP_WEEKS: i64 = 26;

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}
fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else if p == "~" {
        home()
    } else {
        PathBuf::from(p)
    }
}

/// Active work dirs (honors the directory switcher). A selector that matches no configured dir —
/// the synthetic recycle-bin / imported-bundle views ("__trash__", "__imported__"), or a stale
/// value from an older config — falls back to ALL dirs: a filter must never zero the stats.
fn active_roots(config: &Value, active: &str) -> Vec<PathBuf> {
    let mut all = vec![];
    let mut selected = vec![];
    if let Some(arr) = config.get("historyDirs").and_then(|v| v.as_array()) {
        for d in arr {
            if let Some(s) = d.as_str() {
                all.push(expand_tilde(s));
                if active == s {
                    selected.push(expand_tilde(s));
                }
            }
        }
    }
    if active != "all" && !selected.is_empty() {
        selected
    } else {
        all
    }
}

fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}
fn key_of(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(d) => format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day()),
        None => "1970-01-01".to_string(),
    }
}
fn start_of_day(ms: i64) -> i64 {
    match Local.timestamp_millis_opt(ms).single() {
        Some(d) => {
            let day = d.date_naive().and_hms_opt(0, 0, 0).unwrap();
            Local.from_local_datetime(&day).single().map(|x| x.timestamp_millis()).unwrap_or(ms)
        }
        None => ms,
    }
}
fn ms_of_key(k: &str) -> i64 {
    let parts: Vec<i64> = k.split('-').filter_map(|x| x.parse().ok()).collect();
    if parts.len() != 3 {
        return 0;
    }
    let nd = chrono::NaiveDate::from_ymd_opt(parts[0] as i32, parts[1] as u32, parts[2] as u32);
    match nd.and_then(|d| d.and_hms_opt(0, 0, 0)) {
        Some(dt) => Local.from_local_datetime(&dt).single().map(|x| x.timestamp_millis()).unwrap_or(0),
        None => 0,
    }
}
fn hour_of(ms: i64) -> u32 {
    Local.timestamp_millis_opt(ms).single().map(|d| d.hour()).unwrap_or(0)
}

#[derive(Default, Clone)]
struct Day {
    tokens: i64,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
    requests: i64,
    models: HashMap<String, i64>,
    providers: HashMap<String, i64>,
    hours: HashMap<u32, i64>,
}

/// One counted usage event, whichever tree it came from.
struct UsageRec {
    ts: i64,
    model: Option<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
}

impl UsageRec {
    fn total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_creation
    }
}

fn bump(days: &mut HashMap<String, Day>, rec: &UsageRec) {
    let day = days.entry(key_of(rec.ts)).or_default();
    day.requests += 1;
    day.tokens += rec.total();
    day.input += rec.input;
    day.output += rec.output;
    day.cache_read += rec.cache_read;
    day.cache_creation += rec.cache_creation;
    if let Some(m) = &rec.model {
        *day.models.entry(m.clone()).or_insert(0) += rec.total();
    }
    *day.hours.entry(hour_of(rec.ts)).or_insert(0) += rec.total();
}

/// Recursively collect `*.jsonl` under `dir`, any depth (ccusage walks the whole tree — nested
/// session dirs and subagent transcripts are picked up by construction). Depth-capped as a
/// symlink-loop guard.
fn collect_jsonl(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > 8 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for ent in entries.flatten() {
        let p = ent.path();
        if p.is_dir() {
            collect_jsonl(&p, depth + 1, out);
        } else if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(p);
        }
    }
}

/// Byte-based lossy line reader. History files can embed invalid UTF-8 inside tool output —
/// a strict `BufRead::lines` errors there and would silently discard the REST of the file
/// (ccusage reads raw bytes for the same reason).
struct LossyLines {
    reader: std::io::BufReader<std::fs::File>,
    buf: Vec<u8>,
}

impl LossyLines {
    fn open(file: &Path) -> Option<Self> {
        std::fs::File::open(file)
            .ok()
            .map(|f| Self { reader: std::io::BufReader::new(f), buf: Vec::with_capacity(64 * 1024) })
    }
    fn next_line(&mut self) -> Option<String> {
        self.buf.clear();
        match self.reader.read_until(b'\n', &mut self.buf) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(String::from_utf8_lossy(&self.buf).into_owned()),
        }
    }
}

// ---------------------------------------------------------------------------
// Claude Code (projects/ tree)
// ---------------------------------------------------------------------------

struct ClaudeRec {
    id: Option<String>,
    request_id: Option<String>,
    sidechain: bool,
    rec: UsageRec,
}

/// Parse one history line into a usage entry. Requires numeric `message.usage.input_tokens` /
/// `output_tokens` and a parseable `timestamp`; everything else is optional.
fn parse_claude_line(line: &str) -> Option<ClaudeRec> {
    // cheap prefilter before JSON parse (ccusage scans for the same marker)
    if !line.contains("\"usage\"") {
        return None;
    }
    let r: Value = serde_json::from_str(line).ok()?;
    let m = r.get("message")?;
    let u = m.get("usage")?;
    let input = u.get("input_tokens")?.as_i64()?;
    let output = u.get("output_tokens")?.as_i64()?;
    let cache_read = u.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    // nested ephemeral breakdown wins over the flat cache_creation_input_tokens
    let cache_creation = match u.get("cache_creation").filter(|v| v.is_object()) {
        Some(b) => {
            b.get("ephemeral_5m_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0)
                + b.get("ephemeral_1h_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0)
        }
        None => u.get("cache_creation_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
    };
    if input + output + cache_read + cache_creation <= 0 {
        return None; // zero rows (synthetic error turns) carry no token information
    }
    let ts = r.get("timestamp").and_then(|v| v.as_str()).and_then(parse_ts)?;
    let speed_fast = u.get("speed").and_then(|v| v.as_str()) == Some("fast");
    let model = m.get("model").and_then(|v| v.as_str()).and_then(|s| {
        if s.is_empty() || s == "<synthetic>" {
            None // tokens still count; no model attribution
        } else if speed_fast {
            Some(format!("{}-fast", s))
        } else {
            Some(s.to_string())
        }
    });
    Some(ClaudeRec {
        id: m.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from),
        request_id: r.get("requestId").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from),
        sidechain: r.get("isSidechain").and_then(|v| v.as_bool()).unwrap_or(false),
        rec: UsageRec { ts, model, input, output, cache_read, cache_creation },
    })
}

/// Message ids that older ccbud gateway builds stamped on EVERY translated response — known
/// non-unique, so they must never act as a de-dup key (an id-keyed de-dup would collapse whole
/// weeks of history written through the gateway into a single counted turn).
fn degenerate_id(id: &str) -> bool {
    id == "msg_ccbud" || id == "chatcmpl-ccbud" || id == "resp_ccbud"
}

/// Global de-dup, ccusage semantics: key (message.id, requestId); entries without an id are always
/// kept. A miss on the exact key falls back to the id-only bucket when either side is a sidechain
/// (a `/btw` replay reuses the parent's message.id under a new requestId). On a duplicate the
/// non-sidechain copy wins, then the higher token total.
fn dedup_claude(recs: Vec<ClaudeRec>) -> Vec<ClaudeRec> {
    let mut kept: Vec<ClaudeRec> = vec![];
    let mut by_exact: HashMap<(String, Option<String>), usize> = HashMap::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for cand in recs {
        let Some(id) = cand.id.clone().filter(|i| !degenerate_id(i)) else {
            kept.push(cand);
            continue;
        };
        let exact = (id.clone(), cand.request_id.clone());
        let slot = by_exact.get(&exact).copied().or_else(|| {
            by_id.get(&id).copied().filter(|&i| cand.sidechain || kept[i].sidechain)
        });
        match slot {
            Some(i) => {
                let cur = &kept[i];
                let replace = (cur.sidechain && !cand.sidechain)
                    || (cur.sidechain == cand.sidechain && cand.rec.total() > cur.rec.total());
                if replace {
                    kept[i] = cand;
                }
                by_exact.insert(exact, i);
            }
            None => {
                let i = kept.len();
                by_exact.insert(exact, i);
                by_id.entry(id).or_insert(i);
                kept.push(cand);
            }
        }
    }
    kept
}

// ---------------------------------------------------------------------------
// Codex (sessions/ + archived_sessions/ trees)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
struct CodexUsage {
    input: i64,
    cached: i64,
    output: i64,
    reasoning: i64,
    total: i64,
}

/// Lenient token-usage decode (ccusage accepts several field aliases per component).
fn codex_usage_of(v: &Value) -> Option<CodexUsage> {
    let o = v.as_object()?;
    let g = |keys: &[&str]| keys.iter().find_map(|k| o.get(*k).and_then(|v| v.as_i64())).unwrap_or(0);
    let input = g(&["input_tokens", "prompt_tokens", "input"]);
    let cached = g(&["cached_input_tokens", "cache_read_input_tokens", "cached_tokens"]);
    let output = g(&["output_tokens", "completion_tokens", "output"]);
    let reasoning = g(&["reasoning_output_tokens", "reasoning_tokens"]);
    let total = match o.get("total_tokens").and_then(|v| v.as_i64()) {
        Some(t) if t > 0 || input + output + reasoning == 0 => t,
        _ => input + output + reasoning,
    };
    Some(CodexUsage { input, cached, output, reasoning, total })
}

fn codex_usage_sub(cur: CodexUsage, prev: Option<CodexUsage>) -> CodexUsage {
    let p = prev.unwrap_or_default();
    CodexUsage {
        input: (cur.input - p.input).max(0),
        cached: (cur.cached - p.cached).max(0),
        output: (cur.output - p.output).max(0),
        reasoning: (cur.reasoning - p.reasoning).max(0),
        total: (cur.total - p.total).max(0),
    }
}

fn codex_model_of(v: Option<&Value>) -> Option<String> {
    let o = v?.as_object()?;
    o.get("model")
        .or_else(|| o.get("model_name"))
        .or_else(|| o.get("metadata").and_then(|m| m.get("model")))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Whether this rollout is a `thread_spawn` subagent session (marker in the file head).
fn codex_is_subagent(file: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(file) else { return false };
    let mut buf = [0u8; 16 * 1024];
    let n = f.read(&mut buf).unwrap_or(0);
    buf[..n].windows(b"thread_spawn".len()).any(|w| w == b"thread_spawn")
}

/// A subagent file replays the parent's token_count history as a leading burst that shares one
/// timestamp-second — detect that second (the first two usage events landing on the same second),
/// so the replay can be skipped while the cumulative baseline still advances.
fn codex_replay_second(file: &Path) -> Option<String> {
    let mut first: Option<String> = None;
    let mut lines = LossyLines::open(file)?;
    while let Some(line) = lines.next_line() {
        let Some((ts, payload)) = codex_token_count_line(&line) else { continue };
        let info = payload.get("info");
        let has_usage = info
            .map(|i| i.get("last_token_usage").is_some() || i.get("total_token_usage").is_some())
            .unwrap_or(false);
        if !has_usage {
            continue;
        }
        let second: String = ts.chars().take(19).collect();
        match &first {
            None => first = Some(second),
            Some(f) => return if *f == second { Some(second) } else { None },
        }
    }
    None
}

/// Parse a line as a `token_count` event → (timestamp, payload). None for everything else.
fn codex_token_count_line(line: &str) -> Option<(String, Value)> {
    if !line.contains("token_count") {
        return None;
    }
    let r: Value = serde_json::from_str(line).ok()?;
    if r.get("type").and_then(|v| v.as_str()) != Some("event_msg") {
        return None;
    }
    let p = r.get("payload")?;
    if p.get("type").and_then(|v| v.as_str()) != Some("token_count") {
        return None;
    }
    let ts = r.get("timestamp").and_then(|v| v.as_str())?.to_string();
    Some((ts, p.clone()))
}

/// Parse one Codex rollout file into per-turn usage events (ccusage semantics — see module doc).
fn parse_codex_file(file: &Path, out: &mut Vec<(CodexUsage, i64, String)>) {
    let replay_second = if codex_is_subagent(file) { codex_replay_second(file) } else { None };
    let mut skip_replay = replay_second.is_some();
    let mut current_model: Option<String> = None;
    let mut prev_totals: Option<CodexUsage> = None;
    let Some(mut lines) = LossyLines::open(file) else { return };
    while let Some(line) = lines.next_line() {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        // turn_context carries the active model
        if s.contains("turn_context") {
            if let Ok(r) = serde_json::from_str::<Value>(s) {
                if r.get("type").and_then(|v| v.as_str()) == Some("turn_context") {
                    if let Some(m) = codex_model_of(r.get("payload")) {
                        current_model = Some(m);
                    }
                    continue;
                }
            }
        }
        let Some((ts_str, payload)) = codex_token_count_line(s) else { continue };
        let info = payload.get("info").filter(|i| !i.is_null());
        let total = info.and_then(|i| i.get("total_token_usage")).and_then(codex_usage_of);
        let last = info.and_then(|i| i.get("last_token_usage")).and_then(codex_usage_of);
        // leading parent-history replay in a subagent file: skip, but keep the baseline moving
        if skip_replay {
            let second: String = ts_str.chars().take(19).collect();
            if Some(&second) == replay_second.as_ref() {
                if let Some(t) = total {
                    prev_totals = Some(t);
                }
                continue;
            }
            skip_replay = false;
        }
        let usage = last.or_else(|| total.map(|t| codex_usage_sub(t, prev_totals)));
        if let Some(t) = total {
            prev_totals = Some(t);
        }
        let Some(mut u) = usage else { continue };
        if u.input + u.cached + u.output + u.reasoning == 0 {
            continue;
        }
        let Some(ts) = parse_ts(&ts_str) else { continue };
        u.cached = u.cached.min(u.input); // input is INCLUSIVE of cached
        let model = codex_model_of(Some(&payload))
            .or_else(|| codex_model_of(info))
            .or_else(|| current_model.clone())
            .unwrap_or_else(|| "gpt-5".to_string());
        out.push((u, ts, model));
    }
}

/// Collect a work dir's Codex rollout files: sessions/ plus archived_sessions/, where an archived
/// copy of the same relative path loses to the active sessions/ copy.
fn codex_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![];
    let mut seen_rel: HashSet<PathBuf> = HashSet::new();
    for sub in ["sessions", "archived_sessions"] {
        let dir = root.join(sub);
        let mut files = vec![];
        collect_jsonl(&dir, 0, &mut files);
        files.sort();
        for f in files {
            let rel = f.strip_prefix(&dir).map(|p| p.to_path_buf()).unwrap_or_else(|_| f.clone());
            if seen_rel.insert(rel) {
                out.push(f);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// aggregation
// ---------------------------------------------------------------------------

fn build_data(config: &Value, active: &str) -> HashMap<String, Day> {
    let mut days: HashMap<String, Day> = HashMap::new();
    let roots = active_roots(config, active);

    // Claude Code: parse everything, then de-dup globally, then bucket.
    let mut claude_recs: Vec<ClaudeRec> = vec![];
    for root in &roots {
        let mut files = vec![];
        collect_jsonl(&root.join("projects"), 0, &mut files);
        files.sort();
        for file in files {
            let Some(mut lines) = LossyLines::open(&file) else { continue };
            while let Some(line) = lines.next_line() {
                if let Some(rec) = parse_claude_line(line.trim()) {
                    claude_recs.push(rec);
                }
            }
        }
    }
    for kept in dedup_claude(claude_recs) {
        bump(&mut days, &kept.rec);
    }

    // Codex: per-turn events, de-duped globally by (timestamp, model, tokens) so resumed/forked
    // session copies collapse.
    let mut events: Vec<(CodexUsage, i64, String)> = vec![];
    for root in &roots {
        for file in codex_files(root) {
            parse_codex_file(&file, &mut events);
        }
    }
    let mut seen: HashSet<(i64, String, CodexUsage)> = HashSet::new();
    for (u, ts, model) in events {
        if !seen.insert((ts, model.clone(), u)) {
            continue;
        }
        bump(
            &mut days,
            &UsageRec {
                ts,
                model: Some(model),
                input: (u.input - u.cached).max(0),
                output: u.output,
                cache_read: u.cached,
                cache_creation: 0,
            },
        );
    }
    days
}

// ---- usage cache ----
// build_data scans every history .jsonl (~0.5s cold for ~1200 files), and the popover calls
// usage_get TWICE per open (heatmap "all" + stats range). Cache the scanned per-day map keyed by
// the active dirs, invalidated when history files change (notify watcher) — so the second per-open
// call + repeated opens are instant, and a startup/post-change warm makes the first open instant.
struct UsageCache {
    sig: String,
    days: HashMap<String, Day>,
}
static USAGE_CACHE: std::sync::Mutex<Option<UsageCache>> = std::sync::Mutex::new(None);

fn dirs_sig(config: &Value, active: &str) -> String {
    format!("{}|{:?}", active, active_roots(config, active))
}

fn build_data_cached(config: &Value, active: &str) -> HashMap<String, Day> {
    let sig = dirs_sig(config, active);
    {
        // recover a poisoned lock (a panicked scan thread must not disable caching forever)
        let cache = USAGE_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(c) = cache.as_ref() {
            if c.sig == sig {
                return c.days.clone();
            }
        }
    }
    let days = build_data(config, active);
    let mut cache = USAGE_CACHE.lock().unwrap_or_else(|p| p.into_inner());
    *cache = Some(UsageCache { sig, days: days.clone() });
    days
}

/// Drop the cached scan — call when history files change so the next read rescans.
pub fn invalidate_cache() {
    let mut cache = USAGE_CACHE.lock().unwrap_or_else(|p| p.into_inner());
    *cache = None;
}

/// Scan + cache now (off the click path). Call at startup and after history changes so the first
/// popover open is instant instead of paying the cold-scan cost.
pub fn warm_cache(config: &Value, active: &str) {
    let _ = build_data_cached(config, active);
}

fn range_keys(days: &HashMap<String, Day>, range: &str, now: i64) -> Vec<String> {
    let mut all: Vec<String> = days.keys().cloned().collect();
    all.sort();
    if range == "all" {
        return all;
    }
    let n = match range {
        "1d" => 1,
        "30d" => 30,
        _ => 7,
    };
    let cut = start_of_day(now - (n - 1) * DAY_MS);
    all.into_iter().filter(|k| ms_of_key(k) >= cut).collect()
}

fn top_key(map: &HashMap<String, i64>) -> Option<String> {
    map.iter().max_by_key(|(_, v)| **v).map(|(k, _)| k.clone())
}

fn streaks(days: &HashMap<String, Day>, now: i64) -> (i64, i64) {
    let mut active: Vec<i64> = days
        .iter()
        .filter(|(_, d)| d.requests > 0)
        .map(|(k, _)| ms_of_key(k))
        .collect();
    active.sort();
    let set: HashSet<i64> = active.iter().cloned().collect();
    let (mut longest, mut run, mut prev): (i64, i64, Option<i64>) = (0, 0, None);
    for t in &active {
        run = if prev.map(|p| t - p == DAY_MS).unwrap_or(false) { run + 1 } else { 1 };
        prev = Some(*t);
        if run > longest {
            longest = run;
        }
    }
    let mut cur = 0;
    let mut t = start_of_day(now);
    if !set.contains(&t) {
        t -= DAY_MS;
    }
    while set.contains(&t) {
        cur += 1;
        t -= DAY_MS;
    }
    (cur, longest)
}

fn build_heatmap(days: &HashMap<String, Day>, weeks: i64, now: i64) -> Vec<Value> {
    let today = start_of_day(now);
    let span = weeks * 7;
    let mut start = today - (span - 1) * DAY_MS;
    let dow = Local.timestamp_millis_opt(start).single().map(|d| d.weekday().num_days_from_sunday() as i64).unwrap_or(0);
    start -= dow * DAY_MS;
    let mut cells: Vec<(String, i64)> = vec![];
    let mut max = 1i64;
    let mut t = start;
    while t <= today {
        let k = key_of(t);
        let tok = days.get(&k).map(|d| d.tokens).unwrap_or(0);
        if tok > max {
            max = tok;
        }
        cells.push((k, tok));
        t += DAY_MS;
    }
    cells
        .into_iter()
        .map(|(date, tokens)| {
            let r = tokens as f64 / max as f64;
            let level = if tokens == 0 {
                0
            } else if r > 0.66 {
                4
            } else if r > 0.33 {
                3
            } else if r > 0.1 {
                2
            } else {
                1
            };
            json!({ "date": date, "tokens": tokens, "level": level })
        })
        .collect()
}

fn query(days: &HashMap<String, Day>, range: &str, now: i64) -> Value {
    let keys = range_keys(days, range, now);
    let (mut tokens, mut input, mut output, mut cache_read, mut cache_creation, mut requests) = (0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
    let mut models: HashMap<String, i64> = HashMap::new();
    let mut providers: HashMap<String, i64> = HashMap::new();
    let mut hours: HashMap<u32, i64> = HashMap::new();
    let mut active_days = 0;
    for k in &keys {
        if let Some(d) = days.get(k) {
            tokens += d.tokens;
            input += d.input;
            output += d.output;
            cache_read += d.cache_read;
            cache_creation += d.cache_creation;
            requests += d.requests;
            if d.requests > 0 {
                active_days += 1;
            }
            for (m, v) in &d.models {
                *models.entry(m.clone()).or_insert(0) += v;
            }
            for (p, v) in &d.providers {
                *providers.entry(p.clone()).or_insert(0) += v;
            }
            for (h, v) in &d.hours {
                *hours.entry(*h).or_insert(0) += v;
            }
        }
    }
    let mut by_model: Vec<Value> = models
        .iter()
        .map(|(m, t)| json!({ "model": m, "tokens": t, "pct": if tokens > 0 { *t as f64 / tokens as f64 } else { 0.0 } }))
        .collect();
    by_model.sort_by(|a, b| b["tokens"].as_i64().unwrap_or(0).cmp(&a["tokens"].as_i64().unwrap_or(0)));
    let mut by_provider: Vec<Value> = providers
        .iter()
        .map(|(p, t)| json!({ "provider": p, "tokens": t, "pct": if tokens > 0 { *t as f64 / tokens as f64 } else { 0.0 } }))
        .collect();
    by_provider.sort_by(|a, b| b["tokens"].as_i64().unwrap_or(0).cmp(&a["tokens"].as_i64().unwrap_or(0)));
    let peak_hour = hours.iter().max_by_key(|(_, v)| **v).map(|(h, _)| *h as i64);
    let (cur, longest) = streaks(days, now);

    json!({
        "range": range,
        "tokens": tokens, "input": input, "output": output, "cacheRead": cache_read, "cacheCreation": cache_creation,
        "requests": requests, "activeDays": active_days,
        "peakHour": peak_hour,
        "favoriteModel": top_key(&models),
        "favoriteProvider": top_key(&providers),
        "byModel": by_model,
        "byProvider": by_provider,
        "currentStreak": cur,
        "longestStreak": longest,
        "heatmap": build_heatmap(days, HEATMAP_WEEKS, now),
    })
}

/// One-line scan diagnostic for the gateway log — which dirs resolved, how many files/lines
/// reached each pipeline stage, and the day span. Makes an empty/partial aggregation visible
/// without a debugger ("only today shows up" → the counters name the stage that dropped it).
pub fn diag(config: &Value, active: &str) -> String {
    let roots = active_roots(config, active);
    let mut claude_files = 0usize;
    let mut codex_file_count = 0usize;
    let (mut usage_lines, mut parsed, mut zero_rows, mut no_ts, mut degen) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let mut recs: Vec<ClaudeRec> = vec![];
    for root in &roots {
        let mut files = vec![];
        collect_jsonl(&root.join("projects"), 0, &mut files);
        claude_files += files.len();
        for f in &files {
            let Some(mut lines) = LossyLines::open(f) else { continue };
            while let Some(l) = lines.next_line() {
                let l = l.trim();
                if !l.contains("\"usage\"") {
                    continue;
                }
                usage_lines += 1;
                match parse_claude_line(l) {
                    Some(r) => {
                        parsed += 1;
                        if r.id.as_deref().map(degenerate_id).unwrap_or(false) {
                            degen += 1;
                        }
                        recs.push(r);
                    }
                    None => {
                        if let Ok(v) = serde_json::from_str::<Value>(l) {
                            if v.get("message").and_then(|m| m.get("usage")).is_some() {
                                if v.get("timestamp").and_then(|t| t.as_str()).and_then(parse_ts).is_none() {
                                    no_ts += 1;
                                } else {
                                    zero_rows += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        codex_file_count += codex_files(root).len();
    }
    let kept = dedup_claude(recs).len();
    let days = build_data(config, active);
    let mut keys: Vec<_> = days.keys().cloned().collect();
    keys.sort();
    let span = match (keys.first(), keys.last()) {
        (Some(a), Some(b)) => format!("{}..{}", a, b),
        _ => "-".to_string(),
    };
    format!(
        "usage scan: active={} roots={:?} claude-files={} codex-files={} usage-lines={} parsed={} kept={} days={} span={} dropped(no-ts)={} dropped(zero/invalid)={} degenerate-ids={}",
        active,
        roots.iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>(),
        claude_files, codex_file_count, usage_lines, parsed, kept, keys.len(), span, no_ts, zero_rows, degen
    )
}

/// Public entry: aggregate the active dirs and return the usage stats payload for `range`.
pub fn usage_get(config: &Value, active: &str, range: &str) -> Value {
    let days = build_data_cached(config, active);
    let now = Local::now().timestamp_millis();
    query(&days, range, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn line(v: Value) -> String {
        format!("{}\n", v)
    }
    fn asst(id: &str, req: &str, model: &str, ts: &str, inp: i64, out: i64) -> String {
        line(json!({ "type": "assistant", "timestamp": ts, "requestId": req,
            "message": { "id": id, "model": model,
                "usage": { "input_tokens": inp, "output_tokens": out } } }))
    }

    fn sum(days: &HashMap<String, Day>) -> (i64, i64, i64, i64, i64, HashMap<String, i64>) {
        let (mut tokens, mut input, mut output, mut cache_read, mut requests) = (0i64, 0i64, 0i64, 0i64, 0i64);
        let mut models: HashMap<String, i64> = HashMap::new();
        for d in days.values() {
            tokens += d.tokens;
            input += d.input;
            output += d.output;
            cache_read += d.cache_read;
            requests += d.requests;
            for (m, v) in &d.models {
                *models.entry(m.clone()).or_insert(0) += v;
            }
        }
        (tokens, input, output, cache_read, requests, models)
    }

    #[test]
    fn claude_ccusage_semantics() {
        let base = std::env::temp_dir().join(format!("ccbud-usage-cl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let proj = base.join("projects").join("-p");
        // nested session dir + subagent transcript at arbitrary depth — recursive walk finds both
        let deep = proj.join("s1").join("subagents");
        fs::create_dir_all(&deep).unwrap();

        fs::write(
            proj.join("s1.jsonl"),
            // counted (110)
            asst("m1", "r1", "claude-x", "2026-07-01T10:00:00Z", 100, 10)
                // same (id, requestId) duplicate → collapsed
                + &asst("m1", "r1", "claude-x", "2026-07-01T10:00:00Z", 100, 10)
                // same id, DIFFERENT requestId, no sidechain → distinct entry (counted, 55)
                + &asst("m1", "r2", "claude-x", "2026-07-01T10:05:00Z", 50, 5)
                // undated → dropped
                + &line(json!({ "type": "assistant",
                    "message": { "id": "m2", "model": "claude-x", "usage": { "input_tokens": 9, "output_tokens": 9 } } }))
                // zero usage → dropped
                + &asst("m3", "r3", "<synthetic>", "2026-07-01T10:06:00Z", 0, 0)
                // synthetic model with tokens → counted (7), no model attribution
                + &asst("m4", "r4", "<synthetic>", "2026-07-01T10:07:00Z", 5, 2)
                // no type field at all (ccusage has no type gate) → counted (13)
                + &line(json!({ "timestamp": "2026-07-01T10:08:00Z", "requestId": "r5",
                    "message": { "id": "m5", "model": "claude-x",
                        "usage": { "input_tokens": 10, "output_tokens": 3 } } })),
        )
        .unwrap();
        // subagent transcript, nested cache_creation breakdown + fast speed suffix (counted, 3+4+6+7=20)
        fs::write(
            deep.join("agent-a.jsonl"),
            line(json!({ "timestamp": "2026-07-01T11:00:00Z", "requestId": "r6",
                "message": { "id": "m6", "model": "claude-x",
                    "usage": { "input_tokens": 3, "output_tokens": 4, "speed": "fast",
                        "cache_read_input_tokens": 6,
                        "cache_creation_input_tokens": 999,
                        "cache_creation": { "ephemeral_5m_input_tokens": 5, "ephemeral_1h_input_tokens": 2 } } } })),
        )
        .unwrap();
        // sidechain replay: reuses m1 under a NEW requestId with isSidechain → collapses onto parent
        fs::write(
            proj.join("s2.jsonl"),
            line(json!({ "type": "assistant", "timestamp": "2026-07-01T10:00:01Z", "requestId": "r9",
                "isSidechain": true,
                "message": { "id": "m1", "model": "claude-x", "usage": { "input_tokens": 100, "output_tokens": 10 } } })),
        )
        .unwrap();

        let config = json!({ "historyDirs": [ base.to_string_lossy() ] });
        let days = build_data(&config, "all");
        let (tokens, input, output, cache_read, requests, models) = sum(&days);
        // m1(110) + m1/r2(55) + m4(7) + m5(13) + m6(20)
        assert_eq!(requests, 5);
        assert_eq!(input, 100 + 50 + 5 + 10 + 3);
        assert_eq!(output, 10 + 5 + 2 + 3 + 4);
        assert_eq!(cache_read, 6);
        assert_eq!(tokens, 110 + 55 + 7 + 13 + 20);
        // synthetic tokens counted but unattributed; fast suffix applied
        assert_eq!(models.get("claude-x").copied(), Some(110 + 55 + 13));
        assert_eq!(models.get("claude-x-fast").copied(), Some(20));
        assert!(models.get("<synthetic>").is_none());

        let _ = fs::remove_dir_all(&base);
    }

    // History written through OLD ccbud gateway builds: every streamed response carries the
    // constant id "msg_ccbud" (and often no requestId — the gateway didn't forward the header).
    // Those ids must never act as de-dup keys, or weeks of history collapse into one turn.
    #[test]
    fn degenerate_gateway_ids_never_dedup() {
        let base = std::env::temp_dir().join(format!("ccbud-usage-degen-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let proj = base.join("projects").join("-p");
        fs::create_dir_all(&proj).unwrap();
        let no_req = |ts: &str, inp: i64| {
            line(json!({ "type": "assistant", "timestamp": ts,
                "message": { "id": "msg_ccbud", "model": "glm-4.7",
                    "usage": { "input_tokens": inp, "output_tokens": 1 } } }))
        };
        fs::write(
            proj.join("old-era.jsonl"),
            no_req("2026-06-20T10:00:00Z", 100)
                + &no_req("2026-06-21T10:00:00Z", 200)
                + &no_req("2026-06-22T10:00:00Z", 300)
                + &line(json!({ "type": "assistant", "timestamp": "2026-06-23T10:00:00Z", "requestId": "r1",
                    "message": { "id": "chatcmpl-ccbud", "model": "glm-4.7",
                        "usage": { "input_tokens": 400, "output_tokens": 1 } } })),
        )
        .unwrap();
        let config = json!({ "historyDirs": [ base.to_string_lossy() ] });
        let days = build_data(&config, "all");
        let (_, input, _, _, requests, _) = sum(&days);
        // all four turns count — four distinct days survive
        assert_eq!(requests, 4);
        assert_eq!(input, 100 + 200 + 300 + 400);
        assert_eq!(days.len(), 4);
        let _ = fs::remove_dir_all(&base);
    }

    // The 对话 page's dir switcher persists synthetic views (recycle bin, imported bundles) into
    // historyActive — those match no configured dir and previously zeroed every usage number.
    #[test]
    fn synthetic_or_stale_active_falls_back_to_all_dirs() {
        let base = std::env::temp_dir().join(format!("ccbud-usage-active-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let proj = base.join("projects").join("-p");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("s.jsonl"), asst("a1", "r1", "m", "2026-07-01T10:00:00Z", 10, 1)).unwrap();
        let config = json!({ "historyDirs": [ base.to_string_lossy() ] });
        for active in ["all", "__trash__", "__imported__", "/no/such/dir"] {
            let days = build_data(&config, active);
            let (tokens, ..) = sum(&days);
            assert_eq!(tokens, 11, "active={} must not zero the stats", active);
        }
        // a VALID selector still filters
        let days = build_data(&config, base.to_string_lossy().as_ref());
        let (tokens, ..) = sum(&days);
        assert_eq!(tokens, 11);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn invalid_utf8_does_not_truncate_a_file() {
        let base = std::env::temp_dir().join(format!("ccbud-usage-u8-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let proj = base.join("projects").join("-p");
        fs::create_dir_all(&proj).unwrap();
        let mut bytes = asst("u1", "r1", "m", "2026-07-01T10:00:00Z", 10, 1).into_bytes();
        bytes.extend_from_slice(b"{\"garbage\": \"\xff\xfe binary tool output\"}\n");
        bytes.extend_from_slice(asst("u2", "r2", "m", "2026-07-02T10:00:00Z", 20, 2).as_bytes());
        fs::write(proj.join("s.jsonl"), bytes).unwrap();

        let config = json!({ "historyDirs": [ base.to_string_lossy() ] });
        let days = build_data(&config, "all");
        let (_, input, _, _, requests, _) = sum(&days);
        // the record AFTER the invalid-UTF-8 line still counts
        assert_eq!(requests, 2);
        assert_eq!(input, 30);
        let _ = fs::remove_dir_all(&base);
    }

    fn tc(ts: &str, last: Option<(i64, i64, i64)>, total: Option<(i64, i64, i64)>) -> String {
        let mut info = json!({});
        if let Some((i, c, o)) = last {
            info["last_token_usage"] = json!({ "input_tokens": i, "cached_input_tokens": c, "output_tokens": o,
                "total_tokens": i + o });
        }
        if let Some((i, c, o)) = total {
            info["total_token_usage"] = json!({ "input_tokens": i, "cached_input_tokens": c, "output_tokens": o,
                "total_tokens": i + o });
        }
        line(json!({ "timestamp": ts, "type": "event_msg", "payload": { "type": "token_count", "info": info } }))
    }

    #[test]
    fn codex_ccusage_semantics() {
        let base = std::env::temp_dir().join(format!("ccbud-usage-cx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let day = base.join("sessions").join("2026").join("07").join("01");
        fs::create_dir_all(&day).unwrap();

        // main session: model from turn_context; one last_token_usage turn; one turn WITHOUT
        // last (only cumulative total) → counted as the diff from the baseline.
        fs::write(
            day.join("rollout-a.jsonl"),
            line(json!({ "timestamp": "2026-07-01T12:00:00Z", "type": "session_meta", "payload": { "id": "a" } }))
                + &line(json!({ "timestamp": "2026-07-01T12:00:01Z", "type": "turn_context", "payload": { "model": "gpt-5.5" } }))
                + &tc("2026-07-01T12:00:02Z", Some((900, 600, 80)), Some((900, 600, 80)))
                + &tc("2026-07-01T12:00:03Z", None, Some((1400, 900, 130))) // diff: 500/300/50
                + &tc("2026-07-01T12:00:04Z", None, None), // info without usage → skipped
        )
        .unwrap();
        // resumed copy of the same session: identical events must de-dup, a new turn counts.
        fs::write(
            day.join("rollout-b.jsonl"),
            line(json!({ "timestamp": "2026-07-01T12:10:00Z", "type": "turn_context", "payload": { "model": "gpt-5.5" } }))
                + &tc("2026-07-01T12:00:02Z", Some((900, 600, 80)), None) // duplicate of a's turn 1
                + &tc("2026-07-01T12:10:01Z", Some((10, 0, 5)), None), // new turn (15)
        )
        .unwrap();
        // archived copy of rollout-a (same relative path) → file-level de-dup, never read twice.
        let arch = base.join("archived_sessions").join("2026").join("07").join("01");
        fs::create_dir_all(&arch).unwrap();
        fs::write(arch.join("rollout-a.jsonl"), tc("2026-07-01T12:00:02Z", Some((900, 600, 80)), None)).unwrap();
        // thread_spawn subagent: leading replay burst (same second) skipped, own turn counted,
        // and the baseline carried from the replayed cumulative total.
        fs::write(
            day.join("rollout-sub.jsonl"),
            line(json!({ "timestamp": "2026-07-01T13:00:00Z", "type": "session_meta",
                "payload": { "id": "sub", "source": { "type": "thread_spawn" } } }))
                + &tc("2026-07-01T13:00:01Z", Some((900, 600, 80)), Some((900, 600, 80)))
                + &tc("2026-07-01T13:00:01Z", Some((500, 300, 50)), Some((1400, 900, 130)))
                + &tc("2026-07-01T13:00:05Z", None, Some((1600, 900, 160))), // own turn: diff 200/0/30
        )
        .unwrap();

        let config = json!({ "historyDirs": [ base.to_string_lossy() ] });
        let days = build_data(&config, "all");
        let (tokens, input, output, cache_read, requests, models) = sum(&days);
        // a#1: in 900 (cached 600) out 80 → input 300, cacheRead 600, out 80  (980)
        // a#2 (diff): in 500 (cached 300) out 50 → input 200, cacheRead 300, out 50 (550)
        // b#2: 10/0/5 (15)
        // sub own turn: 200/0/30 (230)
        assert_eq!(requests, 4);
        assert_eq!(input, 300 + 200 + 10 + 200);
        assert_eq!(cache_read, 600 + 300);
        assert_eq!(output, 80 + 50 + 5 + 30);
        assert_eq!(tokens, 980 + 550 + 15 + 230);
        assert_eq!(models.get("gpt-5.5").copied(), Some(980 + 550 + 15));
        // subagent file had no turn_context → fallback model
        assert_eq!(models.get("gpt-5").copied(), Some(230));

        let _ = fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod real_data_probe {
    use super::*;

    // Diagnostic harness (not an assertion): aggregate a REAL history dir and print per-range
    // totals, so the implementation can be diffed against `ccusage` on the same data.
    // Run: CCBUD_PROBE_DIR=~/.claude cargo test --lib probe_real_dir -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_real_dir() {
        let Ok(dir) = std::env::var("CCBUD_PROBE_DIR") else {
            eprintln!("set CCBUD_PROBE_DIR");
            return;
        };
        // parse-level diagnostics: where do lines fall out of the pipeline?
        let root = expand_tilde(&dir);
        let mut files = vec![];
        collect_jsonl(&root.join("projects"), 0, &mut files);
        let (mut n_files, mut n_usage_lines, mut n_parsed, mut n_no_ts, mut n_degen) = (0u64, 0u64, 0u64, 0u64, 0u64);
        for file in &files {
            n_files += 1;
            let Some(mut lines) = LossyLines::open(file) else { continue };
            while let Some(l) = lines.next_line() {
                let l = l.trim();
                if !l.contains("\"usage\"") {
                    continue;
                }
                n_usage_lines += 1;
                match parse_claude_line(l) {
                    Some(rec) => {
                        n_parsed += 1;
                        if rec.id.as_deref().map(degenerate_id).unwrap_or(false) {
                            n_degen += 1;
                        }
                    }
                    None => {
                        // distinguish the "usage present but timestamp bad/missing" case
                        if let Ok(v) = serde_json::from_str::<Value>(l) {
                            if v.get("message").and_then(|m| m.get("usage")).is_some()
                                && v.get("timestamp").and_then(|t| t.as_str()).and_then(parse_ts).is_none()
                            {
                                n_no_ts += 1;
                            }
                        }
                    }
                }
            }
        }
        eprintln!(
            "claude files={} usage-lines={} parsed={} dropped-no-ts={} degenerate-id={}",
            n_files, n_usage_lines, n_parsed, n_no_ts, n_degen
        );
        let config = json!({ "historyDirs": [dir] });
        let days = build_data(&config, "all");
        let now = Local::now().timestamp_millis();
        let mut keys: Vec<_> = days.keys().cloned().collect();
        keys.sort();
        for k in &keys {
            let d = &days[k];
            eprintln!("{}  tokens={} in={} out={} cr={} cc={} req={}", k, d.tokens, d.input, d.output, d.cache_read, d.cache_creation, d.requests);
        }
        for range in ["1d", "7d", "30d", "all"] {
            let q = query(&days, range, now);
            eprintln!("range {:>3}: tokens={} requests={}", range, q["tokens"], q["requests"]);
        }
    }
}

#[cfg(test)]
mod diag_probe {
    use super::*;
    #[test]
    #[ignore]
    fn probe_diag() {
        let Ok(dir) = std::env::var("CCBUD_PROBE_DIR") else { return };
        eprintln!("{}", diag(&json!({ "historyDirs": [dir] }), "all"));
    }
}
