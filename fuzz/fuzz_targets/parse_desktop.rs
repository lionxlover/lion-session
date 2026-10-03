#![no_main]
// Fuzz the XDG autostart .desktop parser (spec 02 §10: fuzz every
// parser). Invariant: parse_desktop_group either errors or yields a
// map whose keys/values are bounded, and entry_from_map never panics.
use libfuzzer_sys::fuzz_target;

struct YesCheck;
impl lion_session::autostart::TryExecChecker for YesCheck {
    fn is_executable(&self, _n: &str) -> bool {
        true
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 1024 * 1024 {
        return;
    }
    let s = String::from_utf8_lossy(data);
    if let Ok(map) = lion_session::autostart::parse_desktop_group_public(&s) {
        for (k, v) in map {
            assert!(k.len() <= 512, "key bound");
            assert!(v.len() <= 8192, "value bound");
        }
        let _ = lion_session::autostart::entry_from_map_public(&map, "fuzz", "LionOS", &YesCheck);
    }
});
