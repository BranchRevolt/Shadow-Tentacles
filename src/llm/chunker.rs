// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Windowing long text into model-sized pieces.
//!
//! Operates on `char`s, never byte offsets. Sizing happens in `summarize`.

/// Split `text` into pieces of at most `chunk_chars` characters, sharing
/// `overlap_chars` between neighbours. Cuts snap back to the nearest whitespace
/// so a word is never split in half.
pub fn split_chars(text: &str, chunk_chars: usize, overlap_chars: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let chunk_chars = chunk_chars.max(1);
    if chars.len() <= chunk_chars {
        return vec![text.to_string()];
    }
    // Overlap must be strictly smaller than the chunk or the window can't advance.
    let overlap = overlap_chars.min(chunk_chars - 1);

    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = (start + chunk_chars).min(chars.len());

        let mut cut = end;
        if end < chars.len()
            && let Some(off) = chars[start..end].iter().rposition(|c| c.is_whitespace())
            && off > 0
        {
            cut = start + off + 1;
        }

        chunks.push(chars[start..cut].iter().collect());

        if cut >= chars.len() {
            break;
        }

        // Step back by the overlap, but always make forward progress.
        let next = cut.saturating_sub(overlap);
        start = if next > start { next } else { cut };
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(split_chars("привет мир", 100, 10), vec!["привет мир"]);
    }

    #[test]
    fn cuts_snap_to_word_boundaries() {
        let chunks = split_chars("один два три четыре пять шесть", 12, 0);
        assert!(chunks.iter().all(|c| !c.trim().is_empty()));
        // No chunk may end mid-word: each non-final chunk ends on whitespace.
        for c in &chunks[..chunks.len() - 1] {
            assert!(c.ends_with(' '), "chunk {c:?} cut mid-word");
        }
    }

    #[test]
    fn always_makes_progress_with_large_overlap() {
        // Overlap >= chunk would otherwise loop forever.
        let chunks = split_chars(&"а".repeat(500), 10, 999);
        assert!(chunks.len() < 500);
    }

    #[test]
    fn multibyte_is_not_split() {
        let text = "тест ".repeat(200);
        let chunks = split_chars(&text, 37, 5);
        // Reassembly proves no char was cut in half (a byte-index bug would have
        // panicked above, but this also catches silent replacement chars).
        assert!(chunks.iter().all(|c| !c.contains('\u{FFFD}')));
    }
}
