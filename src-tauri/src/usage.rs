// Usage analytics — Rust port of insights.js + usage.js.
//
// Aggregates token usage from on-disk history across the active config dirs into per-day buckets,
// then computes the stats payload the renderer's usage panel / tray render (tokens, requests,
// byModel, heatmap, streaks). Two session trees contribute per work dir:
//   - Claude Code `projects/` .jsonl — assistant records' message.usage, de-duped by message.id
//     (a resumed/forked session repeats earlier messages in a new file), incl. per-session
//     subagent transcripts (`<proj>/<session>/subagents/agent-*.jsonl`).
//   - Codex `sessions/` rollout .jsonl — `token_count` events (one per model turn; the model comes
//     from the preceding `turn_context`). Rollouts append in place and forks re-persist only the
//     conversation items, so lines are counted as-is without a cross-file id.
// Day bucketing is local-timezone (chrono::Local) to match usage.js exactly.

#![allow(dead_code)]

use chrono::{Datelike, Local, TimeZone, Timelike};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const DAY_MS: i64 = 86_400_000;
const HEATMAP_WEEKS: i64 = 26;
const MAX_FILE: u64 = 64 * 1024 * 1024;

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

/// Active work dirs (honors the directory switcher; imported dirs excluded).
fn active_roots(config: &Value, active: &str) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Some(arr) = config.get("historyDirs").and_then(|v| v.as_array()) {
        for d in arr {
            if let Some(s) = d.as_str() {
                if active == "all" || active == s {
                    out.push(expand_tilde(s));
                }
            }
        }
    }
    out
}

/// Claude Code projects trees of the active dirs.
fn active_dirs(config: &Value, active: &str) -> Vec<PathBuf> {
    active_roots(config, active).into_iter().map(|r| r.join("projects")).collect()
}

/// Codex sessions trees of the active dirs (rollout-*.jsonl live under sessions/YYYY/MM/DD/).
fn active_codex_dirs(config: &Value, active: &str) -> Vec<PathBuf> {
    active_roots(config, active).into_iter().map(|r| r.join("sessions")).collect()
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

struct UsageRec {
    id: Option<String>,
    ts: Option<i64>,
    model: String,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
}

fn parse_assistant_usage(raw: &str) -> Vec<UsageRec> {
    let mut out = vec![];
    for line in raw.split('\n') {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        let r: Value = match serde_json::from_str(s) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if r.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let m = match r.get("message") {
            Some(m) => m,
            None => continue,
        };
        let u = match m.get("usage") {
            Some(u) => u,
            None => continue,
        };
        let inp = u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let outp = u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let cr = u.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let cc = u.get("cache_creation_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        if inp + outp + cr + cc == 0 {
            continue;
        }
        out.push(UsageRec {
            id: m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()),
            ts: r.get("timestamp").and_then(|v| v.as_str()).and_then(parse_ts),
            model: m.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
            input: inp,
            output: outp,
            cache_read: cr,
            cache_creation: cc,
        });
    }
    out
}

/// Codex rollout .jsonl → per-turn usage. `token_count` events carry `info.last_token_usage`
/// (input includes the cached portion; split out like codex.rs does for the session viewer);
/// the active model rides the preceding `turn_context` record.
fn parse_codex_usage(raw: &str) -> Vec<UsageRec> {
    let mut out = vec![];
    let mut model = "codex".to_string();
    for line in raw.split('\n') {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        let r: Value = match serde_json::from_str(s) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let p = r.get("payload").cloned().unwrap_or(Value::Null);
        match r.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "turn_context" => {
                if let Some(m) = p.get("model").and_then(|v| v.as_str()) {
                    if !m.is_empty() {
                        model = m.to_string();
                    }
                }
            }
            "event_msg" => {
                if p.get("type").and_then(|v| v.as_str()) != Some("token_count") {
                    continue;
                }
                // info is null on rate-limit-only updates — skip those.
                let u = match p.get("info").and_then(|i| i.get("last_token_usage")) {
                    Some(u) => u,
                    None => continue,
                };
                let input = u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                let cached = u.get("cached_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                let output = u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                if input + cached + output == 0 {
                    continue;
                }
                out.push(UsageRec {
                    id: None, // rollouts append in place; no cross-file duplication to de-dup
                    ts: r.get("timestamp").and_then(|v| v.as_str()).and_then(parse_ts),
                    model: model.clone(),
                    input: (input - cached).max(0),
                    output,
                    cache_read: cached,
                    cache_creation: 0,
                });
            }
            _ => {}
        }
    }
    out
}

fn bump(days: &mut HashMap<String, Day>, rec: &UsageRec, fallback_ts: i64) {
    let ts = rec.ts.unwrap_or(fallback_ts);
    let total = rec.input + rec.output + rec.cache_read + rec.cache_creation;
    let day = days.entry(key_of(ts)).or_default();
    day.requests += 1;
    day.tokens += total;
    day.input += rec.input;
    day.output += rec.output;
    day.cache_read += rec.cache_read;
    day.cache_creation += rec.cache_creation;
    *day.models.entry(rec.model.clone()).or_insert(0) += total;
    *day.hours.entry(hour_of(ts)).or_insert(0) += total;
}

fn each_file(dirs: &[PathBuf], mut cb: impl FnMut(PathBuf)) {
    for root in dirs {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for ent in entries.flatten() {
            let p = ent.path();
            if !p.is_dir() {
                continue;
            }
            let files = match std::fs::read_dir(&p) {
                Ok(f) => f,
                Err(_) => continue,
            };
            for f in files.flatten() {
                let fp = f.path();
                if fp.is_file() && fp.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    cb(fp);
                } else if fp.is_dir() {
                    // Subagent transcripts live one level deeper, per session:
                    // <proj>/<session>/subagents/agent-*.jsonl. (A bare <proj>/subagents dir is
                    // tolerated too for older layouts.)
                    let sub = if fp.file_name().and_then(|n| n.to_str()) == Some("subagents") {
                        fp.clone()
                    } else {
                        fp.join("subagents")
                    };
                    if let Ok(sfiles) = std::fs::read_dir(&sub) {
                        for sf in sfiles.flatten() {
                            let sp = sf.path();
                            if sp.is_file() && sp.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                                cb(sp);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// File metadata gate shared by both trees: (skip >MAX_FILE, mtime fallback ts, contents).
fn read_history_file(file: &PathBuf) -> Option<(i64, String)> {
    let meta = std::fs::metadata(file).ok()?;
    if meta.len() > MAX_FILE {
        return None;
    }
    let fallback_ts = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let raw = std::fs::read_to_string(file).ok()?;
    Some((fallback_ts, raw))
}

fn build_data(config: &Value, active: &str) -> HashMap<String, Day> {
    let mut days: HashMap<String, Day> = HashMap::new();

    // Claude Code projects/ tree (sessions + per-session subagents), de-duped by message.id.
    let mut seen: HashSet<String> = HashSet::new();
    let mut files: Vec<PathBuf> = vec![];
    each_file(&active_dirs(config, active), |f| files.push(f));
    for file in files {
        let Some((fallback_ts, raw)) = read_history_file(&file) else { continue };
        for rec in parse_assistant_usage(&raw) {
            if let Some(id) = &rec.id {
                if !seen.insert(id.clone()) {
                    continue;
                }
            }
            bump(&mut days, &rec, fallback_ts);
        }
    }

    // Codex sessions/ tree (rollout token_count events).
    let mut cx_files: Vec<PathBuf> = vec![];
    for root in active_codex_dirs(config, active) {
        crate::codex::walk_sessions(&root, |f| cx_files.push(f));
    }
    for file in cx_files {
        let Some((fallback_ts, raw)) = read_history_file(&file) else { continue };
        for rec in parse_codex_usage(&raw) {
            bump(&mut days, &rec, fallback_ts);
        }
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
    if let Ok(cache) = USAGE_CACHE.lock() {
        if let Some(c) = cache.as_ref() {
            if c.sig == sig {
                return c.days.clone();
            }
        }
    }
    let days = build_data(config, active);
    if let Ok(mut cache) = USAGE_CACHE.lock() {
        *cache = Some(UsageCache { sig, days: days.clone() });
    }
    days
}

/// Drop the cached scan — call when history files change so the next read rescans.
pub fn invalidate_cache() {
    if let Ok(mut cache) = USAGE_CACHE.lock() {
        *cache = None;
    }
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

    fn asst(id: &str, model: &str, ts: &str, inp: i64, out: i64) -> String {
        format!(
            "{}\n",
            json!({ "type": "assistant", "timestamp": ts,
                "message": { "id": id, "model": model,
                    "usage": { "input_tokens": inp, "output_tokens": out } } })
        )
    }

    // One temp work dir exercising all three sources: main sessions (with a resumed-file
    // duplicate + a zero-usage synthetic turn), a per-session subagent transcript at the REAL
    // depth (<proj>/<session>/subagents/), and a Codex rollout (model from turn_context,
    // cached split out, info-null token_count skipped).
    #[test]
    fn aggregates_sessions_subagents_and_codex() {
        let base = std::env::temp_dir().join(format!("ccbud-usage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let proj = base.join("projects").join("-p");
        fs::create_dir_all(&proj).unwrap();
        fs::write(
            proj.join("s1.jsonl"),
            asst("msg_1", "claude-x", "2026-07-01T10:00:00Z", 100, 10)
                + &asst("msg_zero", "claude-x", "2026-07-01T10:01:00Z", 0, 0),
        )
        .unwrap();
        fs::write(
            proj.join("s2.jsonl"),
            asst("msg_1", "claude-x", "2026-07-01T10:00:00Z", 100, 10)
                + &asst("msg_2", "claude-x", "2026-07-01T11:00:00Z", 50, 5),
        )
        .unwrap();
        let sub = proj.join("s1").join("subagents");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("agent-a.jsonl"), asst("msg_sub", "claude-x", "2026-07-01T10:30:00Z", 30, 3)).unwrap();

        let day = base.join("sessions").join("2026").join("07").join("01");
        fs::create_dir_all(&day).unwrap();
        let rollout = [
            json!({ "timestamp": "2026-07-01T12:00:00Z", "type": "session_meta", "payload": { "id": "s" } }),
            json!({ "timestamp": "2026-07-01T12:00:01Z", "type": "turn_context", "payload": { "model": "gpt-5.5" } }),
            json!({ "timestamp": "2026-07-01T12:00:02Z", "type": "event_msg", "payload": { "type": "token_count",
                "info": { "last_token_usage": { "input_tokens": 900, "cached_input_tokens": 600, "output_tokens": 80, "total_tokens": 980 } } } }),
            json!({ "timestamp": "2026-07-01T12:00:03Z", "type": "event_msg", "payload": { "type": "token_count", "info": null } }),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(day.join("rollout-2026-07-01T12-00-00-abc.jsonl"), rollout).unwrap();

        let config = json!({ "historyDirs": [ base.to_string_lossy() ] });
        let days = build_data(&config, "all");
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
        // claude: msg_1(110) + msg_2(55) + subagent msg_sub(33); duplicate + zero-usage skipped.
        // codex: input 900-600=300, cacheRead 600, output 80 → 980; info-null line skipped.
        assert_eq!(requests, 4);
        assert_eq!(input, 100 + 50 + 30 + 300);
        assert_eq!(output, 10 + 5 + 3 + 80);
        assert_eq!(cache_read, 600);
        assert_eq!(tokens, 110 + 55 + 33 + 980);
        assert_eq!(models.get("claude-x").copied(), Some(198));
        assert_eq!(models.get("gpt-5.5").copied(), Some(980));

        let _ = fs::remove_dir_all(&base);
    }
}
