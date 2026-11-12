//! Per-session markers in `<data dir>/hook-sessions/<session id>`, written by the hooks: which hook
//! last recorded the session, and the session's directory. `lode mcp` serves that directory, since
//! Claude Code in the desktop app starts MCP servers in the main checkout of a worktree session.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Markers older than this are removed when one is written.
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    /// The hook event that wrote it.
    pub event: String,
    /// The session's directory; `None` in a marker from a build that recorded only the event.
    pub cwd: Option<PathBuf>,
}

/// Where session `session`'s marker lives; `None` for an id with no usable characters.
pub fn path(data_dir: &Path, session: &str) -> Option<PathBuf> {
    let id: String = session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    (!id.is_empty()).then(|| data_dir.join("hook-sessions").join(id))
}

/// The marker at `path`, if one was written.
pub fn read(path: &Path) -> Option<Marker> {
    let text = fs::read_to_string(path).ok()?;
    Some(serde_json::from_str(&text).unwrap_or(Marker {
        event: text.trim().to_string(),
        cwd: None,
    }))
}

/// Writes `marker` and removes markers older than [`MAX_AGE`].
pub fn write(path: &Path, marker: &Marker) -> anyhow::Result<()> {
    let dir = path.parent().expect("marker has a parent");
    fs::create_dir_all(dir)?;
    fs::write(path, serde_json::to_string(marker)?)?;
    let now = SystemTime::now();
    for entry in fs::read_dir(dir)?.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > MAX_AGE);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tempdir;

    #[test]
    fn markers_round_trip_and_read_the_event_only_format() {
        let dir = tempdir();
        let p = path(&dir, "a/../b c").unwrap();
        assert_eq!(p, dir.join("hook-sessions/abc"));
        assert_eq!(read(&p), None);
        let m = Marker {
            event: "UserPromptSubmit".into(),
            cwd: Some("/w".into()),
        };
        write(&p, &m).unwrap();
        assert_eq!(read(&p), Some(m));
        fs::write(&p, "SessionStart").unwrap();
        assert_eq!(
            read(&p),
            Some(Marker {
                event: "SessionStart".into(),
                cwd: None
            })
        );
        assert_eq!(path(&dir, "../"), None);
    }
}
