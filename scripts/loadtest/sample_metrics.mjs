#!/usr/bin/env node
// Poll /v1/metrics during a load run and attribute end-to-end latency to layers
// by subtracting the nested histograms (window deltas, not cumulative averages):
//
//   embedding+framework = vector_request_seconds − vector_store_op_seconds
//   store (non-Milvus)   = vector_store_op_seconds − milvus_request_seconds
//   Milvus               = milvus_request_seconds
//
// Plus MySQL pool usage (each vectors call does one resolve_dataset round-trip).
// These are per-window mean latencies — they show WHERE time goes by layer;
// read p99 from the k6 summary.
//
// Reads scripts/loadtest/.env.loadtest (BASE). Needs the metrics scrape token
// (the VEDA_METRICS_TOKEN the server was started with):
//   METRICS_TOKEN=... node sample_metrics.mjs --op search [--interval 2]

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const HERE = dirname(fileURLToPath(import.meta.url));

function loadEnv() {
  const env = {};
  for (const line of readFileSync(join(HERE, '.env.loadtest'), 'utf8').split('\n')) {
    const m = line.match(/^(\w+)=(.*)$/);
    if (m) env[m[1]] = m[2];
  }
  return env;
}

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    if (argv[i].startsWith('--')) out[argv[i].slice(2)] = argv[i + 1], i++;
  }
  return out;
}

// Parse Prometheus text exposition into {name, labels, value} rows.
function parseMetrics(text) {
  const out = [];
  for (const line of text.split('\n')) {
    if (!line || line[0] === '#') continue;
    const m = line.match(/^([a-zA-Z_:][\w:]*)(\{[^}]*\})?\s+([0-9.eE+-]+|NaN|[+-]?Inf)$/);
    if (!m) continue;
    const labels = {};
    if (m[2]) {
      for (const kv of m[2].slice(1, -1).split(',')) {
        const mm = kv.match(/^(\w+)="(.*)"$/);
        if (mm) labels[mm[1]] = mm[2];
      }
    }
    out.push({ name: m[1], labels, value: Number(m[3]) });
  }
  return out;
}

const sumWhere = (rows, name, pred) =>
  rows.filter((r) => r.name === name && (!pred || pred(r.labels))).reduce((a, r) => a + r.value, 0);
const lastGauge = (rows, name) => {
  const r = rows.filter((x) => x.name === name);
  return r.length ? r[r.length - 1].value : NaN;
};

const env = loadEnv();
const args = parseArgs(process.argv.slice(2));
const BASE = env.BASE.replace(/\/$/, '');
const OP = args.op || null; // filter request/store layers by operation label
const INTERVAL = Number(args.interval || 2);
const TOKEN = process.env.METRICS_TOKEN || process.env.VEDA_METRICS_TOKEN || '';

if (!TOKEN) {
  console.error('need METRICS_TOKEN (the VEDA_METRICS_TOKEN the server runs with)');
  process.exit(1);
}

const opFilter = OP ? (l) => l.operation === OP : null;

async function snapshot() {
  const res = await fetch(`${BASE}/v1/metrics`, { headers: { authorization: `Bearer ${TOKEN}` } });
  if (res.status === 404) throw new Error('metrics 404 — server started without VEDA_METRICS_TOKEN, or wrong token');
  if (!res.ok) throw new Error(`metrics -> ${res.status}`);
  const rows = parseMetrics(await res.text());
  return {
    reqSum: sumWhere(rows, 'veda_vector_request_seconds_sum', opFilter),
    reqCnt: sumWhere(rows, 'veda_vector_request_seconds_count', opFilter),
    stoSum: sumWhere(rows, 'veda_vector_store_op_seconds_sum', opFilter),
    stoCnt: sumWhere(rows, 'veda_vector_store_op_seconds_count', opFilter),
    mvSum: sumWhere(rows, 'veda_milvus_request_seconds_sum'),
    mvCnt: sumWhere(rows, 'veda_milvus_request_seconds_count'),
    poolSize: lastGauge(rows, 'veda_mysql_pool_connections'),
    poolIdle: lastGauge(rows, 'veda_mysql_pool_idle'),
  };
}

const pad = (s, n) => String(s).padStart(n);
const ms = (dSum, dCnt) => (dCnt > 0 ? ((dSum / dCnt) * 1000).toFixed(1) : '—');

console.log(`sampling /v1/metrics every ${INTERVAL}s${OP ? ` (op=${OP})` : ''}; Ctrl-C to stop`);
console.log(
  [pad('time', 8), pad('qps', 7), pad('e2e_ms', 8), pad('embed+fw', 9),
   pad('store-mv', 9), pad('milvus', 8), pad('pool_use', 9)].join(' ')
);

let prev = null;
async function tick() {
  try {
    const cur = await snapshot();
    if (prev) {
      const dReq = cur.reqCnt - prev.reqCnt;
      const e2e = (cur.reqSum - prev.reqSum) / Math.max(dReq, 1) * 1000;
      const sto = (cur.stoSum - prev.stoSum) / Math.max(cur.stoCnt - prev.stoCnt, 1) * 1000;
      const mv = (cur.mvSum - prev.mvSum) / Math.max(cur.mvCnt - prev.mvCnt, 1) * 1000;
      const t = new Date().toTimeString().slice(0, 8);
      const inUse = Number.isNaN(cur.poolSize) ? '—' : `${cur.poolSize - cur.poolIdle}/${cur.poolSize}`;
      console.log(
        [pad(t, 8), pad((dReq / INTERVAL).toFixed(0), 7),
         pad(dReq > 0 ? e2e.toFixed(1) : '—', 8),
         pad(dReq > 0 ? Math.max(e2e - sto, 0).toFixed(1) : '—', 9),
         pad(dReq > 0 ? Math.max(sto - mv, 0).toFixed(1) : '—', 9),
         pad(dReq > 0 ? mv.toFixed(1) : '—', 8),
         pad(inUse, 9)].join(' ')
      );
    }
    prev = cur;
  } catch (e) {
    console.error('sample error:', e.message);
  }
}

setInterval(tick, INTERVAL * 1000);
tick();
