//! Model-facing goal tool.
pub fn name() -> &'static str {
    "dsh-tool-goal"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
