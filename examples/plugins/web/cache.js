"use strict";
// In-memory only: continuation reads the same extracted document, without
// putting browsing history on disk.
function createStore({ttl = 120000, limit = 8, now = Date.now} = {}) {
  const pages = new Map();
  return {
    get(key) {
      const entry = pages.get(key);
      if (!entry) return null;
      if (now() - entry.time > ttl) {
        pages.delete(key);
        return null;
      }
      // Sliding expiry: a long document stays readable while it is still being
      // read, instead of dying two minutes after the first chunk.
      entry.time = now();
      return entry.page;
    },
    set(keys, page) {
      for (const [key, entry] of pages) if (now() - entry.time > ttl) pages.delete(key);
      for (const key of keys) pages.delete(key);
      const time = now();
      for (const key of keys) pages.set(key, {page, time});
      // Aliases share one snapshot, so the limit counts address names.
      while (pages.size > limit) pages.delete(pages.keys().next().value);
    },
    get size() {
      return pages.size;
    }
  };
}

module.exports = {createStore};
