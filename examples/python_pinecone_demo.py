#!/usr/bin/env python3
"""Pinecone-style usage of Veda's /v1/vectors/* data-plane endpoints.

Run against a Veda server with a db-kind workspace already created and a
`wk_` workspace key for it. The target workspace is bound to the `wk_` key, so
requests carry NO workspace_id. Env vars:

    VEDA_URL       e.g. http://localhost:3000 for a server you run yourself;
                   the deployed data plane is https://veda.ddmc-inc.com (prod)
                   or https://veda.dbpaas.dingdongxiaoqu.com (test)
    VEDA_API_KEY   wk_... workspace key (data-plane; NOT an account vk_)

Demonstrates: upsert with defaults, search with a meta-field filter,
query by id, delete.
"""
from __future__ import annotations

import json
import os
import sys
from typing import Any

import urllib.request


def request(method: str, url: str, body: dict[str, Any] | None = None) -> dict[str, Any]:
    api_key = os.environ["VEDA_API_KEY"]
    headers = {"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"}
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    with urllib.request.urlopen(req) as resp:
        return json.loads(resp.read())


def main() -> int:
    base = os.environ["VEDA_URL"].rstrip("/")

    # 1. Upsert two records into the bootstrapped "default" dataset.
    #    The workspace is bound to the wk_ key — no workspace_id in the body.
    upserted = request("POST", f"{base}/v1/vectors/upsert", {
        "records": [
            {"id": "sku-1", "text": "Air Jordan 1", "meta": {"price": 1299}},
            {"id": "sku-2", "text": "Yeezy 350",    "meta": {"price": 1599}},
        ],
    })
    print("upsert ids:", upserted["data"]["ids"])

    # 2. Semantic search with a meta-field filter (price < 1500). mode defaults
    #    to hybrid; pick semantic/fulltext explicitly.
    found = request("POST", f"{base}/v1/vectors/search", {
        "query": "sneakers under 1500",
        "mode": "semantic",
        "top_k": 5,
        "filter": {"must": [{"field": "meta.price", "op": "lt", "value": 1500}]},
    })
    for hit in found["data"]["hits"]:
        print(f"  hit: {hit['id']} score={hit['score']:.4f} meta={hit['meta']}")

    # 3. Query by id (no semantic search; direct lookup).
    queried = request("POST", f"{base}/v1/vectors/query", {
        "ids": ["sku-1", "sku-2"],
    })
    print("query hits:", [h["id"] for h in queried["data"]["hits"]])

    # 4. Delete both records.
    deleted = request("POST", f"{base}/v1/vectors/delete", {
        "ids": ["sku-1", "sku-2"],
    })
    print("delete:", deleted["data"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
