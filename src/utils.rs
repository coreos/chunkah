use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

pub fn get_current_epoch() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time is before UNIX epoch")
        .map(|d| d.as_secs())
}

/// Returns the OCI/Go architecture string.
///
/// If `arch` is provided, translates it to OCI format.
/// Otherwise, uses the current system architecture.
pub fn get_goarch(arch: Option<&str>) -> &str {
    match arch.unwrap_or(std::env::consts::ARCH) {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "powerpc64" => "ppc64le",
        arch => arch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_goarch() {
        assert_eq!(get_goarch(Some("x86_64")), "amd64");
        assert_eq!(get_goarch(Some("aarch64")), "arm64");
        assert_eq!(get_goarch(Some("powerpc64")), "ppc64le");
        assert_eq!(get_goarch(Some("amd64")), "amd64"); // passthrough
        assert_eq!(get_goarch(Some("unknown")), "unknown"); // passthrough
    }
}
