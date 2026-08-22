//! /compact command consumer.
pub fn name() -> &'static str {
    "dsh-command-compact"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
