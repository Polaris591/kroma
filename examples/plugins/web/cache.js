"use strict";
// In-memory only: continuation reads the same extracted document, without
// putting browsing history on disk. A page being paged through is held for a
// long tool between continuations; every other page keeps the shorter idle
// window. Both are bounded lifetimes, not durable task memory.
const SNAPSHOT_IDLE_TTL_MS = 2 * 60 * 1000;
const SNAPSHOT_HOLD_TTL_MS = 30 * 60 * 1000;
function createStore({ttl = SNAPSHOT_IDLE_TTL_MS, holdTtl = SNAPSHOT_HOLD_TTL_MS,
  limit = 8, now = Date.now} = {}) {
  const pages = new Map();
  return {
    get(key) {
      const entry = pages.get(key);
      if (!entry) return null;
      if (now() - entry.time > entry.ttl) {
        pages.delete(key);
        return null;
      }
      // Sliding expiry: a long document stays readable while it is still being
      // read, instead of dying after a short idle window.
      entry.time = now();
      return entry.page;
    },
    set(keys, page) {
      for (const [key, entry] of pages) if (now() - entry.time > entry.ttl) pages.delete(key);
      for (const key of keys) pages.delete(key);
      const time = now();
      for (const key of keys) pages.set(key, {page, time, ttl});
      // Aliases share one snapshot, so the limit counts address names.
      while (pages.size > limit) pages.delete(pages.keys().next().value);
    },
    // Promote a page with an open continuation to the long window, or release
    // it after the final chunk.
    retain(keys, held = true) {
      const time = now();
      for (const key of keys) {
        const entry = pages.get(key);
        if (!entry) continue;
        entry.time = time;
        entry.ttl = held ? holdTtl : ttl;
      }
    },
    get size() {
      return pages.size;
    }
  };
}

module.exports = {createStore, SNAPSHOT_IDLE_TTL_MS, SNAPSHOT_HOLD_TTL_MS};
