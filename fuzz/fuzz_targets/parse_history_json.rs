#![no_main]
// Fuzz the persisted-state loaders (history.json / state.json): corrupt
// input must degrade gracefully (None/default), never panic, and the
// sanitized restore state must honor its bounds.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 256 * 1024 {
        return;
    }
    // These run through tempfile paths in the smoke test; here the pure
    // deserialization + sanitization path is exercised directly.
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(state) = serde_json::from_str::<lion_session::savestate::SessionState>(s) {
            let clean = state.sanitized();
            assert!(clean.apps.len() <= lion_session::savestate::MAX_APPS);
        }
        let _ = serde_json::from_str::<lion_session::safemode::History>(s);
    }
});
