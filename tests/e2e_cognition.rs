//! Full-stack smoke: real bundled adapter ↔ local proxy ↔ stub upstream.
//! Opt-in: `RUN_E2E=1 cargo test --test e2e_cognition -- --nocapture`.

#[tokio::test]
async fn thought_round_trips_through_bundle() {
    if std::env::var("RUN_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping e2e (set RUN_E2E=1 to run)");
        return;
    }
    // A real run requires:
    //   - a built bundle (`make bundle`) OR HI_AGENT_DEV_NODE/ADAPTER/CLAUDE set,
    //   - HI_AGENT_UPSTREAM_KEY set,
    //   - upstream_base_url pointing at a reachable Anthropic-compatible endpoint
    //     (or a local stub that returns a canned Messages SSE stream).
    // Build a Config, call hi_agent::run on a random port in a task, POST a
    // /thought, and assert a journal line / thought-bus emission appears.
    // (Left as the single heavy integration check; keep it deterministic by
    //  pointing upstream at a local stub rather than the real API.)
    eprintln!("e2e harness placeholder — implement against a local SSE stub");
}
