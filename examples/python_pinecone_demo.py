#!/usr/bin/env python3
"""Pinecone-style usage of Veda's /v1/vectors/* endpoints.

Run against a Veda server with a db-kind workspace already created and a
vk_ API key scoped to (at minimum) that workspace. Env vars:

    VEDA_URL       e.g. http://localhost:9009
    VEDA_API_KEY   vk_... token
    VEDA_WS_ID     UUID of the db-kind workspace

Demonstrates: upsert with defaults, search with a meta-field filter,
query by row_key, delete.
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
    ws = os.environ["VEDA_WS_ID"]

    # 1. Upsert two records into the bootstrapped "default" dataset.
    upserted = request("POST", f"{base}/v1/vectors/upsert", {
        "workspace_id": ws,
        "records": [
            {"row_key": "sku-1", "text": "Air Jordan 1", "meta": {"price": 1299}},
            {"row_key": "sku-2", "text": "Yeezy 350",    "meta": {"price": 1599}},
        ],
    })
    print("upsert:", upserted["data"])

    # 2. Semantic search with a meta-field filter (price < 1500).
    found = request("POST", f"{base}/v1/vectors/search", {
        "workspace_id": ws,
        "query": "sneakers under 1500",
        "top_k": 5,
        "filter": {"must": [{"field": "meta.price", "op": "lt", "value": 1500}]},
    })
    for hit in found["data"]["hits"]:
        print(f"  hit: {hit['row_key']} score={hit['score']:.4f} meta={hit['meta']}")

    # 3. Query by row_key (no semantic search; direct lookup).
    queried = request("POST", f"{base}/v1/vectors/query", {
        "workspace_id": ws,
        "row_keys": ["sku-1", "sku-2"],
    })
    print("query hits:", [h["row_key"] for h in queried["data"]["hits"]])

    # 4. Delete both records.
    deleted = request("POST", f"{base}/v1/vectors/delete", {
        "workspace_id": ws,
        "row_keys": ["sku-1", "sku-2"],
    })
    print("delete:", deleted["data"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
