# Troubleshooting

## `veda` says "unauthorized"

Your account key or workspace key was revoked or expired. Either:

- Re-onboard from the [home page](/) and paste the new `vk_` with `veda init --import-key vk_…`.
- Or, from the [Console](#/console), mint a new workspace key for an existing workspace and re-import.

The config file is at `~/.config/veda/config.toml`. Editing the `api_key` directly works too.

## `veda` complains about server URL

Make sure `~/.config/veda/config.toml` has the right `server_url`. Override per-command:

```bash
veda --server https://veda.dbpaas.dingdongxiaoqu.com ls
```

## Search returns nothing

- Embedding is **asynchronous** — give it a few seconds, then retry after ~5s if still missing.
- Check the server is reachable: `veda status`.
- Tiny files (a few words) may not match BM25. Try `--mode semantic`.
- For a guaranteed literal hit: `veda grep "string"` (sync, no embedding lag).

## FUSE mount shows stale data

The SSE event stream may have disconnected. The mount reconnects automatically; if you can't wait, `fusermount -u ~/veda && veda-fuse mount …` resets cleanly.

## Anonymous account got "lost"

The account itself lives on the server, but the `vk_` from your first onboarding was only kept in the browser that minted it. If you cleared localStorage or used a different browser, you can't recover it without claiming the account first.

**Prevention**: on the [Console](#/console) page, click **Claim account** and set an email + password before clearing browser data. Then `veda init --login --email …` works anywhere.

## "permission denied" on a write

Your key is read-only. Mint a `readwrite` key from the [Console](#/console).

## Found a bug?

File an issue at [git.ddxq.mobi/middleware/dbpaas/veda](http://git.ddxq.mobi/middleware/dbpaas/veda) with `veda --version` output and the full command you ran.
