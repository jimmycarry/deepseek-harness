//! Model-facing terminal tool.
pub fn name() -> &'static str {
    "dsh-tool-terminal"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
