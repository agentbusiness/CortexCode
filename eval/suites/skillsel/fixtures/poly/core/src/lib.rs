/// Split text into words (renamed from `tokenize_words` in 0.2.0).
pub fn split_words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits() {
        assert_eq!(split_words(" a  b "), vec!["a", "b"]);
    }
}
