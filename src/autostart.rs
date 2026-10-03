#![forbid(unsafe_code)]
//! XDG autostart (spec 02 §3): parse `.desktop` entries from the
//! autostart dirs, filter them for this desktop, and produce runnable
//! argv with per-entry delays. Entries start after the shell is ready
//! (orchestrated in `session.rs`).
//!
//! Semantics implemented:
//! - XDG override order: later dirs replace earlier entries by file id.
//! - `Type=Application` only; `Hidden=true` skipped.
//! - `OnlyShowIn` must contain the desktop id when present;
//!   `NotShowIn` must not contain it.
//! - `X-GNOME-Autostart-enabled=false` skipped (de-facto standard).
//! - `TryExec` resolved against PATH (via the [`TryExecChecker`] port, so
//!   tests need no filesystem).
//! - `Exec` tokenized with quote/escape awareness; desktop-entry field
//!   codes (`%f %F %u %U %i %c %k …`) are dropped — autostart passes no
//!   file arguments. This tokenizer is a fuzz boundary.
//! - Bounds: 1 MiB per file, ≤ 64 tokens per Exec, delay ≤ 120 s.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Port: can a binary be executed? (PATH lookup or absolute + X_OK.)
pub trait TryExecChecker {
    fn is_executable(&self, name: &str) -> bool;
}

/// Real PATH checker.
pub struct PathTryExecChecker;

impl TryExecChecker for PathTryExecChecker {
    fn is_executable(&self, name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        let p = Path::new(name);
        if p.is_absolute() {
            return p.is_file();
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(name);
            if cand.is_file() {
                return true;
            }
        }
        false
    }
}

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_EXEC_TOKENS: usize = 64;
const MAX_DELAY_SECS: u64 = 120;

/// A parsed, filtered autostart entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutostartEntry {
    /// Desktop-file id (file stem).
    pub id: String,
    /// Tokenized, field-code-free argv.
    pub exec: Vec<String>,
    /// Per-entry extra delay beyond the base autostart_delay_ms.
    pub delay: Duration,
}

/// Raw key=value map from one .desktop file (single group tolerated:
/// `Desktop Entry`).
fn parse_desktop_group(content: &str) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    let mut in_entry_group = false;
    let mut saw_group = false;
    for (idx, raw) in content.lines().enumerate() {
        let line = raw.trim_end_matches(['\r']);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.starts_with('[') {
            if !trimmed.ends_with(']') {
                return Err(format!("line {}: malformed group header", idx + 1));
            }
            saw_group = true;
            in_entry_group = trimmed == "[Desktop Entry]";
            continue;
        }
        if !in_entry_group {
            // Keys before any group are an error; keys in other groups are
            // simply ignored (actions, translations…).
            if !saw_group {
                return Err(format!("line {}: key outside any group", idx + 1));
            }
            continue;
        }
        let Some(eq) = trimmed.find('=') else {
            return Err(format!("line {}: not key=value", idx + 1));
        };
        let key = trimmed[..eq].trim().to_string();
        let value = trimmed[eq + 1..].trim().to_string();
        if key.is_empty() {
            return Err(format!("line {}: empty key", idx + 1));
        }
        // Locale keys (Name[xx]) and duplicates: first value wins, matching
        // the desktop-entry spec's "first occurrence" rule.
        map.entry(key).or_insert(value);
    }
    if !saw_group {
        return Err("no group header".into());
    }
    Ok(map)
}

/// Split a locale string list ("LionOS;GNOME;").
fn parse_list(v: &str) -> Vec<String> {
    v.split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Tokenize an Exec value: whitespace-separated, honoring `"` `'` and
/// backslash escapes (desktop-entry spec §"Recognized arguments" —
/// quoting follows the same rules as POSIX shells, simplified to the
/// subset the spec defines). Field codes are dropped.
pub fn tokenize_exec(exec: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_token = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                has_token = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has_token = true;
            }
            '\\' if in_double || (!in_single && !in_double) => {
                // Inside double quotes or unquoted: backslash escapes the
                // next char. Inside single quotes: literal backslash.
                if in_single {
                    cur.push('\\');
                    has_token = true;
                } else if let Some(next) = chars.next() {
                    if !next.is_control() {
                        cur.push(next);
                        has_token = true;
                    }
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has_token {
                    tokens.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            '\0' => {
                // A literal NUL can never appear in a valid argv string
                // (execve would truncate); hostile input — reject.
                return Err("NUL byte in Exec".into());
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
        if tokens.len() > MAX_EXEC_TOKENS {
            return Err("too many tokens".into());
        }
    }
    if in_single || in_double {
        return Err("unterminated quote".into());
    }
    if has_token {
        tokens.push(cur);
    }
    if tokens.len() > MAX_EXEC_TOKENS {
        return Err("too many tokens".into());
    }
    // Drop desktop-entry field codes: tokens that start with '%' and whose
    // remainder consists solely of field-code characters (%f %F %u %U %d
    // %D %n %N %i %c %k %v %m and doubled forms' inner chars). Doubled
    // codes (%%f — literal) survive because '%' is not a code char.
    const FIELD_CODE_CHARS: &str = "fFuUdDnNiIcCkKvVmM";
    tokens.retain(|t| match t.strip_prefix('%') {
        Some(rest) => !rest.chars().all(|c| FIELD_CODE_CHARS.contains(c)),
        None => true,
    });
    Ok(tokens)
}

/// Filter + convert one raw entry. Returns None when the entry must not
/// autostart on this desktop.
fn entry_from_map(
    map: &BTreeMap<String, String>,
    id: &str,
    desktop_name: &str,
    try_exec: &dyn TryExecChecker,
) -> Result<Option<AutostartEntry>, String> {
    let ty = map.get("Type").map(|s| s.as_str()).unwrap_or("");
    if ty != "Application" {
        return Ok(None);
    }
    if map.get("Hidden").map(|v| v == "true").unwrap_or(false) {
        return Ok(None);
    }
    if let Some(only) = map.get("OnlyShowIn") {
        let list = parse_list(only);
        if !list.iter().any(|d| d == desktop_name) {
            return Ok(None);
        }
    }
    if let Some(not) = map.get("NotShowIn") {
        if parse_list(not).iter().any(|d| d == desktop_name) {
            return Ok(None);
        }
    }
    if map
        .get("X-GNOME-Autostart-enabled")
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        return Ok(None);
    }
    if let Some(tryexec) = map.get("TryExec") {
        if !try_exec.is_executable(tryexec) {
            return Ok(None);
        }
    }
    let Some(exec_raw) = map.get("Exec") else {
        return Ok(None);
    };
    let exec = tokenize_exec(exec_raw)?;
    if exec.is_empty() {
        return Ok(None);
    }
    let delay_secs: u64 = map
        .get("X-GNOME-Autostart-Delay")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
        .min(MAX_DELAY_SECS);
    Ok(Some(AutostartEntry {
        id: id.to_string(),
        exec,
        delay: Duration::from_secs(delay_secs),
    }))
}

/// Scan the autostart dirs (XDG precedence: later entries override
/// earlier ones by id) and return filtered entries sorted by id.
/// Unreadable/malformed files are skipped with a warning — autostart must
/// never brick the session (spec 02 §6 failure handling).
pub fn scan_dirs(
    dirs: &[PathBuf],
    desktop_name: &str,
    try_exec: &dyn TryExecChecker,
) -> Vec<AutostartEntry> {
    let mut by_id: BTreeMap<String, Result<Option<AutostartEntry>, String>> = BTreeMap::new();
    for dir in dirs {
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(target: "autostart", "cannot read {}: {e}", dir.display());
                continue;
            }
        };
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension().map(|x| x == "desktop").unwrap_or(false)
                    && !p
                        .file_name()
                        .map(|n| n.to_string_lossy().starts_with("."))
                        .unwrap_or(true)
            })
            .collect();
        files.sort();
        for path in files {
            let Some(id) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else {
                continue;
            };
            match std::fs::metadata(&path) {
                Ok(m) if m.len() > MAX_FILE_BYTES => {
                    tracing::warn!(target: "autostart", "{id}: oversized, skipped");
                    continue;
                }
                Err(_) => continue,
                _ => {}
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(target: "autostart", "{id}: unreadable ({e})");
                    continue;
                }
            };
            let parsed = parse_desktop_group(&content)
                .and_then(|map| entry_from_map(&map, &id, desktop_name, try_exec));
            match parsed {
                Ok(entry) => {
                    by_id.insert(id, Ok(entry)); // later dirs override
                }
                Err(reason) => {
                    // Malformed entries override to "skip" too (XDG: a
                    // later broken file still wins).
                    by_id.insert(id, Err(reason));
                }
            }
        }
    }
    let mut out: Vec<AutostartEntry> = by_id
        .into_iter()
        .filter_map(|(id, v)| match v {
            Ok(Some(entry)) => Some(entry),
            Ok(None) => None,
            Err(reason) => {
                tracing::warn!(target: "autostart", "{id}: {reason}, skipped");
                None
            }
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Fuzzing/test entry for the raw group parser (the production path is
/// `scan_dirs`, which applies file bounds and XDG precedence on top).
#[doc(hidden)]
pub fn parse_desktop_group_public(
    content: &str,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    parse_desktop_group(content)
}

/// Fuzzing/test entry for entry conversion (private in production).
#[doc(hidden)]
pub fn entry_from_map_public(
    map: &std::collections::BTreeMap<String, String>,
    id: &str,
    desktop_name: &str,
    try_exec: &dyn TryExecChecker,
) -> Result<Option<AutostartEntry>, String> {
    entry_from_map(map, id, desktop_name, try_exec)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysYes;
    impl TryExecChecker for AlwaysYes {
        fn is_executable(&self, _n: &str) -> bool {
            true
        }
    }
    struct AlwaysNo;
    impl TryExecChecker for AlwaysNo {
        fn is_executable(&self, _n: &str) -> bool {
            false
        }
    }

    fn parse(content: &str) -> Result<Option<AutostartEntry>, String> {
        let map = parse_desktop_group(content)?;
        entry_from_map(&map, "test", "LionOS", &AlwaysYes)
    }

    const VALID: &str = "[Desktop Entry]\nType=Application\nName=Foo\nExec=foo --bar\n";

    #[test]
    fn valid_entry() {
        let e = parse(VALID).unwrap().unwrap();
        assert_eq!(e.exec, vec!["foo", "--bar"]);
        assert_eq!(e.delay, Duration::ZERO);
    }

    #[test]
    fn wrong_type_skipped() {
        let e = parse("[Desktop Entry]\nType=Link\nName=x\nExec=foo\n").unwrap();
        assert!(e.is_none());
    }

    #[test]
    fn hidden_skipped() {
        let e = parse(&format!("{VALID}Hidden=true\n")).unwrap();
        assert!(e.is_none());
    }

    #[test]
    fn only_show_in_filters() {
        let kept = parse(&format!("{VALID}OnlyShowIn=LionOS;GNOME;\n"))
            .unwrap()
            .unwrap();
        assert_eq!(kept.id, "test");
        let skipped = parse(&format!("{VALID}OnlyShowIn=KDE;\n")).unwrap();
        assert!(skipped.is_none());
        // absent OnlyShowIn → applies everywhere → kept
        assert!(parse(VALID).unwrap().is_some());
    }

    #[test]
    fn not_show_in_filters() {
        let e = parse(&format!("{VALID}NotShowIn=LionOS;\n")).unwrap();
        assert!(e.is_none());
        assert!(parse(&format!("{VALID}NotShowIn=KDE;\n"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn gnome_disabled_skipped() {
        let e = parse(&format!("{VALID}X-GNOME-Autostart-enabled=false\n")).unwrap();
        assert!(e.is_none());
        assert!(parse(&format!("{VALID}X-GNOME-Autostart-enabled=true\n"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn try_exec_checked_via_port() {
        let map = parse_desktop_group(&format!("{VALID}TryExec=/nonexistent/foo\n")).unwrap();
        let e = entry_from_map(&map, "t", "LionOS", &AlwaysNo).unwrap();
        assert!(e.is_none());
        let e = entry_from_map(&map, "t", "LionOS", &AlwaysYes).unwrap();
        assert!(e.is_some());
    }

    #[test]
    fn no_exec_skipped() {
        let e = parse("[Desktop Entry]\nType=Application\nName=x\n").unwrap();
        assert!(e.is_none());
    }

    #[test]
    fn delay_parsed_and_bounded() {
        let e = parse(&format!("{VALID}X-GNOME-Autostart-Delay=7\n"))
            .unwrap()
            .unwrap();
        assert_eq!(e.delay, Duration::from_secs(7));
        let e = parse(&format!("{VALID}X-GNOME-Autostart-Delay=999\n"))
            .unwrap()
            .unwrap();
        assert_eq!(e.delay, Duration::from_secs(120));
        let e = parse(&format!("{VALID}X-GNOME-Autostart-Delay=garbage\n"))
            .unwrap()
            .unwrap();
        assert_eq!(e.delay, Duration::ZERO);
    }

    #[test]
    fn field_codes_dropped() {
        let e = parse("[Desktop Entry]\nType=Application\nExec=foo %f --opt %U %i %c %k %m %v\n")
            .unwrap()
            .unwrap();
        assert_eq!(e.exec, vec!["foo", "--opt"]);
    }

    #[test]
    fn quoted_arguments_preserved() {
        let e = parse(
            "[Desktop Entry]\nType=Application\nExec=foo \"two words\" 'single arg' plain\\ escape\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            e.exec,
            vec!["foo", "two words", "single arg", "plain escape"]
        );
    }

    #[test]
    fn tokenizer_errors() {
        assert!(tokenize_exec("foo \"unterminated").is_err());
        assert!(tokenize_exec("foo 'unterminated").is_err());
        let big: String = "arg ".repeat(100);
        assert!(tokenize_exec(&big).is_err());
    }

    #[test]
    fn malformed_files_error() {
        assert!(
            parse_desktop_group("Type=Application\n").is_err(),
            "key before group"
        );
        assert!(parse_desktop_group("no group at all\n").is_err());
        assert!(parse_desktop_group("[Desktop Entry\n").is_err());
        let e = parse("[Desktop Entry]\nnotkeyvalue\n").unwrap_err();
        assert!(e.contains("not key=value"));
    }

    #[test]
    fn locale_keys_do_not_shadow() {
        // Name[fr] must not be treated as Exec etc.; plain keys win, and
        // bracketed variants are simply different keys.
        let e = parse(&format!(
            "{VALID}Name[fr]=Pas important\nExec[fr]=impossible\n"
        ))
        .unwrap()
        .unwrap();
        assert_eq!(e.exec, vec!["foo", "--bar"]);
    }

    #[test]
    fn scan_dirs_override_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let d1 = dir.path().join("sys");
        let d2 = dir.path().join("user");
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();
        std::fs::write(d1.join("a.desktop"), VALID).unwrap();
        std::fs::write(
            d1.join("b.desktop"),
            "[Desktop Entry]\nType=Application\nExec=first\n",
        )
        .unwrap();
        // user overrides b with a NotShowIn → b disappears
        std::fs::write(
            d2.join("b.desktop"),
            "[Desktop Entry]\nType=Application\nExec=second\nNotShowIn=LionOS\n",
        )
        .unwrap();
        std::fs::write(
            d2.join("c.desktop"),
            "[Desktop Entry]\nType=Application\nExec=third\n",
        )
        .unwrap();
        let entries = scan_dirs(&[d1, d2], "LionOS", &AlwaysYes);
        let ids: Vec<(String, Vec<String>)> = entries
            .iter()
            .map(|e| (e.id.clone(), e.exec.clone()))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("a".into(), vec!["foo".to_string(), "--bar".into()]),
                ("c".into(), vec!["third".into()])
            ]
        );
    }

    #[test]
    fn scan_dirs_skips_broken_and_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("autostart");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("broken.desktop"), "no group\n").unwrap();
        std::fs::write(d.join("good.desktop"), VALID).unwrap();
        std::fs::write(
            d.join(".hidden.desktop"),
            "[Desktop Entry]\nType=Application\nExec=nope\n",
        )
        .unwrap();
        let mut entries = scan_dirs(std::slice::from_ref(&d), "LionOS", &AlwaysYes);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries.remove(0).id, "good");
    }

    #[test]
    fn nonexistent_dir_ok() {
        let out = scan_dirs(&[PathBuf::from("/nonexistent-xyz")], "LionOS", &AlwaysYes);
        assert!(out.is_empty());
    }

    #[test]
    fn comments_and_crlf() {
        let e =
            parse("# comment\r\n[Desktop Entry]\r\n; another\r\nType=Application\r\nExec=foo\r\n")
                .unwrap()
                .unwrap();
        assert_eq!(e.exec, vec!["foo"]);
    }
}
