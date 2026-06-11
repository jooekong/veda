// k6 load test for the db vectors data plane. One endpoint per run (env OP),
// ramping VUs to find the knee where throughput stalls / p99 turns up.
//
// Required env: BASE, WK   (source them from .env.loadtest)
// Optional env:
//   OP=search|query|upsert|delete   which endpoint (default search)
//   MODE=hybrid|semantic|fulltext   search mode (default hybrid)
//   UNIQUE_TEXT=1|0                  1 = every query/text unique → real embedding
//                                    pressure (exposes batch_size=10 / rate limits);
//                                    0 = fixed pool → cache hit, isolates Milvus.
//   MAX_VUS=200                      top of the ramp
//   SEED_TOTAL=20000  SEED_PREFIX=seed   id space to read from (query/delete)
//   UPSERT_BATCH=50                  records per upsert request
//   WRITE_MODE=insert                upsert body write_mode (omit = server default
//                                    dedup upsert). insert needs fresh unique ids.
//   BASELINE=100                     run 1 VU × N iterations instead of the ramp
//                                    (unloaded single-stream latency baseline)
//   OP=mixed VUS=50 DURATION=30m     steady read-heavy mix for soak runs
//                                    (cache-hit only, no delete; see MIX below)
//
// Example:
//   k6 run -e OP=search -e MODE=hybrid -e BASE=$BASE -e WK=$WK k6_vectors.js
//
import http from 'k6/http';
import { check } from 'k6';

const BASE = (__ENV.BASE || '').replace(/\/$/, '');
const WK = __ENV.WK || '';
const OP = __ENV.OP || 'search';
const MODE = __ENV.MODE || 'hybrid';
const UNIQUE = (__ENV.UNIQUE_TEXT || '1') === '1';
const MAX_VUS = Number(__ENV.MAX_VUS || 200);
const SEED_TOTAL = Number(__ENV.SEED_TOTAL || 20000);
const SEED_PREFIX = __ENV.SEED_PREFIX || 'seed';
const UPSERT_BATCH = Number(__ENV.UPSERT_BATCH || 50);
const STEP_SECS = Number(__ENV.STEP_SECS || 30); // seconds per ramp step (lower = quick smoke)
const WRITE_MODE = __ENV.WRITE_MODE || '';
const BASELINE = Number(__ENV.BASELINE || 0);
const VUS = Number(__ENV.VUS || 50);
const DURATION = __ENV.DURATION || '30m';

if (!BASE || !WK) throw new Error('BASE and WK env are required (source .env.loadtest)');

// Fixed query pool for the cache-hit / isolation mode.
const POOL = [
  '新鲜水果 健康早餐', '高蛋白 减脂晚餐', '进口零食 下午茶', '冷链生鲜 当天配送',
  '宝宝辅食 营养均衡', '宴客硬菜 海鲜大餐', '低卡轻食 沙拉', '家庭囤货 划算装',
  '手工烘焙 吐司面包', '有机蔬菜 农场直供',
];

function rampStages(maxVus) {
  // 6 steps from 5% → 100%, 30s each, then drain. Watch the live panel:
  // the knee is where RPS flattens while p95/p99 climbs.
  const pcts = [0.05, 0.12, 0.25, 0.5, 0.75, 1.0];
  const stages = pcts.map((p) => ({ duration: `${STEP_SECS}s`, target: Math.max(1, Math.round(maxVus * p)) }));
  stages.push({ duration: `${STEP_SECS}s`, target: 0 });
  return stages;
}

function scenario() {
  if (BASELINE > 0) {
    // Unloaded single-stream latency: serial iterations, no concurrency.
    return { executor: 'per-vu-iterations', vus: 1, iterations: BASELINE, maxDuration: '30m' };
  }
  if (OP === 'mixed') {
    // Steady-state soak at fixed VUs rather than a knee-hunting ramp.
    return { executor: 'constant-vus', vus: VUS, duration: DURATION, gracefulStop: '30s' };
  }
  return { executor: 'ramping-vus', startVUs: 0, stages: rampStages(MAX_VUS), gracefulRampDown: '10s' };
}

export const options = {
  scenarios: {
    [OP]: { ...scenario(), tags: { op: OP } },
  },
  // Thresholds are reference markers (no abort): they color the summary so the
  // pass/fail line is obvious at a glance.
  thresholds: {
    http_req_failed: ['rate<0.01'],
    http_req_duration: ['p(95)<1000', 'p(99)<2000'],
  },
  summaryTrendStats: ['avg', 'min', 'med', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

const headers = { 'Content-Type': 'application/json', Authorization: `Bearer ${WK}` };

function randSeedId() {
  return `${SEED_PREFIX}-${Math.floor(Math.random() * SEED_TOTAL)}`;
}

// Read-heavy steady-state profile for OP=mixed soak runs. Cache-hit only
// (fixed POOL texts): a long unique-text run would burn the shared airouter
// quota against live tenants. delete is excluded so the seed corpus survives
// the whole run.
const MIX = [
  ['query', 0.45],
  ['search-fulltext', 0.25],
  ['search-hybrid', 0.2],
  ['upsert', 0.1],
];

function pickMixedOp() {
  let r = Math.random();
  for (const [op, w] of MIX) {
    r -= w;
    if (r <= 0) return op;
  }
  return 'query';
}

function buildRequest() {
  const op = OP === 'mixed' ? pickMixedOp() : OP;
  const tag = `${__VU}-${__ITER}`;
  switch (op) {
    case 'search':
    case 'search-fulltext':
    case 'search-hybrid': {
      const mode = op === 'search' ? MODE : op.slice('search-'.length);
      const unique = op === 'search' && UNIQUE; // mixed soak stays on cache hits
      const q = unique ? `${POOL[__ITER % POOL.length]} ${tag}` : POOL[__ITER % POOL.length];
      return { op, path: '/v1/vectors/search', body: { query: q, mode, top_k: 10 } };
    }
    case 'query': {
      const ids = Array.from({ length: 10 }, randSeedId);
      return { op, path: '/v1/vectors/query', body: { ids } };
    }
    case 'delete': {
      // NOTE: deletes consume seed ids. Re-seed after a delete run, or point
      // SEED_PREFIX at a throwaway id segment.
      return { op, path: '/v1/vectors/delete', body: { ids: [randSeedId()] } };
    }
    case 'upsert': {
      const unique = OP !== 'mixed' && UNIQUE;
      const records = Array.from({ length: UPSERT_BATCH }, (_, i) => ({
        id: `load-${tag}-${i}`,
        text: unique ? `${POOL[i % POOL.length]} ${tag}-${i}` : POOL[i % POOL.length],
        meta: { idx: i },
      }));
      const body = WRITE_MODE ? { write_mode: WRITE_MODE, records } : { records };
      return { op, path: '/v1/vectors/upsert', body };
    }
    default:
      throw new Error(`unknown OP: ${op}`);
  }
}

export default function () {
  const { op, path, body } = buildRequest();
  const res = http.post(BASE + path, JSON.stringify(body), { headers, tags: { op } });
  check(res, {
    'status 200': (r) => r.status === 200,
    'success true': (r) => {
      try {
        return r.json('success') === true;
      } catch {
        return false;
      }
    },
  });
}
