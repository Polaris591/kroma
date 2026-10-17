#!/usr/bin/env node
"use strict";

/**
 * T30: the ledger must be checkable, not just readable.
 *
 * Two failures this catches:
 * 1. an acceptance document citing a test that does not exist in the tree (a claim with no evidence);
 * 2. a `commands.json` entry naming a log file that is not there (evidence that vanished).
 *
 * Run: node scripts/verify-ledger.js
 */

const fs = require("node:fs");
const path = require("node:path");

const ROOT = path.join(__dirname, "..");
const RESULTS = path.join(ROOT, "docs/review/phase1_test_results");

function collectSources(dir, out, exts) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) collectSources(full, out, exts);
    else if (exts.some((e) => entry.name.endsWith(e))) {
      out.push(fs.readFileSync(full, "utf8"));
    }
  }
  return out;
}

function main() {
  const sources = [
    ...collectSources(path.join(ROOT, "crates/kroma-core/tests"), [], [".rs"]),
    ...collectSources(path.join(ROOT, "crates/kroma-core/src"), [], [".rs"]),
    ...collectSources(path.join(ROOT, "scripts"), [], [".js"]),
  ].join("\n");

  let problems = 0;
  const acceptance = fs
    .readdirSync(RESULTS)
    .filter((f) => /^T\d\d.*\.md$/.test(f));

  // 1. Every `test_name`-shaped token in an acceptance doc must exist somewhere in the tree.
  const seen = new Set();
  for (const file of acceptance) {
    const text = fs.readFileSync(path.join(RESULTS, file), "utf8");
    const names = text.match(/`[a-z][a-z0-9_]{6,}(?:::)?[a-z0-9_:]*`/g) || [];
    for (const raw of names) {
      const name = raw.replace(/`/g, "");
      if (!/^[a-z][a-z0-9_]*(::[a-z0-9_]+)*$/.test(name)) continue;
      seen.add(name);
      // A line may honestly record a task-card name as *not* implemented by a same-named test; those
      // rows are marked, and an unmarked claim with no test is what this check exists to catch.
      const lines = text.split("\n").filter((line) => line.includes(raw));
      const marked = lines.some((line) =>
        /未实现|未新增|未单独|无同名测试|NOT_RUN|BLOCKED|合并项|partial/i.test(line)
      );
      if (!sources.includes(name) && !marked) {
        // Only flag names that look like tests (test_/a_/the_/ahe_/...) to avoid prose in backticks.
        if (/^(test_|[a-z]+_(and|of|is)|ahe_|mcp_|windows_|cli_|cache_|graph_|prefix_|sandbox_|per_|real_|zero_|early_|path_|summary_|unsupported_|missing_|declared_|duplicate_|oversized_|unknown_|sanitization_|each_|development_|cost_|rebuild_|release_|tampered_|cargo_|dependency_|manual_)/.test(name)) {
          console.error(`missing test cited by ${file}: ${name}`);
          problems += 1;
        }
      }
    }
  }

  // 2. Every commands.json entry with a log must have that log.
  const commands = JSON.parse(fs.readFileSync(path.join(RESULTS, "commands.json"), "utf8"));
  for (const entry of commands.commands) {
    if (!entry.log) continue;
    // Some entries record a prose note ("not archived (inline: ...)") instead of a path; only path-like
    // values are checked, and the note itself is not evidence of anything.
    if (!entry.log.startsWith("logs/")) continue;
    const logPath = path.join(RESULTS, entry.log);
    if (!fs.existsSync(logPath)) {
      console.error(`missing log for command "${entry.id}": ${entry.log}`);
      problems += 1;
      continue;
    }
    const text = fs.readFileSync(logPath, "utf8");
    // The point is that the log records an outcome, not that it uses one particular wording: the T00
    // logs record counts ("patched b08c failures: 1/20"), cargo logs end with "Finished", and the fmt
    // log lists diffs. A missing marker means the file is a truncated or empty capture.
    const markers = [
      /^EXIT=/m,
      /test result:/m,
      /checks passed/m,
      /^\s*Finished\b/m,
      /^\s*ok\b/m,
      /passed/m,
      /failures?:/m,
      /^Diff in /m,
      /no diffs|clean/m,
    ];
    if (!markers.some((re) => re.test(text))) {
      console.error(`log without a result marker for "${entry.id}": ${entry.log}`);
      problems += 1;
    }
  }

  // 3. The ledger must mention every task id T00..T30.
  const ledger = fs.readFileSync(path.join(RESULTS, "T30_LEDGER.md"), "utf8");
  for (let i = 0; i <= 30; i += 1) {
    const id = `T${String(i).padStart(2, "0")}`;
    if (!ledger.includes(`| ${id} |`)) {
      console.error(`the ledger has no row for ${id}`);
      problems += 1;
    }
  }

  if (problems > 0) {
    console.error(`ledger verification FAILED with ${problems} problem(s)`);
    return 1;
  }
  console.log(
    `ledger ok: ${acceptance.length} acceptance documents, ${commands.commands.length} commands, 31 task rows, ${seen.size} distinct cited identifiers`
  );
  return 0;
}

process.exitCode = main();
