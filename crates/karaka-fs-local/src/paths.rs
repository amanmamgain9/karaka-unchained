//! Node's POSIX path functions, on strings, as dsh calls them: `resolve`,
//! `dirname`, `basename` and `pathToFileURL`'s escaping.
//!
//! `std::path` differs where it matters: `Path::new("/a/..").parent()` is
//! `/a`, and its `file_name()` is `None`, where Node's are `/a` and `..`.
//! The local backend keeps `..` in a display path on purpose, so these
//! follow Node exactly.

pub(crate) fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
}

/// The process's working directory. (Node: `process.cwd()`.)
pub(crate) fn process_cwd() -> String {
    std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".into())
}

/// Whether a segment of `path`, split at `/` or `\`, is `..`. (dsh's
/// `/(?:^|[\\/])\.\.(?:[\\/]|$)/u`.)
pub(crate) fn has_parent_segment(path: &str) -> bool {
    path.split(['/', '\\']).any(|segment| segment == "..")
}

/// An absolute path with `.` and empty segments dropped and `..` taken
/// lexically, no trailing slash. (Node: `normalize` of an absolute path.)
pub(crate) fn normalize_absolute(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            segment => parts.push(segment),
        }
    }
    format!("/{}", parts.join("/"))
}

/// `path` resolved against `cwd`, itself resolved against the process's
/// directory. (Node: `resolve(cwd, path)`.)
pub(crate) fn resolve(cwd: &str, path: &str) -> String {
    if is_absolute(path) {
        return normalize_absolute(path);
    }
    let base = if is_absolute(cwd) {
        cwd.to_owned()
    } else {
        format!("{}/{cwd}", process_cwd())
    };
    normalize_absolute(&format!("{base}/{path}"))
}

/// Node's POSIX `dirname`.
pub(crate) fn dirname(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return ".".into();
    }
    let has_root = bytes[0] == b'/';
    let mut end = None;
    let mut matched_slash = true;
    for index in (1..bytes.len()).rev() {
        if bytes[index] == b'/' {
            if !matched_slash {
                end = Some(index);
                break;
            }
        } else {
            matched_slash = false;
        }
    }
    match end {
        None if has_root => "/".into(),
        None => ".".into(),
        Some(1) if has_root => "//".into(),
        Some(end) => path[..end].to_owned(),
    }
}

/// Node's POSIX `basename`, without an extension to strip.
pub(crate) fn basename(path: &str) -> &str {
    let bytes = path.as_bytes();
    let mut start = 0;
    let mut end = None;
    let mut matched_slash = true;
    for index in (0..bytes.len()).rev() {
        if bytes[index] == b'/' {
            if !matched_slash {
                start = index + 1;
                break;
            }
        } else if end.is_none() {
            matched_slash = false;
            end = Some(index + 1);
        }
    }
    match end {
        None => "",
        Some(end) => &path[start..end],
    }
}

/// An absolute path's `file:` URL, escaped as Node's `pathToFileURL`
/// escapes it: the URL path set, plus `%`, `\`, and the C0 controls.
pub(crate) fn file_url(path: &str) -> String {
    let mut url = String::from("file://");
    for byte in path.bytes() {
        let escaped = byte <= 0x20
            || byte >= 0x7f
            || matches!(
                byte,
                b'"' | b'#' | b'%' | b'<' | b'>' | b'?' | b'`' | b'{' | b'}' | b'\\'
            );
        if escaped {
            url.push_str(&format!("%{byte:02X}"));
        } else {
            url.push(char::from(byte));
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirname_and_basename_are_nodes() {
        for (path, dir, base) in [
            ("/a/b", "/a", "b"),
            ("/a/b/", "/a", "b"),
            ("/a/x/..", "/a/x", ".."),
            ("/a", "/", "a"),
            ("/", "/", ""),
            ("a", ".", "a"),
            ("/a//b", "/a/", "b"),
        ] {
            assert_eq!(dirname(path), dir, "dirname({path})");
            assert_eq!(basename(path), base, "basename({path})");
        }
    }

    #[test]
    fn resolve_normalizes_as_node_does() {
        assert_eq!(resolve("/cwd", "a/./b/"), "/cwd/a/b");
        assert_eq!(resolve("/cwd", "/abs//x"), "/abs/x");
        assert_eq!(resolve("/cwd", "a/../b"), "/cwd/b");
        assert!(has_parent_segment("/a/../b"));
        assert!(has_parent_segment("a\\..\\b"));
        assert!(!has_parent_segment("/a/..b/c"));
    }
}
