#!/usr/bin/env python3
# Drop the Milvus collections behind given test workspace ids. Pairs with the
# control-plane cleanup (DELETE /v1/workspaces/{id} soft-deletes MySQL rows and
# revokes keys, but leaves the collection — this drops the vector data, which
# is the bulk).
#
# Works against any Milvus; --mv/--token/--db are required so pointing it at
# prod is a conscious act, never a default:
#   python3 cleanup_test_data.py --mv http://milvus-dist.test.srv.mc.dd:19530 \
#       --token 'user:pw' --db public <ws_id> [<ws_id>...]
import argparse
import hashlib
import json
import urllib.request


def coll(ws):
    return "ws_" + hashlib.sha256(ws.encode()).hexdigest()[:16] + "_default"


def drop(mv, token, name, db):
    body = {"collectionName": name, "dbName": db}
    req = urllib.request.Request(
        f"{mv}/v2/vectordb/collections/drop", data=json.dumps(body).encode(), method="POST",
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"},
    )
    try:
        return json.loads(urllib.request.urlopen(req).read())
    except Exception as e:
        return {"err": str(e)[:120]}


ap = argparse.ArgumentParser(description="drop Milvus collections of test workspaces")
ap.add_argument("--mv", required=True, help="Milvus REST base, e.g. http://host:19530")
ap.add_argument("--token", required=True, help="user:password")
ap.add_argument("--db", required=True, help="Milvus dbName the collections live in")
ap.add_argument("ws", nargs="+", help="workspace id(s) to clean")
args = ap.parse_args()

for ws in args.ws:
    name = coll(ws)
    r = drop(args.mv, args.token, name, args.db)
    print(f"drop {name} (db={args.db}): {r.get('code', r)}")
