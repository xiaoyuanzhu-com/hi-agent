//! The raw store writes the sealed channel-first layout and merges channels on
//! read. Exercises the `Journal`/`store_blob` API directly — the audio HTTP path
//! needs STT configured, which these tests deliberately avoid.

use chrono::{DateTime, TimeZone, Utc};
use hi_agent::mind::memory::layout::{self, MediaSlot};
use hi_agent::mind::memory::media::store_blob;
use hi_agent::mind::memory::Memory;
use hi_agent::types::{Channel, JournalEntry, Media, Origin, Scene};
use tempfile::tempdir;

fn signal_in(id: &str, channel: Channel, ts: DateTime<Utc>, body: &str, media: Option<Media>) -> JournalEntry {
    JournalEntry::SignalIn {
        id: id.into(),
        ts,
        channel,
        scene: Scene("alice@phone".into()),
        body: body.into(),
        stream: None,
        media,
        origin: Some(Origin::Human),
    }
}

fn signal_out(id: &str, channel: Channel, ts: DateTime<Utc>, body: &str) -> JournalEntry {
    JournalEntry::SignalOut {
        id: id.into(),
        ts,
        channel,
        scene: Scene("alice@phone".into()),
        body: body.into(),
        media: None,
        origin: Some(Origin::Reactor),
    }
}

fn id_of(e: &JournalEntry) -> &str {
    match e {
        JournalEntry::SignalIn { id, .. } | JournalEntry::SignalOut { id, .. } => id,
    }
}

#[tokio::test]
async fn appends_route_by_channel_and_recent_merges_in_ts_id_order() {
    let dir = tempdir().expect("tempdir");
    let mem = Memory::open(dir.path()).await.expect("memory");
    let scene = Scene("alice@phone".into());

    let t_typed = Utc.with_ymd_and_hms(2026, 6, 13, 10, 0, 0).unwrap();
    let t_pair = Utc.with_ymd_and_hms(2026, 6, 13, 10, 5, 0).unwrap();

    // Append out of order and across channels; one ts is shared so the uuidv7
    // `id` tiebreak is exercised (id "0002" before "0003" at the same instant).
    let audio_media = Media { file: "10/05-00.mp3".into(), mime: "audio/mpeg".into(), duration_ms: None, width: None, height: None };
    mem.journal.append(signal_in("0003", Channel::Audio, t_pair, "spoken", Some(audio_media))).await.unwrap();
    mem.journal.append(signal_out("0002", Channel::Text, t_pair, "reply")).await.unwrap();
    mem.journal.append(signal_in("0001", Channel::Text, t_typed, "typed", None)).await.unwrap();

    // Logs land under per-channel, per-day folders named for the channel.
    let text_log = layout::channel_log_path(mem.data_dir(), &scene, Channel::Text, t_typed);
    let audio_log = layout::channel_log_path(mem.data_dir(), &scene, Channel::Audio, t_pair);
    assert!(text_log.ends_with("text/2026-06-13/text.jsonl"), "text log at {text_log:?}");
    assert!(audio_log.ends_with("audio/2026-06-13/audio.jsonl"), "audio log at {audio_log:?}");
    assert!(text_log.exists() && audio_log.exists(), "both channel logs written");

    // recent() merges all channels by (ts, id): typed first, then the same-ts
    // pair in id order (text out 0002 before audio in 0003).
    let since = Utc.with_ymd_and_hms(2026, 6, 13, 9, 0, 0).unwrap();
    let got = mem.journal.recent(Some(&scene), since, 10).await.unwrap();
    let ids: Vec<&str> = got.iter().map(id_of).collect();
    assert_eq!(ids, ["0001", "0002", "0003"], "merged in (ts,id) order");
}

#[tokio::test]
async fn store_blob_writes_relative_grid_path() {
    let dir = tempdir().expect("tempdir");
    let mem = Memory::open(dir.path()).await.expect("memory");
    let scene = Scene("alice@phone".into());
    let ts = Utc.with_ymd_and_hms(2026, 6, 13, 9, 16, 45).unwrap();

    let rel = store_blob(mem.data_dir(), &scene, Channel::Audio, ts, MediaSlot::InputOneOff, "mp3", b"xxxx")
        .await
        .unwrap();
    // A one-off input blob is `<HH>/<MM>-<SS>.<ext>`, relative to the channel-day.
    assert_eq!(rel, "09/16-45.mp3");
    let abs = layout::channel_day_dir(mem.data_dir(), &scene, Channel::Audio, ts).join(&rel);
    assert!(abs.exists(), "blob written at {abs:?}");
}
