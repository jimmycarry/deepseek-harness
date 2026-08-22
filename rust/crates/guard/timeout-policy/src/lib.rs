//! tools/execute deadline enforcer.
pub fn name() -> &'static str {
    "dsh-timeout-policy"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
