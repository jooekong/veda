#!/usr/bin/env node
// Provision an isolated db workspace + readwrite wk_ key for load testing.
//
// Flow (control plane, vk_):
//   1. POST /v1/accounts/anonymous   -> vk_ account key (its bundled fs ws is ignored)
//   2. POST /v1/workspaces {kind:db} -> db workspace + auto-bootstrapped default dataset
//   3. POST /v1/workspaces/{id}/keys -> readwrite wk_ data-plane key
//
// Writes scripts/loadtest/.env.loadtest (BASE / VK / WS / WK) for the other scripts.
//
// Usage:
//   node provision.mjs                         # anonymous account, BASE=http://127.0.0.1:3000
//   node provision.mjs --base http://host:port --vk vk_existing   # reuse an account key

import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const HERE = dirname(fileURLToPath(import.meta.url));

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    if (argv[i].startsWith('--')) out[argv[i].slice(2)] = argv[i + 1], i++;
  }
  return out;
}

// POST JSON, unwrap the ApiResponse<T> envelope, throw on transport or API error.
async function api(base, path, token, body) {
  const res = await fetch(base + path, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      ...(token ? { authorization: `Bearer ${token}` } : {}),
    },
    body: JSON.stringify(body),
  });
  const text = await res.text();
  let json;
  try {
    json = JSON.parse(text);
  } catch {
    throw new Error(`${path} -> ${res.status}: non-JSON body: ${text.slice(0, 200)}`);
  }
  if (!res.ok || json.success === false) {
    throw new Error(`${path} -> ${res.status} ${json.error_code || ''}: ${json.error || text}`);
  }
  return json.data;
}

const args = parseArgs(process.argv.slice(2));
const BASE = (args.base || process.env.VEDA_URL || 'http://127.0.0.1:3000').replace(/\/$/, '');

async function main() {
  // 1. Account key (vk_). Reuse if supplied, else mint an anonymous one.
  let vk = args.vk;
  if (!vk) {
    const acct = await api(BASE, '/v1/accounts/anonymous', null, {});
    vk = acct.api_key;
    console.log(`account: ${acct.account_id} (anonymous)`);
  }

  // 2. db workspace — server bootstraps the `default` dataset + Milvus collection.
  const ws = await api(BASE, '/v1/workspaces', vk, { name: 'loadtest', kind: 'db' });
  console.log(`workspace: ${ws.id} (kind=${ws.kind})`);

  // 3. readwrite wk_ data-plane key (plaintext returned once).
  const key = await api(BASE, `/v1/workspaces/${ws.id}/keys`, vk, {
    name: 'loadtest',
    permission: 'readwrite',
  });
  console.log(`wk_ key: ${key.key} (${key.permission})`);

  const envPath = join(HERE, '.env.loadtest');
  const lines = [
    `BASE=${BASE}`,
    `VK=${vk}`,
    `WS=${ws.id}`,
    `WK=${key.key}`,
    '',
  ].join('\n');
  writeFileSync(envPath, lines);
  console.log(`\nwrote ${envPath}`);
}

main().catch((e) => {
  console.error('provision failed:', e.message);
  process.exit(1);
});
