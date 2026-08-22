//! In-process subagent provider.
pub fn name() -> &'static str {
    "dsh-subagent-inprocess"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
