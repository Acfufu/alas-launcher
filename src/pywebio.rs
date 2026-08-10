//! PyWebIO protocol helpers for ALAS scheduler control (macOS tray).
//!
//! Currently holds only the runtime version guard: read the pinned
//! `pywebio` version from the payload `requirements.txt` and check it
//! against the protocol this launcher speaks. The WebSocket client that
//! consumes this guard arrives in a later change.

/// Parse the pinned pywebio version from a requirements.txt payload.
///
/// Scans lines for `pywebio==<version>`, tolerating trailing comments such
/// as `# via -r requirements-in.txt`. Matching is case-sensitive; a missing
/// or malformed line yields `None`.
pub(crate) fn pywebio_version(requirements_txt: &str) -> Option<String> {
    requirements_txt.lines().find_map(|line| {
        let version = line.trim().strip_prefix("pywebio==")?;
        let version: String = version
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if version.is_empty() {
            return None;
        }
        Some(version)
    })
}

/// True when the pywebio version matches the protocol this launcher speaks.
///
/// Only `Some("1.6.2")` passes; anything else (including `None`) is a
/// mismatch the caller warns about. Purely advisory, never blocking.
pub(crate) fn check_pywebio_version(v: Option<&str>) -> bool {
    v == Some("1.6.2")
}

#[cfg(test)]
mod tests {
    use super::{check_pywebio_version, pywebio_version};

    const REAL_SAMPLE: &str = "pywebio==1.6.2            # via -r requirements-in.txt";

    #[test]
    fn parses_real_payload_line_with_trailing_comment() {
        assert_eq!(pywebio_version(REAL_SAMPLE).as_deref(), Some("1.6.2"));
    }

    #[test]
    fn parses_plain_line() {
        assert_eq!(pywebio_version("pywebio==1.6.2\n").as_deref(), Some("1.6.2"));
    }

    #[test]
    fn parses_multiline_file() {
        let txt = "tornado==6.1\npywebio==1.6.2\nuser-agents==2.2.0\n";
        assert_eq!(pywebio_version(txt).as_deref(), Some("1.6.2"));
    }

    #[test]
    fn is_case_sensitive() {
        assert_eq!(pywebio_version("PyWebIO==1.6.2"), None);
        assert_eq!(pywebio_version("PYWEBIO==1.6.2"), None);
    }

    #[test]
    fn missing_pywebio_line_yields_none() {
        assert_eq!(pywebio_version("tornado==6.1\nuser-agents==2.2.0\n"), None);
        assert_eq!(pywebio_version(""), None);
    }

    #[test]
    fn malformed_lines_yield_none() {
        assert_eq!(pywebio_version("pywebio==\n"), None);
        assert_eq!(pywebio_version("pywebio 1.6.2\n"), None);
        assert_eq!(pywebio_version("pywebio>=1.6.2\n"), None);
    }

    #[test]
    fn guard_matrix() {
        assert!(check_pywebio_version(Some("1.6.2")));
        assert!(!check_pywebio_version(Some("1.6.3")));
        assert!(!check_pywebio_version(Some("2.0.0")));
        assert!(!check_pywebio_version(None));
        assert!(check_pywebio_version(pywebio_version(REAL_SAMPLE).as_deref()));
    }
}
