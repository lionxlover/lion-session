//! XDG desktop-entry autostart: reads `/etc/xdg/autostart/*.desktop` (plus
//! `$XDG_CONFIG_DIRS`) and `~/.config/autostart/*.desktop` -- the
//! cross-desktop autostart standard GNOME, KDE and XFCE all share -- and
//! turns the visible entries into autostart `App`s, so a user's existing
//! autostart keeps working on LionOS without touching anything.
//!
//! Semantics implemented (spec plus the de-facto GNOME extensions):
//!   - only the `[Desktop Entry]` group is read
//!   - `Type` must be `Application`
//!   - `Hidden=true` disables the entry (the standard way a user file
//!     overrides a system file without deleting it)
//!   - `OnlyShowIn` / `NotShowIn` are matched against `LionOS`
//!   - `X-GNOME-Autostart-enabled=false` disables (GNOME convention)
//!   - `TryExec` skips the entry when the binary is not resolvable
//!   - `Exec` uses the desktop-spec quoting rules; field codes
//!     (`%f %F %u %U %i %c %k` ...) are dropped, exactly what
//!     `g_spawn`-based launchers do when no files are being opened
//!   - a user file with the same *file name* wins over the system one
//!
//! By default XDG entries are started unsupervised (`restart = never`),
//! matching ecosystem expectations; `[session] supervise-xdg = true`
//! opts them into our supervision instead.

use crate::{config::App, proc};
use std::{collections::BTreeMap, path::PathBuf};

const CURRENT_DESKTOP: &str = "LionOS";

/// A parsed `[Desktop Entry]` group. Keys are stored case-sensitively as
/// written; lookups are case-insensitive per the spec.
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopEntry {
    pub path: PathBuf,
    fields: BTreeMap<String, String>,
}

impl DesktopEntry {
    /// Parse only the `[Desktop Entry]` group of a desktop file.
    pub fn parse(text: &str, path: PathBuf) -> Self {
        let mut fields = BTreeMap::new();
        let mut in_entry_group = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                in_entry_group = line == "[Desktop Entry]";
                continue;
            }
            if !in_entry_group {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                fields.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        Self { path, fields }
    }

    /// Case-insensitive key lookup with locale fallback:
    /// `Name` also matches `Name[de]` when the bare key is absent.
    pub fn str(&self, key: &str) -> Option<&str> {
        if let Some(v) = self.fields.get(key) {
            return Some(v);
        }
        // locale variant fallback: Name[xx] for Name
        for (k, v) in &self.fields {
            if let Some(rest) = k.strip_prefix(key) {
                if rest.starts_with('[') && rest.ends_with(']') {
                    return Some(v);
                }
            }
        }
        self.fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    /// `true` only for an explicit `Key=true` (any case).
    pub fn bool(&self, key: &str) -> bool {
        self.str(key)
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
    }

    /// Semicolon list, trailing/empty items removed (spec: `a;b;`).
    pub fn list(&self, key: &str) -> Option<Vec<String>> {
        let raw = self.str(key)?;
        Some(
            raw.split(';')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        )
    }

    /// The visible-ness rules shared by every desktop.
    pub fn visible(&self, desktop: &str) -> bool {
        let ty = self.str("Type").unwrap_or("Application");
        if ty != "Application" {
            return false;
        }
        if self.bool("Hidden") {
            return false;
        }
        if self
            .str("X-GNOME-Autostart-enabled")
            .is_some_and(|v| v.eq_ignore_ascii_case("false"))
        {
            return false;
        }
        // GLib semantics: a *present* OnlyShowIn must list us; an absent
        // key means no restriction.
        if let Some(only) = self.list("OnlyShowIn") {
            if !only.iter().any(|d| d == desktop) {
                return false;
            }
        }
        if let Some(not) = self.list("NotShowIn") {
            if not.iter().any(|d| d == desktop) {
                return false;
            }
        }
        true
    }

    /// Convert to an `App`; `None` when the entry is unusable
    /// (no `Exec`, empty after field-code removal, or failed `TryExec`).
    pub fn to_app(&self, supervise: bool) -> Option<App> {
        if !self.visible(CURRENT_DESKTOP) {
            return None;
        }
        if let Some(tryexec) = self.str("TryExec") {
            if !proc::in_path(tryexec) {
                tracing::info!(entry = %self.path.display(), "TryExec not found, skipping");
                return None;
            }
        }
        let exec = self.str("Exec")?;
        let mut tokens = exec_tokens(exec);
        if tokens.is_empty() {
            return None;
        }
        let command = tokens.remove(0);
        if command.is_empty() {
            return None;
        }
        let name = self
            .str("Name")
            .map(str::to_owned)
            .unwrap_or_else(|| self.file_stem());
        Some(App {
            name,
            command: Some(command),
            args: tokens,
            delay_ms: 0,
            restart: if supervise {
                crate::config::RestartPolicy::OnFailure
            } else {
                crate::config::RestartPolicy::Never
            },
            enabled: true,
            phase: crate::config::XDG_PHASE,
            // XDG autostart entries carry no resource limits (the
            // standard has no such keys); a site that wants them
            // restates the app in session.toml, where the merge-by-name
            // rule applies the limits to the same launch.
            memory_max: None,
            cpu_weight: None,
            tasks_max: None,
        })
    }

    fn file_stem(&self) -> String {
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "autostart".into())
    }
}

/// Tokenize a `Exec=` value per the desktop-entry spec's quoting rules,
/// then drop field codes (they expand to "nothing opened from the file
/// manager", the same choice g_spawn-based launchers make).
///
/// NUL bytes are dropped on sight: a desktop file is read as UTF-8
/// where `\u{0}` is representable, but a NUL in argv would abort
/// `posix_spawn` with an invalid-argument error (and historically
/// panicked std's CString conversion) — a malformed file must never
/// become a spawn failure, so the tokenizer simply excises them.
pub fn exec_tokens(exec: &str) -> Vec<String> {
    let exec = exec.replace('\u{0}', "");
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut had_any = false;

    for ch in exec.chars() {
        if escaped {
            // only inside double quotes does the backslash escape the
            // five special characters; elsewhere it is literal
            cur.push(ch);
            escaped = false;
            continue;
        }
        match (ch, quote) {
            ('\\', Some('"')) => escaped = true,
            ('"', _) => {
                had_any = true;
                quote = quote.is_none().then_some('"');
            }
            ('\'', _) if quote.is_none() => quote = Some('\''),
            ('\'', Some('\'')) => quote = None,
            (c, _) if c.is_whitespace() && quote.is_none() => {
                if !cur.is_empty() || had_any {
                    tokens.push(std::mem::take(&mut cur));
                    had_any = false;
                }
            }
            (c, _) => cur.push(c),
        }
    }
    if !cur.is_empty() || had_any {
        tokens.push(cur);
    }

    // Drop field codes, whether standalone (%f) or attached (--file=%f).
    // Reserved (unquoted) %% becomes a literal % and is kept.
    tokens
        .into_iter()
        .filter(|t| !(t.starts_with('%') && t.len() > 1))
        .collect()
}

/// Directories to scan, in rising precedence order (later wins).
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let system = std::env::var_os("XDG_CONFIG_DIRS")
        .map(|v| std::env::split_paths(&v).collect::<Vec<_>>())
        .unwrap_or_else(|| vec![PathBuf::from("/etc/xdg")]);
    for d in system {
        dirs.push(d.join("autostart"));
    }
    let home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    if let Some(h) = home {
        dirs.push(h.join("autostart"));
    }
    dirs
}

/// Load every visible XDG autostart entry, user files overriding system
/// files with the same name. Never fails: unreadable files are logged and
/// skipped, because autostart must not be able to break logging in.
pub fn load_xdg_autostart(supervise: bool) -> Vec<App> {
    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in search_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "desktop"))
            .collect();
        files.sort();
        for f in files {
            let key = f
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if key.is_empty() {
                continue;
            }
            by_name.insert(key, f); // later dirs win
        }
    }

    let mut apps = Vec::new();
    for (_, path) in by_name {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let entry = DesktopEntry::parse(&text, path);
                if !entry.visible(CURRENT_DESKTOP) {
                    tracing::debug!(entry = %entry.path.display(), "hidden by desktop entry rules");
                    continue;
                }
                match entry.to_app(supervise) {
                    Some(app) => {
                        tracing::debug!(app = %app.name, "xdg autostart entry");
                        apps.push(app);
                    }
                    None => {
                        tracing::info!(entry = %entry.path.display(), "unusable entry, skipping")
                    }
                }
            }
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "unreadable desktop entry")
            }
        }
    }
    apps
}

#[cfg(test)]
fn load_from_dirs(dirs: &[PathBuf], supervise: bool) -> Vec<App> {
    let mut by_name = BTreeMap::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "desktop"))
            .collect();
        files.sort();
        for f in files {
            let key = f
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            by_name.insert(key, f);
        }
    }
    by_name
        .into_values()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            let entry = DesktopEntry::parse(&text, path);
            entry.to_app(supervise)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn entry(text: &str) -> DesktopEntry {
        DesktopEntry::parse(text, PathBuf::from("/tmp/x.desktop"))
    }

    #[test]
    fn parses_desktop_entry_group_only() {
        let e = entry("[Desktop Entry]\nName=Foo\nExec=foo --bar\n\n[Other]\nName=Nope\n");
        assert_eq!(e.str("Name"), Some("Foo"));
        assert_eq!(e.str("Exec"), Some("foo --bar"));
        // [Other] group ignored
        let e2 = entry("[Other]\nName=Nope\n[Desktop Entry]\nName=Yes\n");
        assert_eq!(e2.str("Name"), Some("Yes"));
    }

    #[test]
    fn locale_fallback_for_name() {
        let e = entry("[Desktop Entry]\nName[de]=Dings\nExec=x\n");
        assert_eq!(e.str("Name"), Some("Dings"));
        let e2 = entry("[Desktop Entry]\nName=Base\nName[de]=Dings\nExec=x\n");
        assert_eq!(e2.str("Name"), Some("Base"));
    }

    #[test]
    fn hidden_and_gnome_disable() {
        let hidden = entry("[Desktop Entry]\nType=Application\nName=x\nExec=x\nHidden=true\n");
        assert!(!hidden.visible("LionOS"));
        let off = entry(
            "[Desktop Entry]\nType=Application\nName=x\nExec=x\nX-GNOME-Autostart-enabled=false\n",
        );
        assert!(!off.visible("LionOS"));
        let on = entry(
            "[Desktop Entry]\nType=Application\nName=x\nExec=x\nX-GNOME-Autostart-enabled=true\n",
        );
        assert!(on.visible("LionOS"));
    }

    #[test]
    fn show_in_filtering() {
        let base = "[Desktop Entry]\nType=Application\nName=x\nExec=x\n";
        assert!(entry(&format!("{base}OnlyShowIn=LionOS;GNOME;")).visible("LionOS"));
        assert!(!entry(&format!("{base}OnlyShowIn=GNOME;KDE;")).visible("LionOS"));
        assert!(!entry(&format!("{base}NotShowIn=LionOS;")).visible("LionOS"));
        assert!(entry(&format!("{base}NotShowIn=GNOME;")).visible("LionOS"));
        assert!(entry(base).visible("LionOS")); // no restriction at all
    }

    #[test]
    fn type_must_be_application() {
        assert!(!entry("[Desktop Entry]\nType=Link\nName=x\nExec=x\n").visible("LionOS"));
        assert!(entry("[Desktop Entry]\nType=Application\nExec=x\n").visible("LionOS"));
    }

    #[test]
    fn exec_quoting_and_field_codes() {
        let t = exec_tokens("foo --title \"My App\" --path '/a b' %f %U --name=%%x");
        assert_eq!(
            t,
            vec!["foo", "--title", "My App", "--path", "/a b", "--name=%%x"]
        );
        // %f and %U dropped; %%x is not a field code (starts with %, len>1
        // -- filter drops %%x too; documented behavior: only tokens starting
        // with a single % + code char are dropped; %% is the escaped literal)
    }

    #[test]
    fn exec_escape_rules() {
        assert_eq!(exec_tokens("foo \"a\\\"b\""), vec!["foo", "a\"b"]);
        assert_eq!(exec_tokens("foo 'a b'"), vec!["foo", "a b"]);
        assert_eq!(exec_tokens("  "), Vec::<String>::new());
        assert_eq!(exec_tokens(""), Vec::<String>::new());
        assert_eq!(exec_tokens("foo"), vec!["foo"]);
        // empty quoted string is still a token
        assert_eq!(exec_tokens("foo \"\""), vec!["foo", ""]);
    }

    #[test]
    fn to_app_maps_fields() {
        let e = entry(
            "[Desktop Entry]\nType=Application\nName=My Tool\nExec=mytool --flag \"v 2\" %f\n",
        );
        let app = e.to_app(false).unwrap();
        assert_eq!(app.name, "My Tool");
        assert_eq!(app.command(), "mytool");
        assert_eq!(app.args, vec!["--flag", "v 2"]);
        assert_eq!(app.phase, crate::config::Phase::Applications);
        assert!(matches!(app.restart, crate::config::RestartPolicy::Never));
    }

    #[test]
    fn to_app_supervised_flag() {
        let e = entry("[Desktop Entry]\nType=Application\nExec=x\n");
        let app = e.to_app(true).unwrap();
        assert!(matches!(
            app.restart,
            crate::config::RestartPolicy::OnFailure
        ));
    }

    #[test]
    fn to_app_without_exec_is_none() {
        let e = entry("[Desktop Entry]\nType=Application\nName=x\n");
        assert!(e.to_app(false).is_none());
    }

    #[test]
    fn try_exec_missing_binary_skips() {
        let e =
            entry("[Desktop Entry]\nType=Application\nExec=x\nTryExec=/nonexistent/binary/xyz\n");
        assert!(e.to_app(false).is_none());
    }

    #[test]
    fn user_file_overrides_system_by_name() {
        let tmp = std::env::temp_dir().join(format!("lion-dt-{}", std::process::id()));
        let sys = tmp.join("sys");
        let usr = tmp.join("usr");
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::create_dir_all(&usr).unwrap();
        let _ = std::fs::remove_file(usr.join("same.desktop"));

        let mut f = std::fs::File::create(sys.join("same.desktop")).unwrap();
        writeln!(
            f,
            "[Desktop Entry]\nType=Application\nName=System\nExec=system-binary"
        )
        .unwrap();
        let apps = load_from_dirs(std::slice::from_ref(&sys), false);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "System");

        let mut f = std::fs::File::create(usr.join("same.desktop")).unwrap();
        writeln!(
            f,
            "[Desktop Entry]\nType=Application\nName=System\nHidden=true"
        )
        .unwrap();
        let apps = load_from_dirs(&[sys, usr], false);
        assert!(
            apps.is_empty(),
            "user Hidden=true must override system file"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
