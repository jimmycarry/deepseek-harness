//! HTTP fetch provider.
pub fn name() -> &'static str {
    "dsh-web-fetch-http"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
