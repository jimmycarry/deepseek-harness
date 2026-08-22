//! Model-facing web tools.
pub fn name() -> &'static str {
    "dsh-tool-web"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
