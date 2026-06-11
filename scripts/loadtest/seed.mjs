#!/usr/bin/env node
// Seed a db workspace with N vectors via the real /v1/vectors/upsert path.
//
// Text comes from a fixed pool of ~1000 distinct sentences cycled across N ids.
// The server's per-text embedding cache (moka) then serves repeats from memory,
// so embedding cost ≈ pool size, not N — fills the index cheaply while still
// exercising the real upsert -> embed -> Milvus write path end to end.
//
// Reads scripts/loadtest/.env.loadtest (BASE, WK). Env overrides:
//   TOTAL (20000)  BATCH (500)  CONCURRENCY (8)  PREFIX (seed)
//   WRITE_MODE=insert  fast path for the FIRST seed into an empty workspace
//                      only — re-seeds must stay on the default upsert
//                      (insert on existing ids = duplicate PK, undefined
//                      behavior in Milvus)
//
// Usage: node seed.mjs

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const HERE = dirname(fileURLToPath(import.meta.url));

function loadEnv() {
  const txt = readFileSync(join(HERE, '.env.loadtest'), 'utf8');
  const env = {};
  for (const line of txt.split('\n')) {
    const m = line.match(/^(\w+)=(.*)$/);
    if (m) env[m[1]] = m[2];
  }
  return env;
}

// Build 10×10×10 = 1000 distinct Dingdong-style product sentences. Real enough
// to give the embedding model meaningful input; fixed so the cache can hit.
function buildTemplates() {
  const adj = ['新鲜', '有机', '精选', '进口', '当季', '冷链', '无添加', '特惠', '产地直供', '手工'];
  const cat = ['草莓', '番茄', '鸡胸肉', '三文鱼', '酸奶', '吐司', '坚果', '气泡水', '酱油', '大米'];
  const sell = ['适合家庭健康早餐', '营养均衡之选', '快手菜首选', '下午茶必备', '宴客硬菜',
    '减脂轻食推荐', '儿童营养补给', '送礼优选', '囤货更划算', '即食方便'];
  const out = [];
  for (const a of adj) for (const c of cat) for (const s of sell) out.push(`${a}${c} ${s}`);
  return out;
}

// Run async tasks with a fixed concurrency ceiling.
async function mapLimit(items, limit, fn) {
  const results = [];
  let i = 0;
  const workers = Array.from({ length: limit }, async () => {
    while (i < items.length) {
      const idx = i++;
      results[idx] = await fn(items[idx], idx);
    }
  });
  await Promise.all(workers);
  return results;
}

const env = loadEnv();
const BASE = env.BASE.replace(/\/$/, '');
const WK = env.WK;
const TOTAL = Number(process.env.TOTAL || 20000);
const BATCH = Number(process.env.BATCH || 500);
const CONCURRENCY = Number(process.env.CONCURRENCY || 8);
const PREFIX = process.env.PREFIX || 'seed';
const WRITE_MODE = process.env.WRITE_MODE || '';

const templates = buildTemplates();

async function upsertBatch(start) {
  const records = [];
  for (let i = start; i < Math.min(start + BATCH, TOTAL); i++) {
    records.push({
      id: `${PREFIX}-${i}`,
      text: templates[i % templates.length],
      category: ['fruit', 'veg', 'meat', 'seafood', 'dairy'][i % 5],
      meta: { price: (i * 37) % 9990 + 10, idx: i },
    });
  }
  const res = await fetch(`${BASE}/v1/vectors/upsert`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${WK}` },
    body: JSON.stringify(WRITE_MODE ? { write_mode: WRITE_MODE, records } : { records }),
  });
  if (!res.ok) {
    const t = await res.text();
    throw new Error(`upsert @${start} -> ${res.status}: ${t.slice(0, 200)}`);
  }
  return records.length;
}

async function main() {
  const starts = [];
  for (let s = 0; s < TOTAL; s += BATCH) starts.push(s);
  console.log(`seeding ${TOTAL} records in ${starts.length} batches of ${BATCH}, concurrency ${CONCURRENCY}`);
  console.log('(first ~2 batches are slow: they warm the embedding cache for the 1000-sentence pool)');

  const t0 = Date.now();
  let done = 0;
  await mapLimit(starts, CONCURRENCY, async (s) => {
    const n = await upsertBatch(s);
    done += n;
    process.stdout.write(`\r  ${done}/${TOTAL} (${((Date.now() - t0) / 1000).toFixed(1)}s)`);
  });
  const secs = (Date.now() - t0) / 1000;
  console.log(`\ndone: ${TOTAL} records in ${secs.toFixed(1)}s (${(TOTAL / secs).toFixed(0)} rec/s)`);
}

main().catch((e) => {
  console.error('\nseed failed:', e.message);
  process.exit(1);
});
