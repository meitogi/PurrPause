// Argv / command-line escaping shared between spawn code paths. The
// logic is pure Rust (no Win32 calls) so it lives outside `win32/` and
// its unit tests run under `cargo test` on Linux.

use std::ffi::OsStr;
use std::path::Path;

/// Build the `lpCommandLine` buffer for `CreateProcessAsUserW`.
///
/// The returned `Vec<u16>` is a NUL-terminated UTF-16 sequence
/// containing the exe path followed by each argument, whitespace-
/// separated, with quoting rules matching `CommandLineToArgvW` (Windows'
/// own parser). The buffer must be mutable — Win32 may write to it.
///
/// Rules (per MSDN's "Parsing C Command-Line Arguments") :
///   - 2n backslashes then `"` → n backslashes + `"` starts/ends a quoted section
///   - 2n+1 backslashes then `"` → n backslashes + a literal `"`
///   - n backslashes NOT before `"` → n literal backslashes
/// Arguments containing space / tab / quote are wrapped in `"..."`.
/// Empty arguments become `""`.
pub fn build_command_line(exe: &Path, args: &[&OsStr]) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::new();
    append_escaped(&mut out, exe.as_os_str());
    for arg in args {
        out.push(b' ' as u16);
        append_escaped(&mut out, arg);
    }
    out.push(0);
    out
}

fn append_escaped(out: &mut Vec<u16>, arg: &OsStr) {
    let wide: Vec<u16> = os_str_to_wide(arg);
    let needs_quotes = wide.is_empty()
        || wide
            .iter()
            .any(|c| *c == ' ' as u16 || *c == '\t' as u16 || *c == '"' as u16);

    if !needs_quotes {
        out.extend_from_slice(&wide);
        return;
    }

    out.push(b'"' as u16);

    let mut backslashes: usize = 0;
    for &c in &wide {
        if c == '\\' as u16 {
            backslashes += 1;
            continue;
        }
        if c == '"' as u16 {
            for _ in 0..backslashes * 2 + 1 {
                out.push(b'\\' as u16);
            }
            out.push(b'"' as u16);
            backslashes = 0;
            continue;
        }
        for _ in 0..backslashes {
            out.push(b'\\' as u16);
        }
        backslashes = 0;
        out.push(c);
    }

    // Any trailing backslashes precede the closing quote — double them.
    for _ in 0..backslashes * 2 {
        out.push(b'\\' as u16);
    }
    out.push(b'"' as u16);
}

fn os_str_to_wide(s: &OsStr) -> Vec<u16> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().collect()
    }
    #[cfg(not(windows))]
    {
        s.to_string_lossy().encode_utf16().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::path::Path;

    fn decode(v: &[u16]) -> String {
        let end = v.iter().position(|c| *c == 0).unwrap_or(v.len());
        String::from_utf16_lossy(&v[..end])
    }

    #[test]
    fn no_args() {
        let cmd = build_command_line(Path::new("C:\\bin\\app.exe"), &[]);
        assert_eq!(decode(&cmd), "C:\\bin\\app.exe");
        assert_eq!(*cmd.last().unwrap(), 0);
    }

    #[test]
    fn simple_args() {
        let cmd = build_command_line(
            Path::new("app.exe"),
            &[OsStr::new("--popup"), OsStr::new("--verbose")],
        );
        assert_eq!(decode(&cmd), "app.exe --popup --verbose");
    }

    #[test]
    fn arg_with_space_gets_quoted() {
        let cmd =
            build_command_line(Path::new("app.exe"), &[OsStr::new("hello world")]);
        assert_eq!(decode(&cmd), "app.exe \"hello world\"");
    }

    #[test]
    fn exe_path_with_space_gets_quoted() {
        let cmd = build_command_line(
            Path::new("C:\\Program Files\\App\\app.exe"),
            &[OsStr::new("--x")],
        );
        assert_eq!(decode(&cmd), "\"C:\\Program Files\\App\\app.exe\" --x");
    }

    #[test]
    fn empty_arg_becomes_pair_of_quotes() {
        let cmd = build_command_line(Path::new("app.exe"), &[OsStr::new("")]);
        assert_eq!(decode(&cmd), "app.exe \"\"");
    }

    #[test]
    fn backslash_quote_escape() {
        // Arg `a\"b` (raw literal chars a, backslash, quote, b) — needs
        // quoting because of the `"`. Inside quotes : the backslash
        // before `"` is doubled and the `"` becomes `\"`.
        let cmd = build_command_line(Path::new("x"), &[OsStr::new("a\\\"b")]);
        assert_eq!(decode(&cmd), "x \"a\\\\\\\"b\"");
    }

    #[test]
    fn trailing_backslash_doubled_before_close_quote() {
        // `a b\` → wrap in quotes because of the space ; trailing
        // backslash doubled so it survives the closing quote.
        let cmd = build_command_line(Path::new("x"), &[OsStr::new("a b\\")]);
        assert_eq!(decode(&cmd), "x \"a b\\\\\"");
    }
}
