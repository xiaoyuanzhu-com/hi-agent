//! Stable per-machine identity for the broker `device_id`.
//!
//! The broker keys one account per `device_id` (see [`crate::foundation::broker`]),
//! so the whole "one device = one account, forever" invariant rests on the
//! `device_id` being **stable across app uninstalls and data-dir wipes**. A random
//! UUID minted into `config.db` fails that: wipe the data dir and the next launch
//! bootstraps a *new* account, orphaning the old one (and any sub tier bound to it).
//!
//! So we derive the id from the **host machine** instead of minting-and-storing it:
//! - macOS: `IOPlatformUUID` (from `ioreg`) — hardware-tied, survives reinstall.
//! - Linux: `/etc/machine-id` (or `/var/lib/dbus/machine-id`).
//! - Windows: `MachineGuid` under `HKLM\SOFTWARE\Microsoft\Cryptography`.
//!
//! We never send the raw platform id to the broker: it's hashed with an app-specific
//! salt into an app-scoped UUID, so the broker sees an opaque, hi-agent-only handle
//! (not a fingerprint that correlates across apps).
//!
//! Fallback: if the OS source is unreadable (odd distro, sandbox, permission), we
//! return `None` and the caller mints+persists a random UUID as before — a machine
//! that can't be fingerprinted still gets a working (if not reinstall-proof) account,
//! rather than failing to bootstrap.
//!
//! Migration: this is only consulted when a store has **no** `device_id` yet. An
//! install that already holds one keeps it untouched — we never re-derive out from
//! under a live account.

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// App-specific salt so the emitted id is scoped to hi-agent and can't be
/// correlated with another app deriving from the same platform id.
const SALT: &str = "hi-agent/device-id/v1";

/// A stable, app-scoped device id derived from the host machine, or `None` when no
/// platform source is readable (caller then falls back to a random UUID). The value
/// is a UUID string, matching the shape the broker already expects.
pub fn derive() -> Option<String> {
    let raw = platform_id()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Some(scope(raw))
}

/// Fold a raw platform id into an app-scoped UUID (v8, custom) via a salted SHA-256.
/// Deterministic: same machine → same id, every launch.
fn scope(raw: &str) -> String {
    let mut h = Sha256::new();
    h.update(SALT.as_bytes());
    h.update(b"\0");
    h.update(raw.as_bytes());
    let digest = h.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes).to_string()
}

#[cfg(target_os = "macos")]
fn platform_id() -> Option<String> {
    // `ioreg -rd1 -c IOPlatformExpertDevice` prints an "IOPlatformUUID" = "<uuid>"
    // line. This is a plain CLI read — no TCC prompt, no framework linkage — so it
    // stays engine-side per the headless-engine boundary.
    let out = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(rest) = line.split("\"IOPlatformUUID\"").nth(1) {
            // rest looks like: ` = "AAAA-BBBB-..."`
            if let Some(v) = rest.split('"').nth(1) {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn platform_id() -> Option<String> {
    // systemd's stable per-installation id; dbus path is the older fallback.
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(s) = std::fs::read_to_string(path) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn platform_id() -> Option<String> {
    // MachineGuid is written at OS install; survives app reinstall. Read it via `reg
    // query` to avoid pulling a registry crate into the engine for one lookup.
    let out = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // A value line looks like: `    MachineGuid    REG_SZ    <guid>`
    for line in text.lines() {
        if line.contains("MachineGuid") {
            if let Some(v) = line.split("REG_SZ").nth(1) {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn platform_id() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_is_deterministic_and_uuid_shaped() {
        let a = scope("ABC-123");
        let b = scope("ABC-123");
        assert_eq!(a, b, "same raw id must fold to the same scoped id");
        assert!(Uuid::parse_str(&a).is_ok(), "scoped id must be a valid UUID");
    }

    #[test]
    fn scope_differs_by_input() {
        assert_ne!(scope("machine-a"), scope("machine-b"));
    }

    #[test]
    fn derive_is_stable_across_calls() {
        // On any host with a readable platform id, two derivations must agree; on a
        // host without one, both are None. Either way they match — no per-call drift.
        assert_eq!(derive(), derive());
    }
}
