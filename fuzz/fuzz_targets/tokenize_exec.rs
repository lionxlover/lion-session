#![no_main]
// Fuzz the Exec tokenizer: either Err or a token vector bounded by
// MAX_EXEC_TOKENS with no NULs and no field codes remaining.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4096 {
        return;
    }
    let s = String::from_utf8_lossy(data);
    if let Ok(tokens) = lion_session::autostart::tokenize_exec(&s) {
        assert!(tokens.len() <= 64, "token bound");
        for t in tokens {
            assert!(!t.contains('\0'));
            assert!(!t.starts_with('%') || !t[1..].chars().all(|c| "fFuUdDnNiIcCkKvVmM".contains(c)));
        }
    }
});
