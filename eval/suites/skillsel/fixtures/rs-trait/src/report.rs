/// One line describing an item for the report.
pub fn describe<T>(item: T) -> String {
    format!("item: {}", item)
}

/// Comma-separated values.
pub fn summary(values: &[f64]) -> String {
    values.iter().map(|v| format!("{v:.2}")).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_numbers() {
        assert_eq!(describe(7), "item: 7");
    }
}
