//! Local jobs provider.
pub fn name() -> &'static str {
    "dsh-jobs-local"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
