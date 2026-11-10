//! Core types shared across Cerebrai crates.

/// Returns the crate name; placeholder until real functionality lands.
pub fn name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_name() {
        assert_eq!(name(), "cerebrai-core");
    }
}
