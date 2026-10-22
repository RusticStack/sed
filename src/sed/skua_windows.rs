// Skua Bash (RusticStack fork): what Git Bash's sed (an MSYS program) does on Windows.
//
// SPDX-License-Identifier: MIT
//
// Skua Bash runs this sed as a native Windows program where Git Bash runs GNU sed under MSYS.
// Two MSYS behaviours that agents rely on are reproduced here (Skua M3-SH9):
// - file names inside a script (`r`, `R`, `w`, `W`, `s///w`) may be POSIX paths: `/tmp/x` is
//   the temp dir (as Skua Bash maps it for arguments), `/c/x` is `C:\x`, `/dev/null` is `NUL`,
//   and `/dev/stdout` / `/dev/stderr` are the standard streams;
// - input is read in text mode: a line ending in `\r\n` ends like one ending in `\n`.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// Maps a file name written in a script the way MSYS sed in Git Bash resolves it. Other names,
/// and every name off Windows, are returned unchanged.
pub fn script_path(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        if let Some(mapped) = path.to_str().and_then(msys_path) {
            return mapped;
        }
    }
    path
}

fn msys_path(s: &str) -> Option<PathBuf> {
    if s == "/dev/null" {
        return Some(PathBuf::from("NUL"));
    }
    if let Some(rest) = s.strip_prefix("/tmp") {
        if rest.is_empty() || rest.starts_with('/') {
            let mut out = std::env::temp_dir();
            let rest = rest.trim_start_matches('/');
            if !rest.is_empty() {
                out.push(rest.replace('/', "\\"));
            }
            return Some(out);
        }
    }
    let bytes = s.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b'/' {
        // `/c/x` only when drive C: exists, as Skua Bash converts arguments.
        let drive = format!("{}:\\", char::from(bytes[1]).to_ascii_uppercase());
        if Path::new(&drive).exists() {
            return Some(PathBuf::from(format!(
                "{drive}{}",
                s[3..].replace('/', "\\")
            )));
        }
    }
    None
}

/// On Windows, `/dev/stdout` and `/dev/stderr` as a `w` file: a duplicate of that stream's
/// handle. `None` for any other name (and off Windows, where the device files exist).
pub fn std_stream(path: &Path) -> Option<io::Result<File>> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        let handle = match path.to_str()? {
            "/dev/stdout" => io::stdout().as_handle().try_clone_to_owned(),
            "/dev/stderr" => io::stderr().as_handle().try_clone_to_owned(),
            _ => return None,
        };
        Some(handle.map(File::from))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// Text-mode input: drops the `\r` of a line that ended in `\r\n` (its `\n` already removed).
pub fn strip_cr(line: &mut Vec<u8>) {
    if cfg!(windows) && line.ends_with(b"\r") {
        line.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_msys_paths_on_windows_only() {
        let tmp = script_path(PathBuf::from("/tmp/sh9.txt"));
        let null = script_path(PathBuf::from("/dev/null"));
        let other = script_path(PathBuf::from("out.txt"));
        if cfg!(windows) {
            assert_eq!(tmp, std::env::temp_dir().join("sh9.txt"));
            assert_eq!(null, PathBuf::from("NUL"));
            let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
            let letter = sys.chars().next().unwrap().to_ascii_lowercase();
            assert_eq!(
                script_path(PathBuf::from(format!("/{letter}/x/y.txt"))),
                PathBuf::from(format!("{}:\\x\\y.txt", letter.to_ascii_uppercase()))
            );
            assert_eq!(script_path(PathBuf::from("/tmpx")), PathBuf::from("/tmpx"));
        } else {
            assert_eq!(tmp, PathBuf::from("/tmp/sh9.txt"));
            assert_eq!(null, PathBuf::from("/dev/null"));
        }
        assert_eq!(other, PathBuf::from("out.txt"));
    }

    #[test]
    fn strips_one_cr_on_windows_only() {
        let mut line = b"a\r".to_vec();
        strip_cr(&mut line);
        let expected: &[u8] = if cfg!(windows) { b"a" } else { b"a\r" };
        assert_eq!(line, expected);
    }
}
