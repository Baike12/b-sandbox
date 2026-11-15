//! b-sandbox REST service entry point.
//!
//! Placeholder until the API module lands; see docs/plan.md module M-8.
fn main() {
    println!("sandbox-api scaffold");
}

#[cfg(test)]
mod tests {
    #[test]
    fn scaffold_links_core() {
        assert!(sandbox_core::workspace_is_wired());
    }
}
