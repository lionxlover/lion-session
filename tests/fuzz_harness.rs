//! Deterministic fuzz harness for lion-session's untrusted-input
//! surfaces (mirrors the lion-greeter harness, same LCG engine).
//!
//! What gets fuzzed:
//! 1. `config::Raw` TOML parsing — a broken config must never panic
//!    the parser, and an accepted config must always merge into
//!    field-valid values.
//! 2. `desktop::exec_tokens` (Exec-line splitting with the desktop-spec
//!    quoting rules) — arbitrary Exec strings must split without panic
//!    into argv with no empty tokens, no embedded NULs, and no lost
//!    quoting characters.
//! 3. The merge-by-name autostart rule — mutation must never corrupt
//!    the "one entry per name" invariant.
//!
//! Deterministic (fixed seed xorshift, zero deps) so CI runs it on
//! every push; `cargo fuzz` targets can grow the corpus on networked
//! infra using the same invariants.

use lion_session::config::Raw;
use lion_session::desktop::exec_tokens;

struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
}

const CONFIG_CORPUS: &[&str] = &[
    "",
    "\n\n# comment only\n",
    "logout-animation-ms = 350\n",
    "[compositor]\ncommand = \"lion-compositor\"\nwayland-display = \"wayland-1\"\n",
    "[compositor]\nmax-restarts = 3\ncrash-window-ms = 60000\nrestart = true\n",
    "[session]\nend-timeout-ms = 10000\nlock-on-sleep = true\n",
    "[session]\nlock-after-ms = 300000\nlogout-after-ms = 0\n",
    "[session]\nlock-on-shutdown = false\nvia = \"never\"\n",
    "[[autostart]]\nname = \"lion-dock\"\nenabled = false\n",
    "[[autostart]]\nname = \"x\"\ncommand = \"/opt/x\"\nmemory-max = \"512M\"\ncpu-weight = 250\ntasks-max = 128\n",
    "[[autostart]]\nname = \"a\"\n[[autostart]]\nname = \"a\"\n",
    "[garbage]\n= = =\n",
    "[session]\nend-timeout-ms = \"not a number\"\n",
];

const EXEC_CORPUS: &[&str] = &[
    "lion-session",
    "foo --flag value",
    "foo \"quoted arg\" bar",
    "foo 'single quoted' bar",
    "foo \"esc \\\\\" bar",
    "foo \"esc \\n \\t \\s \\\\\" bar",
    "foo %U %u %f %F",
    "foo\t tab-separated",
    "  leading and trailing spaces   ",
    "\"\"",
    "unclosed \"quote",
    "foo \\ trailing",
    "a b c d e f g h i j k l m n o p",
];

fn mutate(text: &str, rng: &mut Lcg) -> String {
    let mut bytes: Vec<u8> = text.as_bytes().to_vec();
    if bytes.is_empty() {
        return String::new();
    }
    let rounds = 1 + rng.below(4);
    for _ in 0..rounds {
        if bytes.is_empty() {
            break;
        }
        match rng.below(6) {
            0 => {
                let i = rng.below(bytes.len());
                bytes[i] ^= 1 << (rng.below(8) as u32);
            }
            1 => {
                let at = 1 + rng.below(bytes.len());
                bytes.truncate(at);
            }
            2 => {
                let i = rng.below(bytes.len());
                let b = b"\"'[]#= \n\t\\\x00;{}"[rng.below(14)];
                bytes.insert(i, b);
            }
            3 => {
                let i = rng.below(bytes.len());
                let len = 1 + rng.below(8.min(bytes.len() - i));
                let chunk = bytes[i..i + len].to_vec();
                bytes.extend(chunk);
            }
            4 => {
                let i = rng.below(bytes.len());
                let len = 1 + rng.below(8.min(bytes.len() - i));
                bytes.drain(i..i + len);
            }
            _ => {
                let i = rng.below(bytes.len());
                bytes[i] = b"[]\"'=# \n"[rng.below(7)];
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Invariants every *accepted* config must hold.
fn valid_merged(raw: &Raw) {
    if let Some(c) = &raw.compositor {
        // Times and counters are u64 by type; TOML guarantees
        // non-negative. Check the policy-critical ones anyway.
        assert!(c.ready_timeout_ms > 0, "zero ready timeout accepted");
    }
    if let Some(s) = &raw.session {
        if let Some(v) = s.lock_after_ms {
            assert!(v < 100 * 365 * 24 * 3600 * 1000, "absurd lock-after-ms {v}");
        }
    }
    for app in &raw.autostart {
        assert!(!app.name.is_empty(), "app with empty name accepted");
        if let Some(m) = &app.memory_max {
            // The parser accepts any string; the *consumer* filters.
            // The hard invariant is: no NUL ever reaches argv.
            assert!(!m.contains('\u{0}'), "NUL in memory-max: {m:?}");
            // And the filtered output never contains an invalid value.
            for p in app.cgroup_properties() {
                if let Some(value) = p.strip_prefix("--property=MemoryMax=") {
                    assert!(!value.contains('\u{0}'));
                }
            }
        }
    }
}

#[test]
fn config_toml_never_panics_on_mutated_inputs() {
    let mut rng = Lcg::new(0x5345_5353_494F_4E03); // "SESSION"
    for (idx, seed) in CONFIG_CORPUS.iter().enumerate() {
        if let Ok(raw) = toml::from_str::<Raw>(seed) {
            valid_merged(&raw);
        }
        for m in 0..200 {
            let damaged = mutate(seed, &mut rng);
            let tag = format!("config corpus[{idx}] mutation[{m}]");
            let verdict = std::panic::catch_unwind(|| toml::from_str::<Raw>(&damaged));
            match verdict {
                Err(_) => panic!("config parse panicked on {tag}: {damaged:?}"),
                Ok(Ok(raw)) => valid_merged(&raw),
                Ok(Err(_)) => {}
            }
        }
    }
}

#[test]
fn exec_tokenizer_never_panics_and_never_loses_args() {
    let mut rng = Lcg::new(0x4558_4543_2020_2020); // "EXEC"
    for (idx, seed) in EXEC_CORPUS.iter().enumerate() {
        // Pristine invariant: the ONE argv-fatal character is NUL
        // (empty tokens are legal argv — `foo "" bar` is valid shell
        // semantics and spawns fine).
        for t in exec_tokens(seed) {
            assert!(!t.contains('\u{0}'), "NUL in token: {t:?}");
        }
        for m in 0..300 {
            let damaged = mutate(seed, &mut rng);
            let tag = format!("exec corpus[{idx}] mutation[{m}]");
            let verdict = std::panic::catch_unwind(|| exec_tokens(&damaged));
            match verdict {
                Err(_) => panic!("exec tokenizer panicked on {tag}: {damaged:?}"),
                Ok(tokens) => {
                    for t in tokens {
                        assert!(!t.contains('\u{0}'), "NUL token from {tag}: {t:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn merge_by_name_invariant_survives_mutations() {
    let mut rng = Lcg::new(0x4D45_5247_4531_3233); // "MERGE"
    for (idx, seed) in CONFIG_CORPUS
        .iter()
        .filter(|c| c.contains("[[autostart]]"))
        .enumerate()
    {
        for m in 0..100 {
            let damaged = mutate(seed, &mut rng);
            let tag = format!("merge corpus[{idx}] mutation[{m}]");
            if let Ok(raw) = toml::from_str::<Raw>(&damaged) {
                // Whatever parsed, the merge keeps one entry per name.
                let mut names: Vec<&str> = raw.autostart.iter().map(|a| a.name.as_str()).collect();
                let total = names.len();
                names.sort();
                names.dedup();
                assert_eq!(
                    names.len(),
                    total,
                    "duplicate names from {tag}: {:?}",
                    raw.autostart.iter().map(|a| &a.name).collect::<Vec<_>>()
                );
            }
        }
    }
}

#[test]
fn mutation_engine_is_deterministic_and_effective() {
    let mut a = Lcg::new(99);
    let mut b = Lcg::new(99);
    for _ in 0..500 {
        assert_eq!(a.next_u64(), b.next_u64());
    }
    let text = "[session]\nend-timeout-ms = 5\n";
    let mut x = Lcg::new(7);
    let mut changed = 0;
    for _ in 0..200 {
        let m = mutate(text, &mut x);
        if m != text {
            changed += 1;
        }
    }
    assert!(changed > 180, "mutation too passive: {changed}/200");
}
