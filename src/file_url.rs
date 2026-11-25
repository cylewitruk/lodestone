//! `file://` URLs, as harnesses send local paths.

use std::path::PathBuf;

use url::Url;

/// A path, given plainly or as a `file://` URL (newer Codex versions record command directories
/// as URLs; MCP clients name their roots by them). A URL this platform cannot map to a path (a
/// remote host) is taken as given.
pub fn local_path(s: &str) -> PathBuf {
    Url::parse(s)
        .ok()
        .filter(|u| u.scheme() == "file")
        .and_then(|u| u.to_file_path().ok())
        .unwrap_or_else(|| PathBuf::from(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_and_urls() {
        assert_eq!(local_path("/a/b"), PathBuf::from("/a/b"));
        assert_eq!(local_path("C:\\a\\b"), PathBuf::from("C:\\a\\b"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_urls() {
        for url in ["file:///a/my%20repo", "file://localhost/a/my%20repo"] {
            assert_eq!(local_path(url), PathBuf::from("/a/my repo"), "{url}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_urls() {
        for url in ["file:///C:/a/my%20repo", "file://localhost/C:/a/my%20repo"] {
            assert_eq!(local_path(url), PathBuf::from("C:\\a\\my repo"), "{url}");
        }
        assert_eq!(
            local_path("file://server/share/a"),
            PathBuf::from("\\\\server\\share\\a")
        );
    }
}
