use std::collections::BTreeMap;

/// Parse `--key=value` arguments; anything else is an error naming the argument.
pub fn parse(args: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for arg in args {
        let Some(rest) = arg.strip_prefix("--") else {
            return Err(format!("not an option: {arg}"));
        };
        let Some((key, value)) = rest.split_once('=') else {
            return Err(format!("missing value: {arg}"));
        };
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pairs() {
        let args = vec!["--name=a".to_string(), "--port=80".to_string()];
        let map = parse(&args).unwrap();
        assert_eq!(map["name"], "a");
        assert_eq!(map["port"], "80");
    }

    #[test]
    fn rejects_bare_words() {
        assert!(parse(&["name".to_string()]).is_err());
    }
}
