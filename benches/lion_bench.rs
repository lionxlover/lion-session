//! lion-bench hooks (spec 02 §7): resolver throughput, autostart parsing,
//! inhibitor bookkeeping, lifecycle FSM transitions, startup and idle
//! RSS. Emits JSON (one object per line) so `lion-bench` can consume it
//! and CI can fail on >10% regression.
//!
//! Run: `cargo bench` (harness = false; plain timings, no criterion).

use std::io::Write;
use std::time::{Duration, Instant};

fn json_mode() -> bool {
    std::env::args().any(|a| a == "--json")
}

fn emit(out: &mut impl Write, name: &str, value: &str, unit: &str) {
    let line = format!(
        "{{\"component\":\"lion-session\",\"metric\":\"{name}\",\"value\":{value},\"unit\":\"{unit}\"}}\n"
    );
    print!("{line}");
    let _ = out.flush();
}

fn main() {
    let out = std::io::stdout();
    let mut out = out.lock();

    // ── 1. resolver: 2 000-service random DAG ─────────────────────────
    let specs: Vec<lion_session::config::ServiceSpec> = (0..2000)
        .map(|i: usize| {
            // ~5 deps per node (real sessions: <10 services; this stays
            // a stress case without being a 1M-edge pathology).
            let after: Vec<String> = (0..i)
                .filter(|j| (i.wrapping_mul(31) + j) % 400 == 0)
                .map(|j| format!("s{j}"))
                .collect();
            lion_session::config::ServiceSpec {
                name: format!("s{i}"),
                unit: None,
                exec: Some(vec!["x".into()]),
                after,
                restart: lion_session::config::RestartPolicy::Always,
                ready_gate: false,
            }
        })
        .collect();
    let t = Instant::now();
    let plan = lion_session::resolver::resolve(&specs).expect("resolve");
    let resolve_us = t.elapsed().as_micros() as f64;
    let levels = plan.levels.len();
    if json_mode() {
        emit(
            &mut out,
            "resolver_2000_nodes_us",
            &format!("{resolve_us:.1}"),
            "us",
        );
    } else {
        println!("resolver: 2000 nodes, {levels} levels in {resolve_us:.0} µs");
    }

    // ── 2. autostart: parse 500 desktop entries ──────────────────────
    let dir = tempfile::tempdir().expect("tmpdir");
    let d = dir.path().join("autostart");
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..500 {
        let content = format!(
            "[Desktop Entry]\nType=Application\nName=App {i}\nExec=app{i} --flag %U\n\
             OnlyShowIn=LionOS;\nX-GNOME-Autostart-Delay={}\n",
            i % 30
        );
        std::fs::write(d.join(format!("app{i}.desktop")), content).unwrap();
    }
    struct Yes;
    impl lion_session::autostart::TryExecChecker for Yes {
        fn is_executable(&self, _n: &str) -> bool {
            true
        }
    }
    let t = Instant::now();
    let entries = lion_session::autostart::scan_dirs(&[d], "LionOS", &Yes);
    let autostart_us = t.elapsed().as_micros() as f64;
    assert_eq!(entries.len(), 500, "all entries pass");
    if json_mode() {
        emit(
            &mut out,
            "autostart_parse_500_us",
            &format!("{autostart_us:.1}"),
            "us",
        );
    } else {
        println!("autostart: 500 entries in {autostart_us:.0} µs");
    }

    // ── 3. inhibitors: 10 000 add/remove cycles ──────────────────────
    let mut store = lion_session::inhibitors::InhibitorStore::default();
    let t = Instant::now();
    let mut ids = Vec::new();
    for i in 0..5000 {
        if let Ok(id) = store.add(
            "shutdown",
            &format!("app{i}"),
            "why",
            ":1.1",
            Instant::now(),
            5000,
        ) {
            ids.push(id);
        }
    }
    for id in ids {
        store.remove(id);
    }
    let inhibit_us = t.elapsed().as_micros() as f64;
    if json_mode() {
        emit(
            &mut out,
            "inhibitors_10k_ops_us",
            &format!("{inhibit_us:.1}"),
            "us",
        );
    } else {
        println!("inhibitors: 10k add/remove in {inhibit_us:.0} µs");
    }

    // ── 4. lifecycle FSM: 100 000 transitions ────────────────────────
    let t = Instant::now();
    for _ in 0..10_000 {
        let mut m = lion_session::lifecycle::Machine::new(Duration::from_millis(8000));
        m.register("a").unwrap();
        m.register("b").unwrap();
        let _ = m
            .try_begin(lion_session::lifecycle::Action::Logout, Instant::now(), &[])
            .unwrap();
        let _ = m.ack("a");
        let _ = m.ack("b");
    }
    let fsm_us = t.elapsed().as_micros() as f64;
    if json_mode() {
        emit(
            &mut out,
            "lifecycle_fsm_10k_us",
            &format!("{fsm_us:.1}"),
            "us",
        );
    } else {
        println!("lifecycle FSM: 10k cycles in {fsm_us:.0} µs");
    }

    // ── 5. environment build ─────────────────────────────────────────
    let inputs = lion_session::environment::EnvInputs {
        imports: (0..20)
            .map(|i| (format!("VAR{i}"), "value".to_string()))
            .collect(),
        xdg_runtime_dir: "/run/user/1000".into(),
        wayland_display: Some("wayland-0".into()),
        desktop_name: "LionOS".into(),
        dbus_session_bus_address: None,
        theme: std::collections::BTreeMap::new(),
        extra: std::collections::BTreeMap::new(),
    };
    let t = Instant::now();
    for _ in 0..100_000 {
        let _ = lion_session::environment::SessionEnv::build(&inputs).unwrap();
    }
    let env_us = t.elapsed().as_micros() as f64 / 100_000.0;
    if json_mode() {
        emit(
            &mut out,
            "environment_build_ns",
            &format!("{:.1}", env_us * 1000.0),
            "ns",
        );
    } else {
        println!("environment: 100k builds, {env_us:.2} µs each");
    }

    // ── 6. idle RSS ──────────────────────────────────────────────────
    // (The daemon's *own* idle RSS is measured by the mock demo in CI;
    // here we verify the measurement helper itself is cheap.)
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let rss: u32 = status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        .unwrap_or(0);
    if json_mode() {
        emit(&mut out, "bench_process_rss_kb", &rss.to_string(), "kb");
    } else {
        println!("bench harness RSS: {rss} kB");
    }

    if !json_mode() {
        println!("\n(resolve {resolve_us:.0}µs | autostart {autostart_us:.0}µs | inhibit {inhibit_us:.0}µs | fsm {fsm_us:.0}µs)");
    }
}
