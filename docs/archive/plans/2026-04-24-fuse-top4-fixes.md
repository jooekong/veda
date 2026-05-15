# FUSE Top-4 Fixes Implementation Plan

> **For agentic workers:** Implement task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Do NOT start Task N+1 until Task N tests pass AND the code has been committed.

**Goal:** Fix the four highest-priority FUSE correctness / regression bugs identified in the code review, one at a time, each with tests and a commit before moving on.

**Architecture:** Each task is a self-contained change to `crates/veda-fuse/src/*.rs`. No cross-crate changes. Tests are added inline as `#[cfg(test)] mod tests` blocks in the same files where feasible, plus one integration test file for binary round-trip. Manual-test steps are documented for behavior that cannot be unit-tested (signal handling, live SSE).

**Tech Stack:** Rust 2021, `fuser` 0.14, `reqwest` 0.12 blocking, `nix` 0.29, `signal-hook` 0.3. Workspace note: `crates/veda-fuse` is **excluded** from the top-level Cargo workspace — build and test with `cd crates/veda-fuse && cargo <cmd>`.

---

## Pre-flight

- **Step 0.1: Confirm starting baseline is green**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo build && cargo test
```

Expected: build succeeds, existing tests in `cache.rs`, `main.rs`, `sse.rs` all pass. If any fail, stop and investigate before starting.

- **Step 0.2: Record current git status**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda && git status
```

Expected: the three pre-existing dirty files (`crates/veda-fuse/src/fs.rs`, `crates/veda-fuse/src/main.rs`, `crates/veda-store/src/mysql.rs`) plus `docs/manual-test-sop.md` and `scripts/` untracked. Note the SHA: `git rev-parse HEAD` — we want every commit from this plan to stack cleanly on top.

---

## File Structure


| File                                         | Task    | Responsibility                                                                                               |
| -------------------------------------------- | ------- | ------------------------------------------------------------------------------------------------------------ |
| `crates/veda-fuse/src/fs.rs`                 | 1, 2, 4 | FUSE filesystem ops. Touched for readdir attr fix, `read_file` call sites, and making `dir_cache` shareable. |
| `crates/veda-fuse/src/client.rs`             | 2       | HTTP client. `read_file` signature changes from `Result<String>` to `Result<Vec<u8>>`.                       |
| `crates/veda-fuse/src/sse.rs`                | 4       | SSE watcher. Accept `dir_cache` arg and invalidate it on events.                                             |
| `crates/veda-fuse/src/main.rs`               | 3       | Binary entry. Replace `exit(0)` signal handler with graceful unmount.                                        |
| `crates/veda-fuse/tests/binary_roundtrip.rs` | 2 (new) | Integration test using a local mock HTTP server to prove binary bytes survive round-trip.                    |


---

## Task 1: Stop caching size=0 attrs from readdir (bug_001)

**Root cause:** `FsService::list_dir` returns `size_bytes: None` for every entry. `readdir`/`readdirplus` unconditionally call `inode_set_attr(child_ino, attr)` where `attr.size = size_bytes.unwrap_or(0) = 0`. The new positive lookup/getattr cache then serves `size=0` for `attr_ttl` seconds.

**Fix (Option A, client-side):** Only cache the attr when `de.size_bytes.is_some()` OR the entry is a directory (dirs legitimately have size=0). For `readdirplus`, also pass `ttl = Duration::ZERO` when size is unknown so the kernel doesn't cache it either — it will immediately issue a `getattr` which will do a real `client.stat`.

**Files:**

- Modify: `crates/veda-fuse/src/fs.rs` — `readdir` around line 373-379, `readdirplus` around line 405-412

### Steps

- **Step 1.1: Add the fs.rs test module skeleton**

File: `crates/veda-fuse/src/fs.rs` — append at end of file.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DirEntry;

    fn make_dir_entry(name: &str, is_dir: bool, size: Option<i64>) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            path: format!("/{name}"),
            is_dir,
            size_bytes: size,
        }
    }

    #[test]
    fn attr_from_dir_entry_uses_zero_when_size_missing() {
        let de = make_dir_entry("a.txt", false, None);
        let attr = VedaFs::attr_from_dir_entry(&de, 42);
        // This documents the bug: when size is None, make_attr falls back to 0.
        // The fix is NOT to change this helper, but to avoid caching the result.
        assert_eq!(attr.size, 0);
    }

    #[test]
    fn attr_from_dir_entry_preserves_size() {
        let de = make_dir_entry("a.txt", false, Some(1234));
        let attr = VedaFs::attr_from_dir_entry(&de, 42);
        assert_eq!(attr.size, 1234);
    }
}
```

- **Step 1.2: Run the new tests to confirm the helper baseline works**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test --lib fs::tests
```

Expected: both tests PASS. These document the helper's current behavior; the real fix lives in the callers.

- **Step 1.3: Add a behavioral test for conditional attr caching**

File: `crates/veda-fuse/src/fs.rs` — append inside the `mod tests` block (under the helpers).

```rust
    /// Extracts the decision: "should we cache this attr after readdir?"
    /// Pure function over DirEntry so we can test without a VedaClient.
    fn should_cache_attr(de: &DirEntry) -> bool {
        de.is_dir || de.size_bytes.is_some()
    }

    #[test]
    fn file_with_unknown_size_is_not_cached() {
        let de = make_dir_entry("a.txt", false, None);
        assert!(!should_cache_attr(&de), "files with unknown size must not be cached");
    }

    #[test]
    fn file_with_known_size_is_cached() {
        let de = make_dir_entry("a.txt", false, Some(100));
        assert!(should_cache_attr(&de));
    }

    #[test]
    fn directory_is_always_cached_even_with_none_size() {
        let de = make_dir_entry("docs", true, None);
        assert!(should_cache_attr(&de), "directories legitimately have size=0");
    }
```

- **Step 1.4: Run the behavioral tests to confirm they fail**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test --lib fs::tests::file_with_unknown_size_is_not_cached fs::tests::file_with_known_size_is_cached fs::tests::directory_is_always_cached_even_with_none_size
```

Expected: FAIL with "cannot find function `should_cache_attr`" — because we haven't added it to the production code yet.

- **Step 1.5: Add `should_cache_attr` as a private `VedaFs` method and wire it into readdir/readdirplus**

File: `crates/veda-fuse/src/fs.rs`.

**(a) Add the helper** near the other `fn make_attr` / `fn attr_from_dir_entry` helpers (around line 106):

```rust
    /// True when a readdir entry has enough information to safely cache its attr.
    /// Files without a known size must NOT be cached as size=0, since the server's
    /// `list_dir` returns `size_bytes: None` and we would otherwise report 0 bytes
    /// for every file under `attr_ttl`.
    fn should_cache_attr(de: &DirEntry) -> bool {
        de.is_dir || de.size_bytes.is_some()
    }
```

**(b) Update `readdir` — replace the loop body at lines 373-379:**

Before:

```rust
        for de in &entries {
            let child_ino = self.inode_get_or_create(&de.path);
            let attr = Self::attr_from_dir_entry(de, child_ino);
            self.inode_set_attr(child_ino, attr);
            let kind = if de.is_dir { FileType::Directory } else { FileType::RegularFile };
            full_entries.push((child_ino, kind, de.name.clone()));
        }
```

After:

```rust
        for de in &entries {
            let child_ino = self.inode_get_or_create(&de.path);
            if Self::should_cache_attr(de) {
                let attr = Self::attr_from_dir_entry(de, child_ino);
                self.inode_set_attr(child_ino, attr);
            }
            let kind = if de.is_dir { FileType::Directory } else { FileType::RegularFile };
            full_entries.push((child_ino, kind, de.name.clone()));
        }
```

**(c) Update `readdirplus` — lines 402-413:**

Before:

```rust
        let mut full: Vec<(u64, String, FileAttr)> = Vec::with_capacity(entries.len() + 2);
        full.push((ino, ".".to_string(), self_attr));
        full.push((parent_ino, "..".to_string(), parent_attr));
        for de in &entries {
            let child_ino = self.inode_get_or_create(&de.path);
            let attr = Self::attr_from_dir_entry(de, child_ino);
            self.inode_set_attr(child_ino, attr);
            full.push((child_ino, de.name.clone(), attr));
        }
        for (i, (child_ino, name, attr)) in full.iter().enumerate().skip(offset as usize) {
            if reply.add(*child_ino, (i + 1) as i64, name, &self.config.attr_ttl, attr, 0) { break; }
        }
```

After:

```rust
        // (ino, name, attr, cache_ok). When cache_ok=false we pass Duration::ZERO
        // as TTL so the kernel immediately re-fetches via getattr.
        let mut full: Vec<(u64, String, FileAttr, bool)> = Vec::with_capacity(entries.len() + 2);
        full.push((ino, ".".to_string(), self_attr, true));
        full.push((parent_ino, "..".to_string(), parent_attr, true));
        for de in &entries {
            let child_ino = self.inode_get_or_create(&de.path);
            let attr = Self::attr_from_dir_entry(de, child_ino);
            let cache_ok = Self::should_cache_attr(de);
            if cache_ok {
                self.inode_set_attr(child_ino, attr);
            }
            full.push((child_ino, de.name.clone(), attr, cache_ok));
        }
        let zero_ttl = Duration::ZERO;
        for (i, (child_ino, name, attr, cache_ok)) in full.iter().enumerate().skip(offset as usize) {
            let ttl = if *cache_ok { &self.config.attr_ttl } else { &zero_ttl };
            if reply.add(*child_ino, (i + 1) as i64, name, ttl, attr, 0) { break; }
        }
```

- **Step 1.6: Delete the test-only `should_cache_attr` shim from the test module**

Remove the duplicated `fn should_cache_attr` from inside `mod tests` so the tests now reference `VedaFs::should_cache_attr`. Update each test to use `VedaFs::should_cache_attr(&de)` instead of bare `should_cache_attr(&de)`.

- **Step 1.7: Run all veda-fuse tests**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test
```

Expected: ALL PASS — the three new behavioral tests plus the two helper tests plus existing `cache`, `main`, `sse` tests.

- **Step 1.8: Manual smoke test**

Document the manual verification steps the user should run (do not actually run them — they need a live mount).

Run (user):

```bash
# In one terminal, start veda-server (per existing docs/).
# In another:
cd /Users/konglingqiao/code/personal/veda
cargo build -p veda-fuse --release --manifest-path crates/veda-fuse/Cargo.toml
# Upload a known-size file via API, e.g. 12345 bytes as /sizetest.bin.
./crates/veda-fuse/target/release/veda-fuse mount --server http://127.0.0.1:8080 --key $VEDA_KEY /tmp/veda --foreground &
ls -l /tmp/veda/
# Expected: sizetest.bin shows 12345, not 0.
stat /tmp/veda/sizetest.bin
# Expected: Size: 12345
```

- **Step 1.9: Commit**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda
git add crates/veda-fuse/src/fs.rs
git diff --cached crates/veda-fuse/src/fs.rs | head -120  # show user the diff first per CLAUDE.md
git commit -m "$(cat <<'EOF'
fix(fuse): skip attr cache for files with unknown size

Server-side FsService::list_dir returns size_bytes=None for every entry,
so caching the resulting attr (size=0) caused ls -l / stat / du to
report every just-listed file as 0 bytes for attr_ttl seconds. Only
cache when size is known; in readdirplus, pass Duration::ZERO TTL for
unknown-size entries so the kernel issues a real getattr.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

**GATE:** Do not proceed to Task 2 until `cargo test` passes AND the commit is in. Confirm with: `git log --oneline -1` shows the fix.

---

## Task 2: Return `Vec<u8>` from `read_file` (binary file corruption)

**Root cause:** `VedaClient::read_file` returns `Result<String>` via `resp.text()`, which forcibly UTF-8-decodes the body. Any non-UTF-8 byte sequence (binary files: images, archives, executables) causes `reqwest` to return `Io` or corrupts the bytes after `.into_bytes()` if characters were replaced.

**Fix:** Change the signature to return `Result<Vec<u8>>` via `resp.bytes()`. Update all 3 call sites in `fs.rs` to drop the now-redundant `.into_bytes()`. Add an integration test using a local mock HTTP server that serves non-UTF-8 bytes and verifies they round-trip cleanly.

**Files:**

- Modify: `crates/veda-fuse/src/client.rs:103-113`
- Modify: `crates/veda-fuse/src/fs.rs:333, 432-433, 479-488`
- Create: `crates/veda-fuse/tests/binary_roundtrip.rs`
- Modify: `crates/veda-fuse/Cargo.toml` — add `[dev-dependencies]`

### Steps

- **Step 2.1: Add dev-dependency for the mock server**

We use raw `std::net::TcpListener` to keep dep surface minimal — no new crates needed.

File: `crates/veda-fuse/Cargo.toml` — append:

```toml
[dev-dependencies]
# (no new runtime deps; integration tests use std::net)
```

(This is effectively a no-op but documents the intent. Skip if the section would be empty.)

- **Step 2.2: Create the integration test file**

File: `crates/veda-fuse/tests/binary_roundtrip.rs` (new).

```rust
//! Integration test: verify VedaClient::read_file returns raw bytes untouched,
//! including invalid-UTF-8 sequences typical of binary files.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

// We cannot import VedaClient directly since the crate is a bin, not a lib.
// Workaround: re-export client module via a tiny shim, OR restructure the crate
// to expose src/lib.rs. We take the latter in Step 2.3.

use veda_fuse::client::VedaClient;

/// Spawn a one-shot HTTP server that returns the given body for the next GET.
/// Returns the base URL (e.g. "http://127.0.0.1:PORT").
fn spawn_mock(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf); // read request line + headers
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn read_file_round_trips_non_utf8_bytes() {
    // Bytes that are NOT valid UTF-8: 0xFF, 0xFE, standalone 0x80, etc.
    let binary: Vec<u8> = vec![0x00, 0xFF, 0xFE, 0x80, 0x81, 0xC0, 0xC1, 0xF5, 0xFF, 0x00];
    let base = spawn_mock(binary.clone());
    let client = VedaClient::new(&base, "dummy-key");
    let got = client.read_file("/blob").expect("read_file should not error on binary");
    assert_eq!(got, binary, "bytes must round-trip unchanged");
}

#[test]
fn read_file_round_trips_empty_body() {
    let base = spawn_mock(Vec::new());
    let client = VedaClient::new(&base, "dummy-key");
    let got = client.read_file("/empty").expect("empty body is valid");
    assert!(got.is_empty());
}
```

- **Step 2.3: Expose the crate as a library for integration tests**

Currently `veda-fuse` is binary-only (`[[bin]] name = "veda-fuse" path = "src/main.rs"`). To use `use veda_fuse::client::VedaClient` in `tests/`, add a library target that re-exports the modules.

File: `crates/veda-fuse/src/lib.rs` (new).

```rust
pub mod cache;
pub mod client;
pub mod fs;
pub mod inode;
pub mod sse;
```

File: `crates/veda-fuse/Cargo.toml` — add a `[lib]` section:

```toml
[lib]
name = "veda_fuse"
path = "src/lib.rs"
```

File: `crates/veda-fuse/src/main.rs` — change the module declarations to use the library instead:

Before (lines 1-5):

```rust
mod cache;
mod client;
mod fs;
mod inode;
mod sse;
```

After:

```rust
use veda_fuse::{cache, client, fs, sse};
```

(We drop `mod inode` because `main.rs` does not reference `inode` directly; if compile fails, add `inode` back.)

- **Step 2.4: Run the test — confirm it fails against current `read_file`**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test --test binary_roundtrip
```

Expected: FAIL. Either `read_file` returns `Err(Io(...))` from a UTF-8 decode error, or `assert_eq!` fails because bytes were corrupted. Alternatively, test fails at compile time because the signature is `Result<String>` but we assert against `Vec<u8>` — equally valid proof the signature needs to change.

- **Step 2.5: Change `read_file` to return `Vec<u8>`**

File: `crates/veda-fuse/src/client.rs` — replace lines 103-113.

Before:

```rust
    pub fn read_file(&self, path: &str) -> Result<String> {
        let path = path.trim_start_matches('/');
        let resp = self.http.get(format!("{}/v1/fs/{path}", self.base))
            .bearer_auth(&self.key)
            .send()
            .map_err(|e| ClientError::Io(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().map_err(|e| ClientError::Io(e.to_string()))?;
        Self::check_status(status, &body)?;
        Ok(body)
    }
```

After:

```rust
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let path = path.trim_start_matches('/');
        let resp = self.http.get(format!("{}/v1/fs/{path}", self.base))
            .bearer_auth(&self.key)
            .send()
            .map_err(|e| ClientError::Io(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            // Error bodies are small + expected UTF-8; read as text for diagnostics.
            let body = resp.text().unwrap_or_default();
            return Err(match status.as_u16() {
                404 => ClientError::NotFound,
                409 => ClientError::AlreadyExists,
                403 => ClientError::PermissionDenied,
                412 => ClientError::Conflict,
                _ => ClientError::Io(format!("HTTP {status}: {body}")),
            });
        }
        resp.bytes()
            .map(|b| b.to_vec())
            .map_err(|e| ClientError::Io(e.to_string()))
    }
```

- **Step 2.6: Update `fs.rs` call sites to drop redundant `.into_bytes()`**

File: `crates/veda-fuse/src/fs.rs`.

**(a) `setattr` truncate path (line ~333):**

Before:

```rust
                match self.client.read_file(&path) {
                    Ok(content) => {
                        let mut bytes = content.into_bytes();
```

After:

```rust
                match self.client.read_file(&path) {
                    Ok(mut bytes) => {
```

**(b) `open` for write (lines ~432-433):**

Before:

```rust
                        let buf = match self.client.read_file(&path) {
                            Ok(content) => content.into_bytes(),
                            Err(ClientError::NotFound) => Vec::new(),
```

After:

```rust
                        let buf = match self.client.read_file(&path) {
                            Ok(bytes) => bytes,
                            Err(ClientError::NotFound) => Vec::new(),
```

**(c) `read` hot path (lines ~479-488):**

Before:

```rust
            match self.client.read_file(&path) {
                Ok(content) => {
                    let bytes = content.into_bytes();
                    let off = offset as usize;
```

After:

```rust
            match self.client.read_file(&path) {
                Ok(bytes) => {
                    let off = offset as usize;
```

- **Step 2.7: Run the integration test — confirm it passes**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test --test binary_roundtrip
```

Expected: BOTH tests PASS.

- **Step 2.8: Run the full test suite to check for regressions**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test
```

Expected: ALL PASS (Task 1 tests + cache/main/sse tests + new binary_roundtrip integration).

- **Step 2.9: Manual smoke test — real binary file through a live mount**

Document for user (do not run automatically):

```bash
# With mount active from Task 1:
# Upload /tmp/test.png via API (any real PNG file).
cp /path/to/any/image.png /tmp/veda/image.png
md5 /tmp/veda/image.png  # read back via FUSE
md5 /path/to/any/image.png  # read original
# Expected: same MD5. Before the fix, the FUSE read would either error
# or produce a different hash due to UTF-8 replacement chars.
```

- **Step 2.10: Commit**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda
git add crates/veda-fuse/src/client.rs crates/veda-fuse/src/fs.rs crates/veda-fuse/src/lib.rs crates/veda-fuse/src/main.rs crates/veda-fuse/Cargo.toml crates/veda-fuse/tests/binary_roundtrip.rs
git diff --cached --stat
git commit -m "$(cat <<'EOF'
fix(fuse): return raw bytes from read_file to support binary files

reqwest::Response::text() UTF-8-decodes the body, which turns every
non-UTF-8 byte into a decode error or replacement character. Binary
files (images, archives, pickles) were either un-readable through the
mount or silently corrupted. Switch read_file to Result<Vec<u8>> via
resp.bytes(); drop the .into_bytes() at call sites. Integration test
spins a tiny TCP mock that serves [0x00, 0xFF, 0xFE, ...] and asserts
round-trip equality. Adds src/lib.rs so tests/ can use the crate.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

**GATE:** `cargo test` passes AND commit is in. Confirm: `git log --oneline -2` shows both fixes.

---

## Task 3: Graceful unmount on SIGINT/SIGTERM (data-loss fix)

**Root cause:** `install_signal_handler` in `main.rs:199` calls `std::process::exit(0)` on SIGINT/SIGTERM, bypassing `fuser::mount2`'s return path. That means `Filesystem::destroy` never runs, so dirty `WriteHandle` buffers are not flushed. Any in-progress write when the user Ctrl-C's the daemon is lost.

**Fix:** On signal, spawn `umount <mountpoint>` (using the same platform-dispatched `umount_argv`). The fuser kernel loop exits when the mount disappears, `mount2` returns, `destroy` runs, dirty handles flush. Keep a 5-second watchdog: if `mount2` hasn't returned by then (e.g., hung FS, stuck HTTP flush), `exit(1)` as a last resort so the process doesn't zombie forever.

**Files:**

- Modify: `crates/veda-fuse/src/main.rs:199-208` (signal handler) and call sites at line 174.

### Steps

- **Step 3.1: Refactor `umount_argv` → make it a reusable helper and add a test**

The function already exists (`main.rs:219-229`) and has tests. We just need to be sure the signal handler can call it. It takes `&str` and returns `Vec<String>`, which is already suitable. No changes needed here — verify by reading the existing tests.

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test --bin veda-fuse umount_argv
```

Expected: PASS — baseline confirmation.

- **Step 3.2: Add a test for the new helper `trigger_graceful_unmount`**

File: `crates/veda-fuse/src/main.rs` — add to the existing `#[cfg(test)] mod tests` block at the bottom.

```rust
    #[test]
    fn graceful_unmount_builds_expected_argv_for_nonexistent_mount() {
        // Unit test: we don't actually run the command (the mount doesn't exist),
        // we just verify the argv we'd pass to Command::new matches what umount_argv produces.
        let mp = "/tmp/veda-test-nonexistent-xyz123";
        let argv = umount_argv(mp);
        assert!(!argv.is_empty());
        assert_eq!(argv.last().unwrap(), mp);
    }
```

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test --bin veda-fuse graceful_unmount_builds_expected_argv_for_nonexistent_mount
```

Expected: PASS (trivially — the helper already works, we're just proving a test harness exists).

- **Step 3.3: Rewrite `install_signal_handler` to trigger umount instead of exit**

File: `crates/veda-fuse/src/main.rs`.

**(a) Change the signature** — it now takes the mountpoint:

Before (line 199-208):

```rust
fn install_signal_handler() {
    std::thread::spawn(|| {
        let mut signals = signal_hook::iterator::Signals::new([libc::SIGINT, libc::SIGTERM])
            .expect("failed to register signal handler");
        for sig in signals.forever() {
            eprintln!("\nveda: received signal {sig}, unmounting...");
            std::process::exit(0);
        }
    });
}
```

After:

```rust
fn install_signal_handler(mountpoint: String) {
    std::thread::spawn(move || {
        let mut signals = signal_hook::iterator::Signals::new([libc::SIGINT, libc::SIGTERM])
            .expect("failed to register signal handler");
        for sig in signals.forever() {
            eprintln!("\nveda: received signal {sig}, unmounting {mountpoint}...");
            // Trigger graceful unmount: this causes the FUSE kernel loop to exit,
            // fuser::mount2 returns, VedaFs::destroy runs, dirty write handles flush.
            let argv = umount_argv(&mountpoint);
            let _ = Command::new(&argv[0]).args(&argv[1..]).status();

            // Watchdog: if mount2 hasn't returned in 5s, force-exit so we don't
            // zombie forever on a wedged FS. Data may still be lost in that case.
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(5));
                eprintln!("veda: unmount watchdog timeout, forcing exit");
                std::process::exit(1);
            });

            // Don't exit from the signal handler thread itself — let the main
            // thread return from mount2 naturally so destroy() runs.
            return;
        }
    });
}
```

**(b) Update the call site** in `mount_and_serve` (line 174):

Before:

```rust
    install_signal_handler();
```

After:

```rust
    install_signal_handler(mountpoint.to_string());
```

- **Step 3.4: Build to confirm it compiles**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo build
```

Expected: SUCCESS, no warnings.

- **Step 3.5: Run the full test suite**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test
```

Expected: ALL PASS.

- **Step 3.6: Manual signal-handling smoke test**

Document for user (cannot be automated — needs a live mount):

```bash
# Terminal 1: start mount in foreground
cd /Users/konglingqiao/code/personal/veda
./crates/veda-fuse/target/release/veda-fuse mount \
    --server http://127.0.0.1:8080 --key $VEDA_KEY \
    /tmp/veda --foreground

# Terminal 2: begin a write, then immediately Ctrl-C terminal 1
echo "critical-data-$(date +%s)" > /tmp/veda/ctrltest.txt &
# In Terminal 1: press Ctrl-C
# Expected stderr in T1: "received signal 2, unmounting /tmp/veda..."
# then mount2 returns, "FUSE destroy" debug log, process exits 0.

# Verify data was flushed:
# Terminal 2: re-mount and read
./crates/veda-fuse/target/release/veda-fuse mount ... /tmp/veda --foreground &
cat /tmp/veda/ctrltest.txt
# Expected: prints "critical-data-..." NOT empty.
# Before the fix: file would be empty or 404 because destroy() never ran.
```

- **Step 3.7: Commit**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda
git add crates/veda-fuse/src/main.rs
git diff --cached crates/veda-fuse/src/main.rs | head -80
git commit -m "$(cat <<'EOF'
fix(fuse): graceful unmount on SIGINT/SIGTERM instead of exit(0)

The previous signal handler called std::process::exit(0) directly, which
bypasses fuser::mount2's return path, so Filesystem::destroy never runs
and dirty WriteHandle buffers are silently dropped. Any in-progress write
at Ctrl-C time was lost.

New handler invokes `umount <mountpoint>` (via the existing platform-aware
umount_argv), which causes the FUSE kernel loop to exit; mount2 returns;
destroy runs; dirty handles flush through flush_handle. A 5s watchdog
thread forces exit(1) as a fallback if the unmount wedges.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

**GATE:** `cargo test` passes AND commit is in. Before moving to Task 4, ask Joe to run the manual smoke test in Step 3.6 and confirm data survives Ctrl-C. This is a data-loss bug — manual verification is required.

---

## Task 4: SSE watcher invalidates `dir_cache` (bug_012)

**Root cause:** `VedaFs.dir_cache` is a plain `HashMap` on the struct. `SseWatcher::start` only receives `Arc<Mutex<InodeTable>>` and `Arc<Mutex<ReadCache>>`. Remote create/delete/rename events invalidate attr caches but not the parent directory's listing cache, so the new negative-lookup short-circuit in `lookup` (fs.rs:278) returns ENOENT for files that now exist on the server, for up to `attr_ttl` seconds.

**Fix:** Wrap `dir_cache` in `Arc<Mutex<HashMap<u64, DirCacheEntry>>>`. Expose `VedaFs::dir_cache()` getter alongside `inodes()` / `read_cache()`. Pass it into `SseWatcher::start`. In `invalidate_caches`, compute `parent_path(path)`, look up its ino via `InodeTable`, and remove it from `dir_cache`.

**Files:**

- Modify: `crates/veda-fuse/src/fs.rs` — struct field, constructor, all `dir_cache` access sites (lines 64, 180-207, 555, 580, 599, 624, 653-654), and add `pub fn dir_cache()`.
- Modify: `crates/veda-fuse/src/sse.rs` — add `dir_cache` parameter to `start` and `invalidate_caches`; invalidate parent's entry.
- Modify: `crates/veda-fuse/src/main.rs:171` — pass `vedafs.dir_cache()` into `SseWatcher::start`.

### Steps

- **Step 4.1: Add a failing test for `invalidate_caches` that exercises the new behavior**

File: `crates/veda-fuse/src/sse.rs` — append to the existing `mod tests` block at the bottom.

```rust
    use crate::fs::DirCacheMap;
    use std::collections::HashMap;

    #[test]
    fn invalidate_caches_removes_parent_dir_cache_entry() {
        let inodes = Arc::new(Mutex::new(InodeTable::new()));
        let read_cache = Arc::new(Mutex::new(ReadCache::new(1)));
        let dir_cache: DirCacheMap = Arc::new(Mutex::new(HashMap::new()));

        // Seed: /foo has ino 2, dir_cache[2] is populated as if a readdir just ran.
        let foo_ino = {
            let mut t = inodes.lock().unwrap();
            t.get_or_create_ino("/foo")
        };
        // Populate dir_cache with a dummy entry for foo_ino.
        {
            let mut dc = dir_cache.lock().unwrap();
            dc.insert(foo_ino, crate::fs::DirCacheEntry::empty_for_test());
        }

        // Simulate: SSE event "create /foo/new.txt"
        invalidate_caches("/foo/new.txt", &inodes, &read_cache, &dir_cache);

        // Expect: dir_cache entry for /foo (the parent) is gone.
        assert!(dir_cache.lock().unwrap().get(&foo_ino).is_none());
    }
```

Note: `DirCacheMap` and `DirCacheEntry::empty_for_test()` don't exist yet — that's why this test fails. We add them in Step 4.3.

- **Step 4.2: Run the test — confirm compile error**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test --lib sse::tests::invalidate_caches_removes_parent_dir_cache_entry 2>&1 | head -40
```

Expected: FAIL at compile — "no type `DirCacheMap` in module `fs`" or similar.

- **Step 4.3: Introduce `DirCacheMap` type alias and `DirCacheEntry::empty_for_test`**

File: `crates/veda-fuse/src/fs.rs`.

**(a) Make `DirCacheEntry` `pub(crate)`** and add a test-only ctor. Around line 51:

Before:

```rust
struct DirCacheEntry {
    entries: Vec<DirEntry>,
    child_names: HashSet<String>,
    fetched_at: Instant,
}
```

After:

```rust
pub(crate) struct DirCacheEntry {
    pub(crate) entries: Vec<DirEntry>,
    pub(crate) child_names: HashSet<String>,
    pub(crate) fetched_at: Instant,
}

impl DirCacheEntry {
    #[cfg(test)]
    pub(crate) fn empty_for_test() -> Self {
        Self {
            entries: Vec::new(),
            child_names: HashSet::new(),
            fetched_at: Instant::now(),
        }
    }
}
```

**(b) Add a type alias** right above the `VedaFs` struct declaration (around line 57):

```rust
pub type DirCacheMap = Arc<Mutex<HashMap<u64, DirCacheEntry>>>;
```

- **Step 4.4: Rewrap `dir_cache` in `Arc<Mutex<...>>`**

File: `crates/veda-fuse/src/fs.rs`.

**(a) Field declaration** (line 64):

Before:

```rust
    dir_cache: HashMap<u64, DirCacheEntry>,
```

After:

```rust
    dir_cache: DirCacheMap,
```

**(b) Constructor** (line 73):

Before:

```rust
        Self { client, inodes, config, read_cache, next_fh: 1, write_handles: HashMap::new(), dir_cache: HashMap::new(), notify_fd }
```

After:

```rust
        let dir_cache: DirCacheMap = Arc::new(Mutex::new(HashMap::new()));
        Self { client, inodes, config, read_cache, next_fh: 1, write_handles: HashMap::new(), dir_cache, notify_fd }
```

**(c) Add public getter** near the existing `inodes()` / `read_cache()` getters (after line 77):

```rust
    pub fn dir_cache(&self) -> DirCacheMap { self.dir_cache.clone() }
```

**(d) Update all `dir_cache` access sites** to go through the lock:

- `fetch_dir` (lines 180-191): convert all `self.dir_cache.get(&ino)` / `self.dir_cache.insert(...)` to `self.dir_cache.lock().unwrap().<op>`. Be careful to release the lock before calling `self.client.list_dir` (network I/O under a mutex = deadlock risk with SSE). Refactored version:

```rust
    fn fetch_dir(&mut self, ino: u64, path: &str) -> Result<Vec<DirEntry>, i32> {
        let stale = {
            let dc = self.dir_cache.lock().unwrap();
            match dc.get(&ino) {
                Some(c) => c.fetched_at.elapsed() >= self.config.attr_ttl,
                None => true,
            }
        };
        if stale {
            // NOTE: do network I/O outside the lock.
            let entries = self.client.list_dir(path).map_err(|ref e| Self::err_to_errno(e))?;
            let child_names: HashSet<String> = entries.iter().map(|de| de.name.clone()).collect();
            let mut dc = self.dir_cache.lock().unwrap();
            dc.insert(ino, DirCacheEntry { entries, child_names, fetched_at: Instant::now() });
        }
        let dc = self.dir_cache.lock().unwrap();
        Ok(dc.get(&ino).unwrap().entries.clone())
    }
```

- `dir_cache_has_child` (lines 195-203):

```rust
    fn dir_cache_has_child(&self, parent_ino: u64, name: &str) -> Option<bool> {
        let dc = self.dir_cache.lock().unwrap();
        dc.get(&parent_ino).and_then(|c| {
            if c.fetched_at.elapsed() < self.config.attr_ttl {
                Some(c.child_names.contains(name))
            } else {
                None
            }
        })
    }
```

- `invalidate_dir_cache` (lines 205-207):

```rust
    fn invalidate_dir_cache(&mut self, parent_ino: u64) {
        self.dir_cache.lock().unwrap().remove(&parent_ino);
    }
```

- **Step 4.5: Thread `dir_cache` through `SseWatcher::start` and `invalidate_caches`**

File: `crates/veda-fuse/src/sse.rs`.

**(a) Update import** (top of file):

```rust
use crate::cache::ReadCache;
use crate::fs::{parent_path, DirCacheMap};
use crate::inode::InodeTable;
```

**(b) `SseWatcher::start` signature** (line 29-34):

Before:

```rust
    pub fn start(
        server: &str,
        key: &str,
        inodes: Arc<Mutex<InodeTable>>,
        read_cache: Arc<Mutex<ReadCache>>,
    ) -> Self {
```

After:

```rust
    pub fn start(
        server: &str,
        key: &str,
        inodes: Arc<Mutex<InodeTable>>,
        read_cache: Arc<Mutex<ReadCache>>,
        dir_cache: DirCacheMap,
    ) -> Self {
```

**(c)** Inside the spawned thread, the closure captures `inodes`, `read_cache`; also capture `dir_cache`:

Before (line 87-91):

```rust
                                    invalidate_caches(
                                        &event.path,
                                        &inodes,
                                        &read_cache,
                                    );
```

After:

```rust
                                    invalidate_caches(
                                        &event.path,
                                        &inodes,
                                        &read_cache,
                                        &dir_cache,
                                    );
```

**(d) `invalidate_caches` signature and body** (lines 139-159):

Before:

```rust
fn invalidate_caches(
    path: &str,
    inodes: &Arc<Mutex<InodeTable>>,
    read_cache: &Arc<Mutex<ReadCache>>,
) {
    // Invalidate read cache for the file
    if let Ok(mut cache) = read_cache.lock() {
        cache.invalidate(path);
    }

    // Invalidate inode attr cache for the file and its parent directory
    if let Ok(mut table) = inodes.lock() {
        if let Some(ino) = table.get_ino(path) {
            table.invalidate(ino);
        }
        let parent = parent_path(path);
        if let Some(parent_ino) = table.get_ino(parent) {
            table.invalidate(parent_ino);
        }
    }
}
```

After:

```rust
fn invalidate_caches(
    path: &str,
    inodes: &Arc<Mutex<InodeTable>>,
    read_cache: &Arc<Mutex<ReadCache>>,
    dir_cache: &DirCacheMap,
) {
    // Invalidate read cache for the file itself.
    if let Ok(mut cache) = read_cache.lock() {
        cache.invalidate(path);
    }

    // Invalidate inode attr cache for the file and its parent directory,
    // and drop the parent's directory listing cache so the next lookup / readdir
    // re-fetches from the server. Without this, negative-lookup short-circuit
    // in VedaFs::lookup returns ENOENT for files created remotely, for up to
    // attr_ttl seconds.
    let parent = parent_path(path);
    let parent_ino = if let Ok(mut table) = inodes.lock() {
        if let Some(ino) = table.get_ino(path) {
            table.invalidate(ino);
        }
        let pi = table.get_ino(parent);
        if let Some(ino) = pi {
            table.invalidate(ino);
        }
        pi
    } else {
        None
    };
    if let Some(pi) = parent_ino {
        if let Ok(mut dc) = dir_cache.lock() {
            dc.remove(&pi);
        }
    }
}
```

- **Step 4.6: Update the call site in `main.rs`**

File: `crates/veda-fuse/src/main.rs` — lines 169-172.

Before:

```rust
    let mut watcher = sse::SseWatcher::start(
        server, key,
        vedafs.inodes(), vedafs.read_cache(),
    );
```

After:

```rust
    let mut watcher = sse::SseWatcher::start(
        server, key,
        vedafs.inodes(), vedafs.read_cache(), vedafs.dir_cache(),
    );
```

- **Step 4.7: Run the target test — confirm it passes**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test --lib sse::tests::invalidate_caches_removes_parent_dir_cache_entry
```

Expected: PASS.

- **Step 4.8: Add one more sse.rs test — sanity that read_cache + inode invalidation still work**

File: `crates/veda-fuse/src/sse.rs` — append to the `mod tests` block.

```rust
    #[test]
    fn invalidate_caches_still_invalidates_read_and_attr_caches() {
        let inodes = Arc::new(Mutex::new(InodeTable::new()));
        let read_cache = Arc::new(Mutex::new(ReadCache::new(1)));
        let dir_cache: DirCacheMap = Arc::new(Mutex::new(HashMap::new()));

        let file_ino = {
            let mut t = inodes.lock().unwrap();
            let ino = t.get_or_create_ino("/foo/bar.txt");
            // Also materialize parent so parent_ino lookup succeeds.
            t.get_or_create_ino("/foo");
            // Seed a cached attr we can observe.
            let dummy_attr = fuser::FileAttr {
                ino, size: 100,
                blocks: 1,
                atime: std::time::SystemTime::UNIX_EPOCH,
                mtime: std::time::SystemTime::UNIX_EPOCH,
                ctime: std::time::SystemTime::UNIX_EPOCH,
                crtime: std::time::SystemTime::UNIX_EPOCH,
                kind: fuser::FileType::RegularFile, perm: 0o644, nlink: 1,
                uid: 0, gid: 0, rdev: 0, blksize: 512, flags: 0,
            };
            t.set_cached_attr(ino, dummy_attr);
            ino
        };
        read_cache.lock().unwrap().put("/foo/bar.txt", b"hello".to_vec());

        invalidate_caches("/foo/bar.txt", &inodes, &read_cache, &dir_cache);

        // Attr cache cleared
        assert!(inodes.lock().unwrap().get_cached_attr(file_ino).is_none());
        // Read cache cleared
        assert!(read_cache.lock().unwrap().get("/foo/bar.txt").is_none());
    }
```

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test --lib sse::tests
```

Expected: ALL sse tests pass (new + existing parent_path and sse_event tests).

- **Step 4.9: Full test suite sanity**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test
```

Expected: ALL PASS, no regressions from tasks 1-3.

- **Step 4.10: Manual cross-client SSE test**

Document for user (needs two mounts or a mount + direct API call):

```bash
# Terminal A: mount
./crates/veda-fuse/target/release/veda-fuse mount ... /tmp/vedaA --foreground &

# Warm the dir_cache
ls /tmp/vedaA/somedir

# Terminal B: create a file via the API directly (simulating another client)
curl -X PUT -H "Authorization: Bearer $VEDA_KEY" \
    --data "hello" "http://127.0.0.1:8080/v1/fs/somedir/newfile.txt"

# Wait ~1s for SSE event to propagate.
sleep 1

# Terminal A: the file should now be visible (< attr_ttl seconds)
ls /tmp/vedaA/somedir/newfile.txt
# Expected: "hello" file appears immediately.
# Before fix: ENOENT for up to 5s (attr_ttl).
```

- **Step 4.11: Commit**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda
git add crates/veda-fuse/src/fs.rs crates/veda-fuse/src/sse.rs crates/veda-fuse/src/main.rs
git diff --cached --stat
git commit -m "$(cat <<'EOF'
fix(fuse): SSE watcher invalidates parent dir_cache on remote events

VedaFs.dir_cache lived as a plain HashMap inaccessible to SseWatcher,
so remote create/rename events cleared attr/read caches but left the
parent's listing cache (with its child_names HashSet) intact. The new
negative-lookup short-circuit in lookup() then returned ENOENT for
files created on another client for up to attr_ttl seconds -- defeating
the whole point of the SSE watcher.

Wrap dir_cache in Arc<Mutex<...>>, expose VedaFs::dir_cache(), thread
it into SseWatcher::start and invalidate_caches. All local dir_cache
accesses now go through the lock; fetch_dir releases the lock before
network I/O to avoid starving SSE.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

**GATE:** `cargo test` passes AND commit is in. All 4 tasks done.

---

## Wrap-up

- **Step W.1: Full test summary**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda/crates/veda-fuse && cargo test 2>&1 | tail -20
```

Expected: summary shows `test result: ok. N passed; 0 failed` for each of: `--lib`, `--bin veda-fuse`, `--test binary_roundtrip`.

- **Step W.2: Git log review**

Run:

```bash
cd /Users/konglingqiao/code/personal/veda && git log --oneline -5
```

Expected: 4 new commits on top of `8aae222`, each a single task's fix.

- **Step W.3: Report to Joe**

Summarize in Chinese: tasks completed, test counts (before: N, after: N+M), manual smoke-tests pending for Tasks 1/2/3/4, recommended next items from the remaining list.