#!/usr/bin/env python3
# Bare Milvus REST bench: insert vs upsert, bypassing veda, to isolate the
# cost of upsert's dedup+delete semantics vs a plain insert. Run ON .161 so
# the Milvus hop is same-datacenter (comparable to the veda-side 600ms data).
#
# Env: OP=insert|upsert  N_REQ=200  BATCH=50  CONC=10
#      MILVUS_URL / MILVUS_TOKEN / MILVUS_DB / MILVUS_COLLECTION
import os, time, json, random
import urllib.request
from concurrent.futures import ThreadPoolExecutor

URL = os.environ.get("MILVUS_URL", "http://milvus-dist.test.srv.mc.dd:19530")
TOKEN = os.environ.get("MILVUS_TOKEN", "rw_public:N1GQHYcUEWgg")
DB = os.environ.get("MILVUS_DB", "public")
COLL = os.environ.get("MILVUS_COLLECTION", "ws_cdc5ae9908fd6013_default")
OP = os.environ.get("OP", "upsert")
N_REQ = int(os.environ.get("N_REQ", "200"))
BATCH = int(os.environ.get("BATCH", "50"))
CONC = int(os.environ.get("CONC", "10"))
RUN = str(int(time.time()))[-6:]  # unique pk prefix per run

VEC = [round(random.random(), 4) for _ in range(1024)]
URL_OP = f"{URL}/v2/vectordb/entities/{OP}"
HDR = {"Content-Type": "application/json", "Authorization": f"Bearer {TOKEN}"}


def make_batch(req_idx):
    now = int(time.time() * 1000)
    return [{
        "pk": f"bench-{RUN}-{req_idx}-{j}", "id": f"bench-{RUN}-{req_idx}-{j}",
        "dataset": "default", "category": "bench", "tags": [],
        "status": "active", "created_at": now, "updated_at": now,
        "text": "bench record for milvus insert vs upsert comparison",
        "vector": VEC, "meta": {},
    } for j in range(BATCH)]


def do_request(req_idx):
    body = json.dumps({"collectionName": COLL, "data": make_batch(req_idx), "dbName": DB}).encode()
    req = urllib.request.Request(URL_OP, data=body, method="POST", headers=HDR)
    t0 = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            code = json.loads(resp.read()).get("code", 0)
        return (time.perf_counter() - t0, code == 0, None if code == 0 else f"code={code}")
    except Exception as e:
        return (time.perf_counter() - t0, False, str(e)[:80])


print(f"OP={OP} N_REQ={N_REQ} BATCH={BATCH} CONC={CONC}")
t_start = time.perf_counter()
lats, errs, err_sample = [], 0, None
with ThreadPoolExecutor(max_workers=CONC) as ex:
    for lat, ok, err in ex.map(do_request, range(N_REQ)):
        lats.append(lat)
        if not ok:
            errs += 1
            err_sample = err_sample or err
wall = time.perf_counter() - t_start
lats.sort()
pct = lambda p: lats[min(len(lats) - 1, int(len(lats) * p))] * 1000
print(f"  errs={errs} wall={wall:.1f}s  throughput={N_REQ/wall:.1f} req/s ({N_REQ*BATCH/wall:.0f} rec/s)")
print(f"  latency ms: avg={sum(lats)/len(lats)*1000:.0f} p50={pct(0.5):.0f} "
      f"p95={pct(0.95):.0f} p99={pct(0.99):.0f} max={lats[-1]*1000:.0f}")
if err_sample:
    print(f"  err: {err_sample}")
