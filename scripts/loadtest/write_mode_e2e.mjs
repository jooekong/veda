#!/usr/bin/env node
// e2e for write_mode against a LIVE server. Provisions a fresh db workspace +
// wk_, then verifies write_mode routing end to end plus the insert-vs-upsert
// latency gap (proves the fast path actually skips Milvus dedup).
//
// Usage: node write_mode_e2e.mjs --base http://10.79.51.161:3000

const args = {};
for (let i = 2; i < process.argv.length; i++)
  if (process.argv[i].startsWith('--')) args[process.argv[i].slice(2)] = process.argv[++i];
const BASE = (args.base || 'http://10.79.51.161:3000').replace(/\/$/, '');

let WK, VK, WS;

async function raw(method, path, token, body) {
  const res = await fetch(BASE + path, {
    method,
    headers: { 'content-type': 'application/json', ...(token ? { authorization: `Bearer ${token}` } : {}) },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  let json;
  try { json = JSON.parse(text); } catch { json = { _raw: text }; }
  return { status: res.status, json };
}
const wk = (method, path, body) => raw(method, path, WK, body);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let pass = 0, fail = 0;
function check(name, cond, extra = '') {
  if (cond) { console.log(`  ✅ ${name}`); pass++; }
  else { console.log(`  ❌ ${name}  ${extra}`); fail++; }
}

async function provision() {
  const acct = (await raw('POST', '/v1/accounts/anonymous', null, {})).json.data;
  VK = acct.api_key;
  const ws = (await raw('POST', '/v1/workspaces', VK, { name: 'wm-e2e', kind: 'db' })).json.data;
  WS = ws.id;
  const key = (await raw('POST', `/v1/workspaces/${WS}/keys`, VK, { name: 'e2e', permission: 'readwrite' })).json.data;
  WK = key.key;
  console.log(`provisioned ws=${WS} wk=${WK.slice(0, 12)}...\n`);
}

async function main() {
  await provision();

  // 1. write_mode=insert + unique id → queryable then deletable.
  await wk('POST', '/v1/vectors/upsert', { write_mode: 'insert', records: [{ id: 'e2e-ins', text: 'inserted' }] });
  await sleep(1500);
  let r = await wk('POST', '/v1/vectors/query', { ids: ['e2e-ins'] });
  check('insert unique id → 1 row, text matches',
    r.json.data?.hits?.length === 1 && r.json.data.hits[0].text === 'inserted',
    JSON.stringify(r.json.data?.hits));
  await wk('POST', '/v1/vectors/delete', { ids: ['e2e-ins'] });
  await sleep(1500);
  r = await wk('POST', '/v1/vectors/query', { ids: ['e2e-ins'] });
  check('delete → 0 rows', r.json.data?.hits?.length === 0);

  // 2. default upsert + id-less → insert fast path; UUID surfaced + queryable.
  r = await wk('POST', '/v1/vectors/upsert', { records: [{ text: 'no-id' }] });
  const genId = r.json.data?.ids?.[0];
  check('id-less → surfaces a UUID', !!genId);
  await sleep(1500);
  r = await wk('POST', '/v1/vectors/query', { ids: [genId] });
  check('id-less → queryable by UUID', r.json.data?.hits?.length === 1);

  // 3. default upsert + explicit id replay → idempotent (1 row, latest wins).
  await wk('POST', '/v1/vectors/upsert', { records: [{ id: 'e2e-idem', text: 'v1' }] });
  await sleep(1500);
  await wk('POST', '/v1/vectors/upsert', { records: [{ id: 'e2e-idem', text: 'v2' }] });
  await sleep(1500);
  r = await wk('POST', '/v1/vectors/query', { ids: ['e2e-idem'] });
  check('upsert replay → 1 row, latest wins',
    r.json.data?.hits?.length === 1 && r.json.data.hits[0].text === 'v2',
    JSON.stringify(r.json.data?.hits));

  // 4. mixed batch (explicit + id-less) → both land.
  r = await wk('POST', '/v1/vectors/upsert', { records: [{ id: 'e2e-mix', text: 'explicit' }, { text: 'mixed-idless' }] });
  const mixIds = r.json.data?.ids || [];
  check('mixed batch → 2 ids', mixIds.length === 2);
  await sleep(1500);
  const a = await wk('POST', '/v1/vectors/query', { ids: ['e2e-mix'] });
  check('mixed: explicit-id half landed', a.json.data?.hits?.length === 1);
  const b = await wk('POST', '/v1/vectors/query', { ids: [mixIds[1]] });
  check('mixed: id-less half landed', b.json.data?.hits?.length === 1);

  // 5. perf: insert vs default-upsert over N unique ids (same text → embedding
  //    cache hit, so the gap isolates Milvus's dedup+delete ~400ms cost).
  const N = 20;
  await wk('POST', '/v1/vectors/upsert', { write_mode: 'insert', records: [{ id: 'warm', text: 'perf' }] }); // warm embed cache
  const ins = [], ups = [];
  for (let i = 0; i < N; i++) {
    let t = Date.now();
    await wk('POST', '/v1/vectors/upsert', { write_mode: 'insert', records: [{ id: `perf-ins-${i}`, text: 'perf' }] });
    ins.push(Date.now() - t);
    t = Date.now();
    await wk('POST', '/v1/vectors/upsert', { records: [{ id: `perf-ups-${i}`, text: 'perf' }] }); // default upsert
    ups.push(Date.now() - t);
  }
  const avg = (x) => x.reduce((p, c) => p + c, 0) / x.length;
  const insAvg = avg(ins), upsAvg = avg(ups);
  console.log(`\n  perf (N=${N}, embed cache hit): insert avg ${insAvg.toFixed(0)}ms  vs  upsert avg ${upsAvg.toFixed(0)}ms  (${(upsAvg / insAvg).toFixed(1)}x)`);
  check('insert fast path is faster than upsert', insAvg < upsAvg);

  console.log(`\n${fail === 0 ? '✅ ALL PASS' : '❌ FAIL'}: ${pass} passed, ${fail} failed`);
  process.exit(fail === 0 ? 0 : 1);
}

main().catch((e) => { console.error('e2e error:', e.message); process.exit(1); });
