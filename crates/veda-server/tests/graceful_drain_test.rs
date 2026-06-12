//! Drain-window test against the REAL veda-server binary.
//!
//! Spawns the compiled binary with `config/test.toml` (real MySQL/Milvus,
//! same as the other integration tests), sends an actual SIGTERM, and
//! verifies the rolling-deploy contract end to end:
//!
//!   1. before SIGTERM: /v1/ready is 200
//!   2. during the drain window: /v1/ready flips to 503 "draining",
//!      while /healthz (and the listener generally) keeps serving
//!   3. after the window: the process exits cleanly (code 0)
//!
//! Run with:
//! ```
//! cargo test -p veda-server --test graceful_drain_test -- --ignored --nocapture
//! ```

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const LISTEN: &str = "127.0.0.1:3911";
const DRAIN_SECS: u64 = 3;
/// Startup budget: schema bootstrap + Milvus init_collections on the shared
/// test storage can take a while when the box is busy.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// Exit budget after SIGTERM: drain window + in-flight grace + worker
/// finishing its current poll/batch.
const EXIT_TIMEOUT: Duration = Duration::from_secs(20);

struct ServerProc(Child);

impl Drop for ServerProc {
    fn drop(&mut self) {
        // Belt and braces: never leak the server if an assert fires.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_server() -> ServerProc {
    let config = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/test.toml");
    let child = Command::new(env!("CARGO_BIN_EXE_veda-server"))
        .arg(config)
        .env("VEDA_LISTEN", LISTEN)
        .env("VEDA_DRAIN_SECS", DRAIN_SECS.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn veda-server binary");
    ServerProc(child)
}

fn sigterm(pid: u32) {
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -TERM failed");
}

async fn get(client: &reqwest::Client, path: &str) -> Option<(u16, String)> {
    let url = format!("http://{LISTEN}{path}");
    let resp = client.get(&url).send().await.ok()?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Some((status, body))
}

#[tokio::test]
#[ignore]
async fn drain_window_keeps_serving_and_flips_ready() {
    let mut server = spawn_server();
    let pid = server.0.id();
    // .no_proxy(): the target is 127.0.0.1 — a system/env proxy (common on
    // dev macs) would otherwise swallow every request and fail startup polling.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
        .unwrap();

    // 1. Wait until fully up: /v1/ready 200 means MySQL + Milvus pinged ok.
    let start = Instant::now();
    loop {
        if let Some((200, _)) = get(&client, "/v1/ready").await {
            break;
        }
        assert!(
            start.elapsed() < STARTUP_TIMEOUT,
            "server did not become ready within {STARTUP_TIMEOUT:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // 2. SIGTERM starts the drain window.
    let term_at = Instant::now();
    sigterm(pid);

    // /v1/ready must flip to 503 "draining" near-instantly (poll ≤ 1s so
    // we stay well inside the 3s window for the follow-up asserts).
    loop {
        let (status, body) = get(&client, "/v1/ready")
            .await
            .expect("listener must still accept during drain");
        if status == 503 {
            assert!(
                body.contains("draining"),
                "expected draining status, got: {body}"
            );
            break;
        }
        assert!(
            term_at.elapsed() < Duration::from_secs(1),
            "/v1/ready did not flip to 503 within 1s of SIGTERM"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 3. Mid-drain the node still serves traffic — this is the whole point:
    //    the LB needs time to pull us while requests keep succeeding.
    let (status, body) = get(&client, "/healthz")
        .await
        .expect("/healthz must be reachable during drain");
    assert_eq!(status, 200, "healthz during drain: {body}");

    // 4. After the window the process exits on its own, cleanly.
    let exit_start = Instant::now();
    let exit = loop {
        if let Some(code) = server.0.try_wait().expect("try_wait") {
            break code;
        }
        assert!(
            exit_start.elapsed() < EXIT_TIMEOUT,
            "server did not exit within {EXIT_TIMEOUT:?} after SIGTERM"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(exit.success(), "expected clean exit, got {exit:?}");
    // Shutdown only begins after the drain sleep, so total time since
    // SIGTERM must cover the window (1s tolerance for timer rounding).
    assert!(
        term_at.elapsed() >= Duration::from_secs(DRAIN_SECS - 1),
        "exited before the drain window elapsed"
    );
}
