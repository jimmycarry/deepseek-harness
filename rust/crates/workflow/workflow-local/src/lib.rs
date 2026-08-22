//! Local workflow provider.
pub fn name() -> &'static str {
    "dsh-workflow-local"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
