pub fn non_empty_string(value: String) -> Option<String> {
    non_empty_str(&value).map(ToOwned::to_owned)
}

pub fn non_empty_str(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_empty_string_trims_values() {
        assert_eq!(
            non_empty_string("  text  ".to_string()),
            Some("text".to_string())
        );
        assert_eq!(non_empty_string("   ".to_string()), None);
    }

    #[test]
    fn non_empty_str_trims_values() {
        assert_eq!(non_empty_str("  text  "), Some("text"));
        assert_eq!(non_empty_str("   "), None);
    }
}
