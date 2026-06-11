#!/usr/bin/env node
// Probe the embedding upstream's rate-limit dimension (RPM vs TPM/text-count)
// by walking a fixed request-rate ladder at two batch shapes and watching
// where 429s start:
//
//   node embedding_ratelimit_probe.mjs --batch 1
//   node embedding_ratelimit_probe.mjs --batch 10
//
//   429 onset at the same requests/min for both  ⇒ limit counts REQUESTS (RPM)
//   batch=10 onset ~10× earlier (in requests)    ⇒ limit counts texts/tokens (TPM)
//
// Optional third run with --textlen 10 (same batch, ~10× tokens per text)
// separates token-count from text-count if the second run says "not RPM".
//
// The quota is shared with live tenants, so the probe is deliberately gentle:
// a step aborts at --max-429 (default 5) and the whole ladder stops at the
// first step that shows onset. Absolute onset numbers include whatever
// baseline traffic other tenants are generating — the *dimension* conclusion
// (same vs ~10× earlier) is the robust part; treat onset RPM as headroom.
//
// Env (no secrets on the command line):
//   AIROUTER_API_URL   e.g. https://airouter.ddmc-inc.com/api/v1/embeddings
//   AIROUTER_KEY       bearer token
//   AIROUTER_MODEL     default text-embedding-v4
//   AIROUTER_DIMS      optional dimensions field (e.g. 1024)
// Flags:
//   --batch N          texts per request (default 1; upstream cap is 10)
//   --rpms a,b,c       request-rate ladder (default 30,60,120,240,480)
//   --step-secs N      seconds per step (default 60)
//   --textlen N        repeat the base text N times (default 1)
//   --max-429 N        abort step + ladder after N 429s in a step (default 5)
//
// Node 16 compatible (boxes ship node 16): uses https.request, not fetch.
import https from 'node:https';
import http from 'node:http';
import { URL } from 'node:url';

const API_URL = process.env.AIROUTER_API_URL || '';
const KEY = process.env.AIROUTER_KEY || '';
const MODEL = process.env.AIROUTER_MODEL || 'text-embedding-v4';
const DIMS = process.env.AIROUTER_DIMS ? Number(process.env.AIROUTER_DIMS) : undefined;
if (!API_URL || !KEY) {
  console.error('AIROUTER_API_URL and AIROUTER_KEY env are required');
  process.exit(1);
}

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    if (argv[i].startsWith('--')) out[argv[i].slice(2)] = argv[i + 1], i++;
  }
  return out;
}
const args = parseArgs(process.argv.slice(2));
const BATCH = Number(args.batch || 1);
const RPMS = (args.rpms || '30,60,120,240,480').split(',').map(Number);
const STEP_SECS = Number(args['step-secs'] || 60);
const TEXTLEN = Number(args.textlen || 1);
const MAX_429 = Number(args['max-429'] || 5);

const url = new URL(API_URL);
const transport = url.protocol === 'http:' ? http : https;
const agent = new transport.Agent({ keepAlive: true, maxSockets: 64 });

let seq = 0;
function makeTexts() {
  // Unique per request so no cache layer anywhere can absorb the load.
  const stamp = `${Date.now()}-${process.pid}`;
  return Array.from({ length: BATCH }, () => `压测限流探针 ${stamp}-${seq++} ` + '生鲜食品配送'.repeat(TEXTLEN));
}

function postOnce() {
  const body = JSON.stringify({ model: MODEL, input: makeTexts(), ...(DIMS ? { dimensions: DIMS } : {}) });
  const t0 = Date.now();
  return new Promise((resolve) => {
    const req = transport.request(
      url,
      {
        method: 'POST',
        agent,
        headers: {
          'content-type': 'application/json',
          authorization: `Bearer ${KEY}`,
          'content-length': Buffer.byteLength(body),
        },
        timeout: 30000,
      },
      (res) => {
        let buf = '';
        res.on('data', (c) => (buf += c));
        res.on('end', () =>
          resolve({
            status: res.statusCode,
            ms: Date.now() - t0,
            retryAfter: res.headers['retry-after'] || null,
            // 429/4xx bodies often name the quota — keep a sample.
            snippet: res.statusCode !== 200 ? buf.slice(0, 160) : '',
          }),
        );
      },
    );
    req.on('timeout', () => req.destroy(new Error('timeout')));
    req.on('error', (e) => resolve({ status: 0, ms: Date.now() - t0, retryAfter: null, snippet: e.message.slice(0, 120) }));
    req.end(body);
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const pct = (arr, p) => (arr.length ? arr.slice().sort((a, b) => a - b)[Math.floor((arr.length - 1) * p)] : 0);

async function runStep(rpm) {
  const interval = 60000 / rpm;
  const results = [];
  const inflight = new Set();
  let aborted = false;
  const t0 = Date.now();
  while (Date.now() - t0 < STEP_SECS * 1000 && !aborted) {
    const p = postOnce().then((r) => {
      results.push(r);
      inflight.delete(p);
      if (results.filter((x) => x.status === 429).length >= MAX_429) aborted = true;
    });
    inflight.add(p);
    await sleep(interval);
  }
  await Promise.all([...inflight]);
  const ok = results.filter((r) => r.status === 200);
  const r429 = results.filter((r) => r.status === 429);
  const other = results.filter((r) => r.status !== 200 && r.status !== 429);
  const row = {
    rpm,
    sent: results.length,
    ok: ok.length,
    '429': r429.length,
    other: other.length,
    p50ms: pct(ok.map((r) => r.ms), 0.5),
    p95ms: pct(ok.map((r) => r.ms), 0.95),
    retryAfter: [...new Set(r429.map((r) => r.retryAfter).filter(Boolean))].join(',') || '-',
  };
  console.log(JSON.stringify(row));
  const sample = r429[0] || other[0];
  if (sample && sample.snippet) console.log(`  ↳ sample non-200 body: ${sample.snippet}`);
  return { aborted, hit429: r429.length };
}

async function main() {
  console.log(`# probe batch=${BATCH} textlen=${TEXTLEN} step=${STEP_SECS}s model=${MODEL}`);
  console.log(`# effective texts/min at each step = rpm × ${BATCH}`);
  for (const rpm of RPMS) {
    const { aborted, hit429 } = await runStep(rpm);
    if (aborted || hit429 > 0) {
      console.log(`# onset at ~${rpm} req/min (${rpm * BATCH} texts/min) — stopping ladder to spare shared quota`);
      return;
    }
  }
  console.log('# no 429 across the ladder — raise --rpms if you need the ceiling');
}

main();
