#![forbid(unsafe_code)]
//! Fuzz smoke (spec 02 §10: run a short fuzz pass in CI): exercise the
//! parser boundaries with generated adversarial shapes without
//! cargo-fuzz (which needs nightly). The full fuzz targets live in
//! `fuzz/`; this smoke gate catches the panics that matter most —
//! malformed quoting, unbounded lists, NUL smuggling, deep nesting.

use lion_session::autostart::tokenize_exec;

fn adversarial_inputs() -> Vec<Vec<u8>> {
    let mut cases: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"[Desktop Entry]".to_vec(),
        b"[Desktop Entry]\n".to_vec(),
        b"[Desktop Entry\nType=Application\nExec=x\n".to_vec(),
        b"[Desktop Entry]\nType=Application\n".to_vec(),
        b"key=value\n".to_vec(),
        b"[Desktop Entry]\n=".to_vec(),
        b"[Desktop Entry]\n=a".to_vec(),
        b"[Desktop Entry]\nType\x00=Application".to_vec(),
        b"[Desktop Entry]\r\nType=Application\r\nExec=foo\r\n".to_vec(),
        // quoting horrors for the tokenizer
        b"foo \"unterminated".to_vec(),
        b"foo 'unterminated".to_vec(),
        b"foo \"\\\\\" bar".to_vec(),
        b"foo \"\\\n\" bar".to_vec(),
        b"' ' \" \" \\\\".to_vec(),
        b"%f %F %U %u %i %c %k %v %m %d %D %n %N".to_vec(),
        b"%%f %%c".to_vec(),
        b"%".to_vec(),
        b"%%".to_vec(),
        "a ".repeat(200).into_bytes(),
        // deep brace/quote nesting
        b"\"'\"'\"'\"'\"'\"'".to_vec(),
        // oversized single token
        "x".repeat(100_000).into_bytes(),
    ];
    // random-ish byte soup (deterministic PRNG, no rand dep)
    let mut seed = 0x5eedu64;
    for round in 0..64 {
        let mut blob = Vec::with_capacity(512);
        let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        for _ in 0..512 {
            blob.push((s >> 33) as u8);
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        blob.push(b'\n');
        cases.push(blob);
        seed += 0x9e37 + round as u64;
    }
    cases
}

#[test]
fn fuzz_smoke_desktop_parser() {
    let cases = adversarial_inputs();
    let mut parsed_ok = 0;
    for case in &cases {
        let s = String::from_utf8_lossy(case);
        if lion_session::autostart::parse_desktop_group_public(&s).is_ok() {
            parsed_ok += 1;
        }
        // never panics; that is the invariant under test
    }
    // sanity: some inputs do parse (the harness is not vacuous)
    assert!(
        parsed_ok >= 5,
        "expected several parseable cases, got {parsed_ok}/{len}",
        len = cases.len()
    );
}

#[test]
fn fuzz_smoke_exec_tokenizer() {
    for case in &cases_buffered() {
        let s = String::from_utf8_lossy(case);
        if let Ok(tokens) = tokenize_exec(&s) {
            assert!(tokens.len() <= 64, "token count bound");
            for t in &tokens {
                assert!(!t.contains('\0'), "no NUL smuggling");
            }
        }
    }
    // specific shape checks
    assert!(tokenize_exec("a 'b").is_err());
    assert!(tokenize_exec("a \"b").is_err());
    let tokens = tokenize_exec("foo %f %U --keep %foo").unwrap();
    assert!(tokens.contains(&"--keep".to_string()));
    assert!(
        tokens.contains(&"%foo".to_string()),
        "non-code % tokens stay"
    );
}

#[test]
fn fuzz_smoke_state_json() {
    for case in cases_buffered() {
        if let Ok(s) = std::str::from_utf8(&case) {
            if let Ok(state) = serde_json::from_str::<lion_session::savestate::SessionState>(s) {
                let clean = state.sanitized();
                assert!(clean.apps.len() <= lion_session::savestate::MAX_APPS);
                for app in &clean.apps {
                    assert!(app.exec.len() <= lion_session::savestate::MAX_EXEC_LEN);
                }
            }
            let _ = serde_json::from_str::<lion_session::safemode::History>(s);
        }
    }
    // determinism spot-check on the sanitize bounds
    let raw = r#"{"apps":[{"app_id":"x","exec":["y"],"workspace":99}],"workspaces":99}"#;
    let st: lion_session::savestate::SessionState = serde_json::from_str(raw).unwrap();
    let clean = st.sanitized();
    assert_eq!(clean.apps[0].workspace, 31);
    assert_eq!(clean.workspaces, 16);
}

fn cases_buffered() -> Vec<Vec<u8>> {
    adversarial_inputs()
}
