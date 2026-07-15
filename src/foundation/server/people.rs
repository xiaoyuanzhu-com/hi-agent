//! People-review endpoints — the "认识的人" surface's backend.
//!
//! A single web view ([`_builtin/people-review`](crate::mind::views)) lets the user
//! review who the agent has stored (faces + voices), name the unknown ones, pull a
//! clip that doesn't belong out of a cluster, and — when a cluster is too mixed to
//! fix clip-by-clip — auto-regroup it. Every action maps to a `people_vectors`
//! primitive:
//!
//! - `GET  /api/people` — list every cluster with its per-modality clip stems.
//! - `GET  /api/people/{subject}/{modality}/{stem}` — serve one crop/clip.
//! - `POST /api/people/name` — name / rename a cluster. Renaming onto an existing
//!   name *is* the merge (mirrors `reflection_name_person`/`merge_people`).
//! - `POST /api/people/eject` — pull one clip out into its own fresh cluster.
//! - `POST /api/people/split/preview` — propose an auto-regrouping (moves nothing).
//! - `POST /api/people/split/apply` — commit an accepted regrouping.
//!
//! The people store is global (not scene-scoped), so these take no `X-HI-Scene`.
//! Reads are cheap directory walks; writes are atomic file moves in `people_vectors`.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::foundation::server::AppState;
use crate::mind::memory::facets;
use crate::mind::memory::people_vectors::{self, Modality};

/// Parse the `{modality}` path segment. Only `face`/`voice` are valid.
fn parse_modality(s: &str) -> Option<Modality> {
    match s {
        "face" => Some(Modality::Face),
        "voice" => Some(Modality::Voice),
        _ => None,
    }
}

/// A single clip's uuid stem must be one safe path segment (letters/digits only, as
/// minted). Guards the media route against traversal even though the subject is
/// slugged separately.
fn safe_stem(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

// ── list ──────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct PersonDto {
    subject: String,
    named: bool,
    /// True when the cluster spans genuinely separate occasions — a hint the review
    /// view can use to flag "might be more than one person" softly.
    recurring: bool,
    face: Vec<String>,
    voice: Vec<String>,
}

/// `GET /api/people` — every cluster with its per-modality clip stems, named first.
pub async fn get_people(State(state): State<Arc<AppState>>) -> Response {
    match people_vectors::list_clusters(&state.data_dir).await {
        Ok(list) => {
            let people: Vec<PersonDto> = list
                .into_iter()
                .map(|c| PersonDto {
                    subject: c.subject,
                    named: c.named,
                    recurring: c.occasions >= 2,
                    face: c.face_stems,
                    voice: c.voice_stems,
                })
                .collect();
            Json(serde_json::json!({ "people": people })).into_response()
        }
        Err(e) => err(&e.to_string()),
    }
}

// ── media ─────────────────────────────────────────────────────────────────────

/// `GET /api/people/{subject}/{modality}/{stem}` — serve one face crop or voice clip.
/// `subject` is slugged (it may be an arbitrary name); `modality`/`stem` are checked.
pub async fn get_clip(
    State(state): State<Arc<AppState>>,
    Path((subject, modality, stem)): Path<(String, String, String)>,
) -> Response {
    let Some(modality) = parse_modality(&modality) else {
        return (StatusCode::NOT_FOUND, "no such modality\n").into_response();
    };
    if !safe_stem(&stem) {
        return (StatusCode::NOT_FOUND, "bad stem\n").into_response();
    }
    let subj = facets::slug(&subject);
    if subj.is_empty() {
        return (StatusCode::NOT_FOUND, "bad subject\n").into_response();
    }
    match people_vectors::clip_media_path(&state.data_dir, &subj, modality, &stem).await {
        Ok(Some(path)) => match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let ct = media_content_type(&path);
                let mut resp = Response::new(Body::from(bytes));
                resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static(ct));
                // Content-addressed by uuid stem → immutable.
                resp.headers_mut()
                    .insert(CACHE_CONTROL, HeaderValue::from_static("private, max-age=86400"));
                resp
            }
            Err(_) => (StatusCode::NOT_FOUND, "not found\n").into_response(),
        },
        Ok(None) => (StatusCode::NOT_FOUND, "not found\n").into_response(),
        Err(e) => err(&e.to_string()),
    }
}

/// Content-type for a clip by extension (face crops, voice turns).
fn media_content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "m4a" | "mp4" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg",
        _ => "application/octet-stream",
    }
}

// ── name / rename (+ merge) ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct NameReq {
    /// The cluster to name (its current id, or existing name).
    subject: String,
    /// The name to give it. If a cluster already has this name, the two merge.
    name: String,
}

/// `POST /api/people/name` — name or rename a cluster. Renaming onto a name that
/// already exists merges the two (this is the only merge path — same `rename` the
/// reflection tools call). Reports whether a merge happened so the view can word it.
pub async fn post_name(State(state): State<Arc<AppState>>, Json(req): Json<NameReq>) -> Response {
    let (from, to) = (req.subject.trim(), req.name.trim());
    if from.is_empty() || to.is_empty() {
        return err("subject and name are required");
    }
    let merged = people_vectors::list_clusters(&state.data_dir)
        .await
        .map(|list| list.iter().any(|c| c.subject == facets::slug(to)))
        .unwrap_or(false);
    match people_vectors::rename(&state.data_dir, from, to).await {
        Ok(()) => Json(serde_json::json!({ "ok": true, "merged": merged, "name": to })).into_response(),
        Err(e) => err(&e.to_string()),
    }
}

// ── eject one clip ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct EjectReq {
    subject: String,
    modality: String,
    stem: String,
}

/// `POST /api/people/eject` — pull one clip out of a cluster into its own fresh
/// unnamed cluster ("this one isn't them"). Returns the new cluster's id.
pub async fn post_eject(State(state): State<Arc<AppState>>, Json(req): Json<EjectReq>) -> Response {
    let Some(modality) = parse_modality(&req.modality) else {
        return err("modality must be face or voice");
    };
    if !safe_stem(&req.stem) {
        return err("bad stem");
    }
    match people_vectors::eject_clip(&state.data_dir, &req.subject, modality, &req.stem).await {
        Ok(Some(id)) => Json(serde_json::json!({ "ok": true, "new_cluster": id })).into_response(),
        Ok(None) => err("no such clip in that cluster"),
        Err(e) => err(&e.to_string()),
    }
}

// ── auto-regroup: preview then apply ────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SplitReq {
    subject: String,
    modality: String,
}

#[derive(Serialize)]
struct GroupDto {
    stems: Vec<String>,
}

/// `POST /api/people/split/preview` — propose an auto-regrouping of one modality of a
/// cluster. Moves nothing; returns the proposed groups (each a set of clip stems) and
/// any strays, for the view to preview before applying.
pub async fn post_split_preview(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SplitReq>,
) -> Response {
    let Some(modality) = parse_modality(&req.modality) else {
        return err("modality must be face or voice");
    };
    match people_vectors::propose_split(&state.data_dir, &req.subject, modality).await {
        Ok(p) => {
            let groups: Vec<GroupDto> = p.groups.into_iter().map(|g| GroupDto { stems: g.stems }).collect();
            Json(serde_json::json!({ "groups": groups, "strays": p.strays })).into_response()
        }
        Err(e) => err(&e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct ApplySplitReq {
    subject: String,
    modality: String,
    /// The groups to commit — each a list of clip stems. The largest stays under the
    /// original subject; the rest each become a new cluster.
    groups: Vec<Vec<String>>,
}

/// `POST /api/people/split/apply` — commit an accepted regrouping. Returns the ids of
/// the newly minted clusters (the original keeps its largest group + name).
pub async fn post_split_apply(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ApplySplitReq>,
) -> Response {
    let Some(modality) = parse_modality(&req.modality) else {
        return err("modality must be face or voice");
    };
    let groups: Vec<people_vectors::SplitGroup> = req
        .groups
        .into_iter()
        .map(|stems| people_vectors::SplitGroup { stems })
        .collect();
    match people_vectors::apply_split(&state.data_dir, &req.subject, modality, &groups).await {
        Ok(ids) => Json(serde_json::json!({ "ok": true, "new_clusters": ids })).into_response(),
        Err(e) => err(&e.to_string()),
    }
}

/// A uniform JSON error body with a 400.
fn err(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": msg }))).into_response()
}
