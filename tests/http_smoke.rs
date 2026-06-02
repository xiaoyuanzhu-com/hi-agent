//! Smoke test for the HTTP route surface.
//!
//! Builds the axum router via [`hi_agent::server::build`] directly. The
//! reactor seams are returned alongside so the test holds them past the
//! handlers' send into `inbound` — otherwise the receiver drops and
//! POST /thought returns 503.

use std::time::Duration;

use hi_agent::memory::Memory;
use hi_agent::server::{self, ServerSeams};
use hi_agent::types::JournalEntry;
use tempfile::tempdir;
use tokio::net::TcpListener;

async fn spawn_server() -> (String, tempfile::TempDir, ServerSeams) {
    let dir = tempdir().expect("tempdir");
    let memory = Memory::open(dir.path()).await.expect("memory");
    let (router, seams) = server::build(memory, dir.path().to_path_buf(), None);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    tokio::time::sleep(Duration::from_millis(20)).await;

    (format!("http://{addr}"), dir, seams)
}

/// Read every line of `journal.jsonl` into typed entries.
async fn read_journal(dir: &std::path::Path) -> Vec<JournalEntry> {
    let path = dir.join("journal.jsonl");
    let contents = match tokio::fs::read_to_string(&path).await {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => panic!("read journal: {err}"),
    };
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("decode journal entry"))
        .collect()
}

#[tokio::test]
async fn post_thought_accepts_and_journals() {
    let (base, dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/thought"))
        .header("X-HI-Scene", "alice@phone")
        .body("hi")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);

    tokio::time::sleep(Duration::from_millis(50)).await;
    let entries = read_journal(dir.path()).await;
    assert_eq!(entries.len(), 1, "expected one journal entry, got {entries:?}");
    match &entries[0] {
        JournalEntry::SignalIn { scene, body, .. } => {
            assert_eq!(scene.0, "alice@phone");
            assert_eq!(body, "hi");
        }
        other => panic!("expected SignalIn, got {other:?}"),
    }
}

#[tokio::test]
async fn post_thought_without_scene_header_is_anonymous() {
    // X-HI-Scene is "recommended" per spec; we default missing/empty to a
    // stable anonymous scene so per-scene routing still has a key.
    let (base, dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/thought"))
        .body("hi")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);

    tokio::time::sleep(Duration::from_millis(50)).await;
    let entries = read_journal(dir.path()).await;
    assert_eq!(entries.len(), 1);
    match &entries[0] {
        JournalEntry::SignalIn { scene, .. } => assert_eq!(scene.0, "anonymous"),
        other => panic!("expected SignalIn, got {other:?}"),
    }
}

#[tokio::test]
async fn post_vision_accepts_and_persists_without_journaling() {
    // Vision is a live continuous channel: the frame is accepted (202) and
    // persisted, but deliberately NOT journaled or dispatched yet — the mind
    // has no way to perceive an image, and journaling every frame would flood
    // the cognition snapshot. So the journal stays empty after a frame lands.
    let (base, dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/vision"))
        .header("X-HI-Scene", "alice@phone")
        .header("Content-Type", "image/jpeg")
        .body(vec![0xFFu8, 0xD8, 0xFF, 0xD9]) // minimal JPEG-ish bytes
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);

    tokio::time::sleep(Duration::from_millis(50)).await;
    let entries = read_journal(dir.path()).await;
    assert!(entries.is_empty(), "vision must not journal yet, got {entries:?}");

    // The frame should have been written under media/image/in/.
    let img_dir = dir.path().join("media").join("image").join("in");
    let count = std::fs::read_dir(&img_dir)
        .map(|rd| rd.count())
        .unwrap_or(0);
    assert_eq!(count, 1, "expected one persisted frame under {img_dir:?}");
}

#[tokio::test]
async fn all_sensory_stubs_return_501() {
    // touch/smell/taste are still 501 in v0. /audio returns 501 only when
    // STT is unconfigured, which the test fixture forces with stt: None.
    let (base, _dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    for ch in ["touch", "smell", "taste"] {
        let resp = client
            .post(format!("{base}/api/{ch}"))
            .header("X-HI-Scene", "alice@phone")
            .body("...")
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NOT_IMPLEMENTED,
            "POST /api/{ch} should be 501"
        );
    }

    // POST /api/audio with no STT configured: 501 with the new (capability-gated)
    // body.
    let resp = client
        .post(format!("{base}/api/audio"))
        .header("X-HI-Scene", "alice@phone")
        .header("Content-Type", "audio/wav")
        .body(vec![0u8; 16])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_IMPLEMENTED);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("STT_PROVIDER"),
        "501 body should explain the capability gate, got: {body}"
    );
}

#[tokio::test]
async fn homepage_returns_html() {
    let (base, _dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    let resp = client.get(format!("{base}/")).send().await.expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    assert!(
        ct.starts_with("text/html"),
        "expected text/html, got {ct:?}"
    );
    let body = resp.text().await.expect("body");
    assert!(body.contains("<html") || body.contains("<!doctype"));
}
