//! dsh-base patch layer.
pub fn name() -> &'static str {
    "dsh-bundle-base"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
