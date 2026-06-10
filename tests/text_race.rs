//! Regression: the /out/text delivery race that dropped an utterance when the
//! subscriber was not connected at send time.
//!
//! The field symptom (journal-confirmed): the reactor produced a reply
//! ("Hey! What's up?") and emitted its chunks, but the web client's GET
//! GET /out/text re-subscribed ~150ms too late. The old `tokio::broadcast` delivered
//! nothing to a receiver created after the send, so "send hi, nothing
//! responds". The per-scene `TextBus` buffers utterances, so a late GET still
//! drains the pending one.

use std::time::Duration;

use hi_agent::memory::Memory;
use hi_agent::server::{self, ServerSeams, TextBus};
use hi_agent::types::Scene;
use tempfile::tempdir;
use tokio::net::TcpListener;

async fn spawn_server() -> (String, tempfile::TempDir, ServerSeams) {
    let dir = tempdir().expect("tempdir");
    let memory = Memory::open(dir.path()).await.expect("memory");
    let observatory = hi_agent::observatory::Observatory::new(None, hi_agent::reactor::swap_budget_chars());
    let (router, seams) = server::build(
        memory,
        dir.path().to_path_buf(),
        observatory,
        hi_agent::acp::AcpTap::new(),
        hi_agent::reactor::ToolRegistry::new(),
        hi_agent::reactor::InterruptRegistry::new(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), dir, seams)
}

async fn emit_utterance(bus: &TextBus, scene: &str, chunks: &[&str]) {
    let scene = Scene(scene.to_string());
    for c in chunks {
        bus.push_chunk(&scene, c.to_string()).await;
    }
    bus.end_utterance(&scene).await;
}

async fn get_out_text(base: &str, scene: &str, budget: Duration) -> Result<String, ()> {
    let client = reqwest::Client::new();
    tokio::time::timeout(budget, async {
        client
            .get(format!("{base}/api/out/text"))
            .header("X-HI-Scene", scene)
            .send()
            .await
            .expect("send")
            .text()
            .await
            .expect("body")
    })
    .await
    .map_err(|_| ())
}

/// The original bug: produce the whole reply, *then* subscribe. The buffered
/// utterance must still be delivered.
#[tokio::test]
async fn late_subscriber_still_gets_the_utterance() {
    let (base, _dir, seams) = spawn_server().await;

    emit_utterance(&seams.text_bus, "web@local", &["Hey! What", "'s up?"]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let body = get_out_text(&base, "web@local", Duration::from_millis(500))
        .await
        .expect("late GET should receive the buffered utterance, not hang");
    assert_eq!(body, "Hey! What's up?");
}

/// A subscriber connected *before* the reply still streams it (live path).
#[tokio::test]
async fn connected_subscriber_streams_live() {
    let (base, _dir, seams) = spawn_server().await;

    let bus = seams.text_bus.clone();
    let base2 = base.clone();
    let reader = tokio::spawn(async move {
        get_out_text(&base2, "web@local", Duration::from_millis(800)).await
    });

    // Let the GET subscribe, then emit.
    tokio::time::sleep(Duration::from_millis(80)).await;
    emit_utterance(&bus, "web@local", &["live ", "stream"]).await;

    let body = reader.await.expect("join").expect("should not hang");
    assert_eq!(body, "live stream");
}

/// Two sequential utterances are delivered one-per-GET, in order — no replay of
/// the already-drained one.
#[tokio::test]
async fn sequential_gets_drain_fifo() {
    let (base, _dir, seams) = spawn_server().await;

    emit_utterance(&seams.text_bus, "web@local", &["first"]).await;
    emit_utterance(&seams.text_bus, "web@local", &["second"]).await;

    let a = get_out_text(&base, "web@local", Duration::from_millis(500))
        .await
        .expect("first GET");
    assert_eq!(a, "first");

    let b = get_out_text(&base, "web@local", Duration::from_millis(500))
        .await
        .expect("second GET");
    assert_eq!(b, "second");
}

/// Utterances are keyed by scene: one scene's reply never leaks to another.
#[tokio::test]
async fn utterances_are_per_scene() {
    let (base, _dir, seams) = spawn_server().await;

    emit_utterance(&seams.text_bus, "alice@phone", &["for alice"]).await;
    tokio::time::sleep(Duration::from_millis(30)).await;

    // Bob has nothing buffered → his GET must time out, not steal alice's.
    let bob = get_out_text(&base, "bob@desktop", Duration::from_millis(250)).await;
    assert!(bob.is_err(), "bob should get nothing; got {bob:?}");

    let alice = get_out_text(&base, "alice@phone", Duration::from_millis(500))
        .await
        .expect("alice GET");
    assert_eq!(alice, "for alice");
}

/// GET without X-HI-Scene has no scene to drain → 400, not a silent hang.
#[tokio::test]
async fn get_without_scene_is_bad_request() {
    let (base, _dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/out/text"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}
