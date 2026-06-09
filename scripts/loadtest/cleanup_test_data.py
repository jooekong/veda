#!/usr/bin/env python3
# One-shot cleanup of THIS session's test workspaces' Milvus collections.
# Targets ONLY the exact ws ids we provisioned (printed during the session),
# so dogfood collections are never touched. Drops the vector data (the bulk);
# MySQL metadata rows are left as harmless orphans (test env / GC todo).
import urllib.request, json, hashlib

MV = "http://milvus-dist.test.srv.mc.dd:19530"
TOK = "rw_public:N1GQHYcUEWgg"


def coll(ws):
    return "ws_" + hashlib.sha256(ws.encode()).hexdigest()[:16] + "_default"


def drop(name, db):
    body = {"collectionName": name, "dbName": db}
    req = urllib.request.Request(
        f"{MV}/v2/vectordb/collections/drop", data=json.dumps(body).encode(), method="POST",
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {TOK}"},
    )
    try:
        return json.loads(urllib.request.urlopen(req).read())
    except Exception as e:
        return {"err": str(e)[:120]}


# Exact test ws ids provisioned this session (load test + e2e + first local run).
TESTS = [
    ("268812e4-b60e-4fdf-94eb-f2ace52bf49c", "public", ".161 load test (seed 20k + bench/dup/rc)"),
    ("1c42edec-2c21-41e9-988d-6ac623e588d4", "public", ".161 write_mode e2e"),
    ("a3cbaa1d-7397-4f7c-b7c3-2258bde1230d", "aitest_kb", "first local load test (seed 20k)"),
]

for ws, db, desc in TESTS:
    name = coll(ws)
    r = drop(name, db)
    code = r.get("code", r)
    print(f"drop {name} (db={db}) — {desc}: {code}")
