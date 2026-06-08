#!/usr/bin/env python3
# Diagnostic: does Milvus insert with a DUPLICATE pk produce 2 rows, and does
# query by pk return both? Decides whether the write_mode=insert "multi-row"
# semantics (Q3/Q4) are even achievable on Milvus.
import urllib.request, json, time

MV = "http://milvus-dist.test.srv.mc.dd:19530"
TOK = "rw_public:N1GQHYcUEWgg"
COLL = "ws_cdc5ae9908fd6013_default"


def call(ep, body):
    req = urllib.request.Request(
        f"{MV}/v2/vectordb/{ep}", data=json.dumps(body).encode(), method="POST",
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {TOK}"},
    )
    return json.loads(urllib.request.urlopen(req).read())


VEC = [0.1] * 1024
def rec(pk, txt):
    return {"pk": pk, "id": pk, "dataset": "default", "category": "x", "tags": [],
            "status": "active", "created_at": 1, "updated_at": 1, "text": txt,
            "vector": VEC, "meta": {}}


PK = "duptest-" + str(int(time.time()))
print("pk =", PK)
print("insert#1 code =", call("entities/insert", {"collectionName": COLL, "dbName": "public", "data": [rec(PK, "AAA")]}).get("code"))
print("insert#2 code =", call("entities/insert", {"collectionName": COLL, "dbName": "public", "data": [rec(PK, "BBB")]}).get("code"))
time.sleep(2)
r = call("entities/query", {"collectionName": COLL, "dbName": "public",
                            "filter": f'pk in ["{PK}"]', "limit": 100,
                            "outputFields": ["pk", "text"], "consistencyLevel": "Strong"})
rows = r.get("data", [])
print("query rows =", len(rows))
for row in rows:
    print("   ", row.get("pk"), "->", row.get("text"))
