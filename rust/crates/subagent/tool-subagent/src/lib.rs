//! Model-facing subagent tool.
pub fn name() -> &'static str {
    "dsh-tool-subagent"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
