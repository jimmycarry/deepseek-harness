//! Local PTY provider.
pub fn name() -> &'static str {
    "dsh-terminal-local"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
