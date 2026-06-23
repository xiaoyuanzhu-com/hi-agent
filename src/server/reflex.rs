//! The reflex invoke route — fire a taught quick-action with no model in the loop.
//!
//! `POST /api/reflex/invoke` is the v1 trigger (a later global hotkey/gesture would
//! call the same path). It reads the current desktop context + accessibility tree,
//! asks [`crate::body::reflex::recognize`] whether exactly one taught reflex applies, and
//! if so fires it. The whole path is deterministic and LLM-free.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::body::reflex::{self, Recognition};
use crate::server::AppState;

/// Recognize the current moment against taught reflexes and, if exactly one applies,
/// fire it (click the field, type the value). Always responds 200; the JSON body
/// reports whether it fired and, when it abstained, why — so a caller (or a curling
/// tester) can see the reason without an error status.
pub async fn post_invoke(State(state): State<Arc<AppState>>) -> Json<Value> {
    let reflexes = match reflex::load_all(&state.data_dir).await {
        Ok(r) => r,
        Err(err) => {
            return Json(json!({ "fired": false, "reason": format!("could not read reflexes: {err}") }));
        }
    };
    if reflexes.is_empty() {
        return Json(json!({ "fired": false, "reason": "no reflexes taught yet" }));
    }

    // Coarse window gate — best-effort: an unavailable context just means there's no
    // app/window to match on (a reflex that gates on app then can't match).
    let ctx = crate::body::capabilities::desktop_context::capture().await.ok();
    let app = ctx.as_ref().and_then(|c| c.frontmost_app.as_deref());
    let title = ctx.as_ref().and_then(|c| c.frontmost_window_title.as_deref());

    // Field-level recognition over the accessibility tree.
    let elements = match crate::body::capabilities::accessibility::inspect().await {
        Ok(e) => e,
        Err(err) => {
            return Json(json!({ "fired": false, "reason": format!("accessibility unavailable: {err}") }));
        }
    };

    match reflex::recognize(&reflexes, app, title, &elements) {
        Recognition::Fire { reflex, target } => match reflex::fire(&reflex, &target).await {
            Ok(()) => Json(json!({ "fired": true, "reflex": reflex.name })),
            Err(err) => Json(json!({
                "fired": false,
                "reflex": reflex.name,
                "reason": format!("fire failed: {err}"),
            })),
        },
        Recognition::Abstain(reason) => Json(json!({ "fired": false, "reason": reason })),
    }
}
