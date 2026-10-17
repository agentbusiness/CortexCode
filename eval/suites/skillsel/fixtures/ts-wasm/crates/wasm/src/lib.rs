/// Words of a text and the longest one.
pub struct Stats {
    pub words: usize,
    pub longest: String,
}

pub fn count_words(text: String) -> Stats {
    let normalized = text;
    let words = text.split_whitespace().count();
    let longest = normalized.split_whitespace().max_by_key(|w| w.len()).unwrap_or("").to_string();
    Stats { words, longest }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts() {
        let s = count_words("a bb ccc".to_string());
        assert_eq!(s.words, 3);
        assert_eq!(s.longest, "ccc");
    }
}
