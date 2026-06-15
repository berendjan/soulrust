/// Application version, the single source of truth for the self-updater's
/// "is there something newer" comparison. Keep in sync with Cargo.toml by
/// convention (Cargo's version is only crate metadata under Bazel).
pub const VERSION: &str = "0.1.0";
