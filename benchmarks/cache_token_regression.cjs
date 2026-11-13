#!/usr/bin/env node
// Credential-free, offline regression lane for the prompt/context/cache contract.
//
// It runs only local Rust unit tests: no network, no provider credentials, no
// paid requests, no user configuration and no real session files. It is a
// verification gate, not a benchmark: it establishes that the invariants the
// cache design depends on still hold, and it never claims a token or cost saving.
//
//   node benchmarks/cache_token_regression.cjs           # run the lane
//   node benchmarks/cache_token_regression.cjs --list    # print the groups
'use strict';

const { spawnSync } = require('node:child_process');

const GROUPS = [
  {
    name: 'admitted-reference-prefix',
    filter: 'plugins::model::tests',
    why: 'Every later request must start with the exact bytes of the previous one.',
  },
  {
    name: 'final-projection-trace',
    filter: 'cache_trace::tests',
    why: 'Content-free diagnostics must describe the request that was sent.',
  },
  {
    name: 'cache-usage-accounting',
    filter: 'usage::tests',
    why: 'Cache read/write stay subsets of input; unknown never becomes zero.',
  },
  {
    name: 'anthropic-usage-and-cache',
    filter: 'provider::anthropic::tests',
    why: 'Cache creation/TTL breakdown and request payload caching controls.',
  },
  {
    name: 'checkpoint-prefix-parity',
    filter: 'provider::tests::checkpoint',
    why: 'Checkpoint payloads reuse the coding prefix and never re-run native search.',
  },
  {
    name: 'protocol-payload-regressions',
    filter: 'provider::tests',
    why: 'Wire payload, retry and native-tool behaviour.',
  },
  {
    name: 'compaction-defaults',
    filter: 'context::tests',
    why: 'Lazy checkpoint policy stays capacity-conservative at its documented trigger.',
  },
  {
    name: 'token-estimator-boundaries',
    filter: 'token_estimate::tests',
    why: 'Multilingual, emoji and dense payload classification stays conservative.',
  },
  {
    name: 'large-tool-output-projection',
    filter: 'tool_archive::tests',
    why: 'Large MCP/plugin results are archived once, previewed and retrievable.',
  },
];

if (process.argv.includes('--list')) {
  for (const group of GROUPS) {
    console.log(`${group.name}\t${group.filter}\t${group.why}`);
  }
  process.exit(0);
}

const results = [];
for (const group of GROUPS) {
  process.stdout.write(`\n=== ${group.name} (${group.filter})\n`);
  const run = spawnSync(
    'cargo',
    ['test', '--offline', '--lib', group.filter],
    { stdio: 'inherit', shell: process.platform === 'win32' },
  );
  results.push({ group, status: run.status === 0 ? 'passed' : 'failed' });
}

const failed = results.filter((entry) => entry.status !== 'passed');
console.log('\nOffline cache/context regression summary');
for (const entry of results) {
  console.log(`  ${entry.status === 'passed' ? 'ok  ' : 'FAIL'} ${entry.group.name}`);
}
if (failed.length) {
  console.error(
    `\n${failed.length} group(s) failed. This lane reports local invariants only;` +
      ' it is not a provider cache-hit or cost measurement.',
  );
  process.exit(1);
}
console.log(
  '\nAll offline groups passed. No provider was contacted and no token/cost saving is claimed.',
);
