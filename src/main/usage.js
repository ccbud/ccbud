'use strict';

/**
 * Token-usage tracking. The gateway sees every request/response, so we record real
 * token usage (from Anthropic `usage` fields) into per-day buckets and expose aggregated
 * stats over time ranges (1d / 7d / 30d / all) for the menu-bar display and tray panel.
 */

const fs = require('fs');
const path = require('path');

const DAY = 86400000;
const pad = (n) => String(n).padStart(2, '0');
const keyOf = (ts) => { const d = new Date(ts); return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`; };
const startOfDay = (ts) => { const d = new Date(ts); d.setHours(0, 0, 0, 0); return d.getTime(); };
const msOfKey = (k) => { const [y, m, d] = k.split('-').map(Number); return new Date(y, m - 1, d).getTime(); };

function topKey(map) {
  let best = null, bestV = -1;
  for (const k in map) if (map[k] > bestV) { bestV = map[k]; best = k; }
  return best;
}

function createUsageStore(dir) {
  const file = path.join(dir, 'usage.json');
  let data = { days: {} };
  let timer = null;

  try {
    const raw = JSON.parse(fs.readFileSync(file, 'utf8'));
    if (raw && raw.days) data = raw;
  } catch (_) {}

  function scheduleSave() {
    if (timer) return;
    timer = setTimeout(() => {
      timer = null;
      try {
        fs.mkdirSync(dir, { recursive: true });
        const tmp = file + '.tmp';
        fs.writeFileSync(tmp, JSON.stringify(data));
        fs.renameSync(tmp, file);
      } catch (_) {}
    }, 1500);
    if (timer.unref) timer.unref();
  }

  function record(ev) {
    const ts = ev.ts || Date.now();
    const total = (ev.inputTokens || 0) + (ev.outputTokens || 0) + (ev.cacheRead || 0) + (ev.cacheCreation || 0);
    const k = keyOf(ts);
    const day = data.days[k] || (data.days[k] = { tokens: 0, input: 0, output: 0, requests: 0, models: {}, providers: {}, hours: {} });
    day.requests++;
    day.tokens += total;
    day.input += ev.inputTokens || 0;
    day.output += ev.outputTokens || 0;
    const model = ev.requestedModel || ev.outgoingModel || 'unknown';
    day.models[model] = (day.models[model] || 0) + total;
    if (ev.provider) day.providers[ev.provider] = (day.providers[ev.provider] || 0) + total;
    const h = new Date(ts).getHours();
    day.hours[h] = (day.hours[h] || 0) + total;
    scheduleSave();
  }

  function rangeKeys(range, now) {
    const all = Object.keys(data.days).sort();
    if (range === 'all') return all;
    const n = range === '1d' ? 1 : range === '30d' ? 30 : 7;
    const cut = startOfDay((now || Date.now()) - (n - 1) * DAY);
    return all.filter((k) => msOfKey(k) >= cut);
  }

  function rangeTokens(range, now) {
    return rangeKeys(range, now).reduce((s, k) => s + data.days[k].tokens, 0);
  }

  function streaks(now) {
    const active = new Set(Object.keys(data.days).filter((k) => data.days[k].requests > 0));
    // longest run of consecutive calendar days
    let longest = 0, run = 0;
    let prev = null;
    for (const k of [...active].sort()) {
      const t = msOfKey(k);
      run = prev !== null && t - prev === DAY ? run + 1 : 1;
      prev = t;
      if (run > longest) longest = run;
    }
    // current run ending today or yesterday
    let cur = 0;
    let t = startOfDay(now || Date.now());
    if (!active.has(keyOf(t))) t -= DAY; // allow streak to count up to yesterday
    while (active.has(keyOf(t))) { cur++; t -= DAY; }
    return { current: cur, longest };
  }

  function buildHeatmap(weeks, now) {
    const today = startOfDay(now || Date.now());
    const span = weeks * 7;
    let start = today - (span - 1) * DAY;
    start -= new Date(start).getDay() * DAY; // snap to Sunday so row = weekday
    const cells = [];
    let max = 1;
    for (let t = start; t <= today; t += DAY) {
      const d = data.days[keyOf(t)];
      const tok = d ? d.tokens : 0;
      if (tok > max) max = tok;
      cells.push({ date: keyOf(t), tokens: tok });
    }
    for (const c of cells) {
      const r = c.tokens / max;
      c.level = c.tokens === 0 ? 0 : r > 0.66 ? 4 : r > 0.33 ? 3 : r > 0.1 ? 2 : 1;
    }
    return cells;
  }

  function query(range, now) {
    const keys = rangeKeys(range, now);
    let tokens = 0, input = 0, output = 0, requests = 0;
    const models = {}, providers = {}, hours = {};
    for (const k of keys) {
      const d = data.days[k];
      tokens += d.tokens; input += d.input; output += d.output; requests += d.requests;
      for (const m in d.models) models[m] = (models[m] || 0) + d.models[m];
      for (const p in d.providers) providers[p] = (providers[p] || 0) + d.providers[p];
      for (const h in d.hours) hours[h] = (hours[h] || 0) + d.hours[h];
    }
    const activeDays = keys.filter((k) => data.days[k].requests > 0).length;
    const st = streaks(now);
    return {
      range,
      tokens, input, output, requests, activeDays,
      peakHour: hours && Object.keys(hours).length ? Number(topKey(hours)) : null,
      favoriteModel: topKey(models),
      favoriteProvider: topKey(providers),
      byModel: Object.entries(models).sort((a, b) => b[1] - a[1]).map(([model, t]) => ({ model, tokens: t, pct: tokens ? t / tokens : 0 })),
      byProvider: Object.entries(providers).sort((a, b) => b[1] - a[1]).map(([provider, t]) => ({ provider, tokens: t, pct: tokens ? t / tokens : 0 })),
      currentStreak: st.current,
      longestStreak: st.longest,
      heatmap: buildHeatmap(8, now),
    };
  }

  return { record, query, rangeTokens, _data: () => data };
}

/** Format a token count compactly: 950 → "950", 12345 → "12.3K", 1.3e9 → "1.3B". */
function formatTokens(n) {
  n = n || 0;
  if (n < 1000) return String(n);
  if (n < 1e6) return (n / 1e3).toFixed(n < 1e4 ? 1 : 0).replace(/\.0$/, '') + 'K';
  if (n < 1e9) return (n / 1e6).toFixed(n < 1e7 ? 1 : 0).replace(/\.0$/, '') + 'M';
  return (n / 1e9).toFixed(1).replace(/\.0$/, '') + 'B';
}

module.exports = { createUsageStore, formatTokens };
