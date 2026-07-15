// Guard test : no U+2014 em-dashes inside tracing macro string literals.
//
// Windows cmd.exe (code page 850/1252 on French Windows) misreads
// em-dash bytes on `Get-Content` / `type`, producing "watchdog tick ??
// state.dat missing" instead of "- state.dat missing". Files ARE
// written as valid UTF-8 by tracing_appender, but the cmd.exe reader
// side mangles the display. Keep the convention pure-ASCII in tracing
// so log inspection stays legible on the target platform.

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// Walk src/*.rs and flag any line that both contains a tracing
    /// macro call (`info!(`, `warn!(` etc.) AND an em-dash AND a `"`
    /// (so we're inside a string literal, not a comment). The
    /// combination is stricter than a raw em-dash grep and reliably
    /// skips doc comments that legitimately mention `tracing::warn!`
    /// prose without a trailing paren.
    #[test]
    fn no_em_dashes_in_tracing_messages() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let root = Path::new(manifest_dir).join("src");
        let mut offenders = Vec::new();
        for entry in walkdir::WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // Skip this file itself : it obviously contains the byte we
            // grep for, in prose above.
            if entry.path().ends_with("tracing_lint.rs") {
                continue;
            }
            let src = match std::fs::read_to_string(entry.path()) {
                Ok(s) => s,
                Err(_) => continue,
            };
            for (n, line) in src.lines().enumerate() {
                let macro_call = line.contains("info!(")
                    || line.contains("warn!(")
                    || line.contains("error!(")
                    || line.contains("debug!(")
                    || line.contains("trace!(");
                if macro_call && line.contains('\u{2014}') && line.contains('"') {
                    offenders.push(format!("{}:{}", entry.path().display(), n + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "em-dashes found inside tracing macro calls (use ASCII '-' instead):\n{}",
            offenders.join("\n"),
        );
    }
}
