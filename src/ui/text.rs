//! Text wrapping shared by the message view.

use unicode_width::UnicodeWidthStr;

/// Greedily wrap `text` to `width` display columns, honouring existing line breaks.
///
/// Words longer than the available width (URLs, for instance) are split rather than allowed to
/// overflow the pane.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut current = String::new();

        for word in paragraph.split(' ') {
            if word.is_empty() {
                continue;
            }

            let candidate_width =
                current.width() + if current.is_empty() { 0 } else { 1 } + word.width();
            if candidate_width <= width {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
                continue;
            }

            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }

            if word.width() <= width {
                current.push_str(word);
            } else {
                lines.extend(split_wide_word(word, width));
                // `split_wide_word` leaves the trailing remainder to keep filling.
                current = lines.pop().unwrap_or_default();
            }
        }

        lines.push(current);
    }

    lines
}

/// Break a word that cannot fit on one line into width-sized chunks.
fn split_wide_word(word: &str, width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut chunk = String::new();

    for ch in word.chars() {
        let ch_width = ch.to_string().width();
        if chunk.width() + ch_width > width {
            chunks.push(std::mem::take(&mut chunk));
        }
        chunk.push(ch);
    }
    chunks.push(chunk);
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_on_word_boundaries() {
        assert_eq!(wrap("the quick brown fox", 10), ["the quick", "brown fox"]);
    }

    #[test]
    fn keeps_explicit_line_breaks() {
        assert_eq!(wrap("one\ntwo", 10), ["one", "two"]);
    }

    #[test]
    fn splits_words_wider_than_the_pane() {
        assert_eq!(wrap("abcdefgh", 3), ["abc", "def", "gh"]);
    }

    #[test]
    fn never_exceeds_the_width() {
        let text = "a bb ccc dddd eeeee ffffff ggggggg";
        for width in 1..12 {
            for line in wrap(text, width) {
                assert!(line.width() <= width, "{line:?} exceeds {width}");
            }
        }
    }
}
