/// UI helpers shared between the TUI and plain-stdout modes.

// ── Tool glyphs ───────────────────────────────────────────────────────────────

pub fn tool_glyph(tool_name: &str) -> &'static str {
    match tool_name {
        "read_file"  => "○",
        "write_file" => "●",
        "edit_file"  => "◈",
        "bash"       => "❯",
        "search"     => "⌕",
        "list_files" => "≡",
        _            => "⚙",
    }
}
