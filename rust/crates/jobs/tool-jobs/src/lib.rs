//! Model-facing job_* tools.
pub fn name() -> &'static str {
    "dsh-tool-jobs"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
