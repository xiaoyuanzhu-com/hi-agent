//! Smoke test for the HTTP route surface.
//!
//! Builds the axum router via [`hi_agent::server::build`] directly. The
//! reactor seams are returned alongside so the test holds them past the
//! handlers' send into `inbound` — otherwise the receiver drops and
//! POST /api/in/text returns 503.

use std::time::Duration;

use hi_agent::memory::Memory;
use hi_agent::server::{self, ServerSeams};
use hi_agent::types::JournalEntry;
use tempfile::tempdir;
use tokio::net::TcpListener;

async fn spawn_server() -> (String, tempfile::TempDir, ServerSeams) {
    let dir = tempdir().expect("tempdir");
    let memory = Memory::open(dir.path()).await.expect("memory");
    let observatory = hi_agent::observatory::Observatory::new(None, hi_agent::reactor::swap_budget_chars());
    let (router, seams) = server::build(memory, dir.path().to_path_buf(), observatory);

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
        .post(format!("{base}/api/in/text"))
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
        .post(format!("{base}/api/in/text"))
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
        .post(format!("{base}/api/in/vision"))
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
    // STT is unconfigured, which the test forces by never calling
    // capabilities::init_from_env — the STT global stays uninitialized, so
    // stt::available() is false.
    let (base, _dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    for ch in ["touch", "smell", "taste"] {
        let resp = client
            .post(format!("{base}/api/in/{ch}"))
            .header("X-HI-Scene", "alice@phone")
            .body("...")
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NOT_IMPLEMENTED,
            "POST /api/in/{ch} should be 501"
        );
    }

    // POST /api/in/audio with no STT configured: 501 with the new (capability-gated)
    // body.
    let resp = client
        .post(format!("{base}/api/in/audio"))
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

#[tokio::test]
async fn overlay_round_trips_post_to_get() {
    // The overlay is a non-voice output channel any local party can write. A
    // GET subscriber opens a continuous NDJSON stream; a POST frame to the same
    // scene shows up as the next line.
    let (base, _dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    // Open the stream first so the subscriber exists before we POST.
    let mut resp = client
        .get(format!("{base}/api/out/overlay"))
        .header("X-HI-Scene", "alice@phone")
        .send()
        .await
        .expect("send GET");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    assert!(ct.starts_with("application/x-ndjson"), "got {ct:?}");

    tokio::time::sleep(Duration::from_millis(30)).await;
    let payload = r#"{"rects":[{"x":1,"y":2,"w":3,"h":4}]}"#;
    let post = client
        .post(format!("{base}/api/out/overlay"))
        .header("X-HI-Scene", "alice@phone")
        .header("Content-Type", "application/json")
        .body(payload)
        .send()
        .await
        .expect("send POST");
    assert_eq!(post.status(), reqwest::StatusCode::ACCEPTED);

    let chunk = tokio::time::timeout(Duration::from_secs(2), resp.chunk())
        .await
        .expect("overlay chunk within timeout")
        .expect("chunk read")
        .expect("a chunk present");
    let line = String::from_utf8(chunk.to_vec()).expect("utf8");
    assert_eq!(line.trim_end(), payload, "the NDJSON line is the posted payload");
}

#[tokio::test]
async fn overlay_scene_mismatch_receives_nothing() {
    let (base, _dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    let mut resp = client
        .get(format!("{base}/api/out/overlay"))
        .header("X-HI-Scene", "alice@phone")
        .send()
        .await
        .expect("send GET");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    tokio::time::sleep(Duration::from_millis(30)).await;
    let post = client
        .post(format!("{base}/api/out/overlay"))
        .header("X-HI-Scene", "bob@tv") // different scene
        .body("{}")
        .send()
        .await
        .expect("send POST");
    assert_eq!(post.status(), reqwest::StatusCode::ACCEPTED);

    // Alice's stream must stay silent — reading a chunk times out.
    let res = tokio::time::timeout(Duration::from_millis(300), resp.chunk()).await;
    assert!(
        res.is_err(),
        "a frame for a different scene must not reach this subscriber"
    );
}

#[tokio::test]
async fn vision_get_receives_posted_frame() {
    // GET /api/in/vision is the read side of the input channel: one frame per
    // scene-filtered long-poll response, carrying the frame's Content-Type.
    let (base, _dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    // The GET blocks until a frame arrives, so drive it from a task and POST
    // after it has had time to subscribe.
    let get_base = base.clone();
    let getter = tokio::spawn(async move {
        let c = reqwest::Client::new();
        let resp = c
            .get(format!("{get_base}/api/in/vision"))
            .header("X-HI-Scene", "alice@phone")
            .send()
            .await
            .expect("send GET");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap_or("").to_string())
            .unwrap_or_default();
        let body = resp.bytes().await.expect("body");
        (ct, body)
    });

    tokio::time::sleep(Duration::from_millis(80)).await;
    let frame = vec![0xFFu8, 0xD8, 0xFF, 0xD9];
    let post = client
        .post(format!("{base}/api/in/vision"))
        .header("X-HI-Scene", "alice@phone")
        .header("Content-Type", "image/jpeg")
        .body(frame.clone())
        .send()
        .await
        .expect("send POST");
    assert_eq!(post.status(), reqwest::StatusCode::ACCEPTED);

    let (ct, body) = tokio::time::timeout(Duration::from_secs(2), getter)
        .await
        .expect("vision GET within timeout")
        .expect("getter task");
    assert!(ct.starts_with("image/jpeg"), "content-type echoes frame mime, got {ct:?}");
    assert_eq!(body.as_ref(), frame.as_slice(), "frame bytes round-trip");
}
