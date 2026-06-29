#!/usr/bin/env node
'use strict';

/*
 * One-shot release. Bumps the version in ALL THREE files Tauri needs
 *   - package.json
 *   - src-tauri/tauri.conf.json   (tauri-action reads this for the git tag v<version>)
 *   - src-tauri/Cargo.toml        ([package] version)
 * then commits `v<version>` and pushes main — which triggers the CI pipeline
 * (cargo test -> build 4 platforms -> sign + notarize -> publish Release + latest.json -> homebrew).
 *
 * Usage:
 *   npm run release -- 1.1.1            # bump, commit, push (ships it)
 *   npm run release -- 1.1.1 --no-push  # bump + commit only, you push when ready
 */

const fs = require('fs');
const { execSync } = require('child_process');

const version = process.argv[2];
const noPush = process.argv.includes('--no-push');

if (!/^\d+\.\d+\.\d+$/.test(version || '')) {
  console.error('Usage: npm run release -- <x.y.z> [--no-push]   e.g. npm run release -- 1.1.1');
  process.exit(1);
}

const sh = (c) => execSync(c, { encoding: 'utf8' }).trim();

// Guards: only release from a clean main, so stray edits never ride along into a release.
if (sh('git rev-parse --abbrev-ref HEAD') !== 'main') {
  console.error('✗ releases must run from the main branch');
  process.exit(1);
}
if (sh('git status --porcelain')) {
  console.error('✗ working tree not clean — commit or stash first');
  process.exit(1);
}

const current = JSON.parse(fs.readFileSync('package.json', 'utf8')).version;
console.log(`bumping ${current} → ${version}`);

// Each file: replace its version token; bail loudly if the pattern didn't match (format drifted).
const bump = (path, re) => {
  const before = fs.readFileSync(path, 'utf8');
  const after = before.replace(re, (_, a, b) => `${a}${version}${b}`);
  if (after === before) {
    console.error(`✗ no version field matched in ${path} — not bumped`);
    process.exit(1);
  }
  fs.writeFileSync(path, after);
};
bump('package.json', /("version":\s*")[\d.]+(")/);
bump('src-tauri/tauri.conf.json', /("version":\s*")[\d.]+(")/);
bump('src-tauri/Cargo.toml', /(name = "app"\r?\nversion = ")[\d.]+(")/);

// Verify all three landed on the SAME version before committing.
const got = {
  'package.json': JSON.parse(fs.readFileSync('package.json', 'utf8')).version,
  'tauri.conf.json': JSON.parse(fs.readFileSync('src-tauri/tauri.conf.json', 'utf8')).version,
  'Cargo.toml': (fs.readFileSync('src-tauri/Cargo.toml', 'utf8').match(/name = "app"\r?\nversion = "([\d.]+)"/) || [])[1],
};
for (const [f, v] of Object.entries(got)) {
  if (v !== version) {
    console.error(`✗ ${f} = ${v}, expected ${version}`);
    process.exit(1);
  }
}
console.log(`✓ bumped to ${version}  (package.json · tauri.conf.json · Cargo.toml)`);

execSync('git add package.json src-tauri/tauri.conf.json src-tauri/Cargo.toml');
execSync(`git commit -m "v${version}"`, { stdio: 'inherit' });

if (noPush) {
  console.log(`\nCommitted but not pushed (--no-push). Release with:  git push origin main`);
} else {
  execSync('git push origin main', { stdio: 'inherit' });
  console.log(`\n✓ pushed — CI is building + signing + publishing v${version}.`);
  console.log(`  Watch:  gh run watch "$(gh run list --workflow=release.yml -L1 --json databaseId --jq '.[0].databaseId')"`);
}
