#!/usr/bin/env python3
# Rigorous verify: are insert and upsert really "the same" on a pk?
# Writes the SAME pk 3x with DIFFERENT vectors+text, then checks:
#   - query by pk: how many rows? which text? (read dedup + which wins)
#   - search with the 3rd vector: how many times does the pk appear? (does
#     ANN search dedup too, or surface duplicate physical rows?)
#   - collection rowCount before/after (physical rows: does insert leave
#     garbage rows vs upsert?)
import urllib.request, json, time

MV = "http://milvus-dist.test.srv.mc.dd:19530"
TOK = "rw_public:N1GQHYcUEWgg"
COLL = "ws_cdc5ae9908fd6013_default"


def call(ep, body):
    try:
        req = urllib.request.Request(
            f"{MV}/v2/vectordb/{ep}", data=json.dumps(body).encode(), method="POST",
            headers={"Content-Type": "application/json", "Authorization": f"Bearer {TOK}"},
        )
        return json.loads(urllib.request.urlopen(req).read())
    except urllib.error.HTTPError as e:
        return {"_http_error": e.code, "_body": e.read().decode()[:300]}


def vecv(b):
    return [round(b + i * 1e-6, 6) for i in range(1024)]  # distinct per base


def rec(pk, txt, v):
    return {"pk": pk, "id": pk, "dataset": "default", "category": "x", "tags": [],
            "status": "active", "created_at": 1, "updated_at": 1, "text": txt,
            "vector": v, "meta": {}}


def stats():
    r = call("collections/get_stats", {"collectionName": COLL, "dbName": "public"})
    return r.get("data", r)


def flush():
    return call("collections/flush", {"collectionName": COLL, "dbName": "public"})


def query_pk(pk):
    r = call("entities/query", {"collectionName": COLL, "dbName": "public",
                                "filter": f'pk in ["{pk}"]', "limit": 100,
                                "outputFields": ["pk", "text"], "consistencyLevel": "Strong"})
    return r.get("data", r)


def search_vec(v, pk):
    r = call("entities/search", {"collectionName": COLL, "dbName": "public",
                                 "data": [v], "annsField": "vector", "limit": 50,
                                 "outputFields": ["pk", "text"], "consistencyLevel": "Strong"})
    rows = r.get("data", r)
    if not isinstance(rows, list):
        return rows
    return [h for h in rows if h.get("pk") == pk]


ts = str(int(time.time()))
samples = [("AAA", vecv(0.11)), ("BBB", vecv(0.22)), ("CCC", vecv(0.33))]

print(f"=== stats before === {stats()}")

# ---- INSERT same pk 3x ----
ipk = f"ins-{ts}"
for t, v in samples:
    print(f"insert {t}: code={call('entities/insert', {'collectionName': COLL, 'dbName': 'public', 'data': [rec(ipk, t, v)]}).get('code')}")
print(f"flush: {flush().get('code', flush())}")
time.sleep(4)
qi = query_pk(ipk)
si = search_vec(vecv(0.33), ipk)  # search with CCC (the last-written) vector
print(f"\n[INSERT pk={ipk}]")
print(f"  query rows = {len(qi) if isinstance(qi, list) else qi}  texts={[r.get('text') for r in qi] if isinstance(qi, list) else '-'}")
print(f"  search pk-hits = {len(si) if isinstance(si, list) else si}  texts={[h.get('text') for h in si] if isinstance(si, list) else '-'}")
print(f"  stats after insert = {stats()}")

# ---- UPSERT same pk 3x ----
upk = f"ups-{ts}"
for t, v in samples:
    print(f"upsert {t}: code={call('entities/upsert', {'collectionName': COLL, 'dbName': 'public', 'data': [rec(upk, t, v)]}).get('code')}")
print(f"flush: {flush().get('code', flush())}")
time.sleep(4)
qu = query_pk(upk)
su = search_vec(vecv(0.33), upk)
print(f"\n[UPSERT pk={upk}]")
print(f"  query rows = {len(qu) if isinstance(qu, list) else qu}  texts={[r.get('text') for r in qu] if isinstance(qu, list) else '-'}")
print(f"  search pk-hits = {len(su) if isinstance(su, list) else su}  texts={[h.get('text') for h in su] if isinstance(su, list) else '-'}")
print(f"  stats after upsert = {stats()}")
