'use strict';

/**
 * In-memory ring buffer of the gateway's most recent HTTP exchanges, so the monitor view
 * can open any forwarded request and inspect its full request/response headers + bodies.
 *
 * Live debugging tool, not history — kept in memory only (cleared on quit / 清空), bounded
 * to `max` entries. Bodies are already capped + auth headers redacted by the proxy before
 * they reach here, so this store just holds and indexes them by id.
 */

function createMonitorStore(opts) {
  const max = (opts && opts.max) || 100;
  const order = []; // ids, oldest first
  const byId = new Map();

  function record(ex) {
    if (!ex || ex.id == null) return;
    if (!byId.has(ex.id)) order.push(ex.id);
    byId.set(ex.id, ex);
    while (order.length > max) {
      const old = order.shift();
      byId.delete(old);
    }
  }

  function get(id) {
    if (id == null) return null;
    return byId.get(id) || byId.get(Number(id)) || byId.get(String(id)) || null;
  }

  function clear() {
    order.length = 0;
    byId.clear();
  }

  return { record, get, clear, size: () => order.length };
}

module.exports = { createMonitorStore };
