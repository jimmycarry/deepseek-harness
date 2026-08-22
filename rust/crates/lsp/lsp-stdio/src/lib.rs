//! Stdio LSP provider.
pub fn name() -> &'static str {
    "dsh-lsp-stdio"
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
    }
}
