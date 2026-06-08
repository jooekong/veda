#!/usr/bin/env python3
# Large-N physical row count: does insert leave N rows for the same pk while
# upsert keeps 1? Big N drowns out background noise / rowCount lag.
import urllib.request, json, time

MV = "http://milvus-dist.test.srv.mc.dd:19530"
TOK = "rw_public:N1GQHYcUEWgg"
COLL = "ws_cdc5ae9908fd6013_default"
N = 30


def call(ep, body):
    try:
        req = urllib.request.Request(
            f"{MV}/v2/vectordb/{ep}", data=json.dumps(body).encode(), method="POST",
            headers={"Content-Type": "application/json", "Authorization": f"Bearer {TOK}"},
        )
        return json.loads(urllib.request.urlopen(req).read())
    except urllib.error.HTTPError as e:
        return {"_err": e.code, "_body": e.read().decode()[:200]}


def stats():
    return call("collections/get_stats", {"collectionName": COLL, "dbName": "public"}).get("data", {}).get("rowCount")


def flush():
    call("collections/flush", {"collectionName": COLL, "dbName": "public"})


def rec(pk, i):
    return {"pk": pk, "id": pk, "dataset": "default", "category": "x", "tags": [],
            "status": "active", "created_at": 1, "updated_at": 1, "text": f"t{i}",
            "vector": [round(0.1 + i * 1e-4, 6)] * 1024, "meta": {}}


ts = str(int(time.time()))
print("baseline rowCount samples (3x, ~2s apart):", end=" ")
for _ in range(3):
    print(stats(), end=" ")
    time.sleep(2)
print()

# INSERT same pk N times
ipk = f"rc-ins-{ts}"
b = stats()
for i in range(N):
    call("entities/insert", {"collectionName": COLL, "dbName": "public", "data": [rec(ipk, i)]})
flush()
time.sleep(8)
ai = stats()
print(f"INSERT same pk x{N}:  rowCount {b} -> {ai}  (delta {ai - b})   [expect ~+{N} if physical rows kept]")

# UPSERT same pk N times
upk = f"rc-ups-{ts}"
b2 = stats()
for i in range(N):
    call("entities/upsert", {"collectionName": COLL, "dbName": "public", "data": [rec(upk, i)]})
flush()
time.sleep(8)
au = stats()
print(f"UPSERT same pk x{N}:  rowCount {b2} -> {au}  (delta {au - b2})   [expect ~+1 if in-place replace]")
