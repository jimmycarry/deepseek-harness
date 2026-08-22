//! Filesystem skill provider.
pub fn name() -> &'static str {
    "dsh-skill-filesystem"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
