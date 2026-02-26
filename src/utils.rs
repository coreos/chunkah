use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

pub fn get_current_epoch() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time is before UNIX epoch")
        .map(|d| d.as_secs())
}

/// Parse an RFC 3339 timestamp string into a Unix epoch (seconds).
pub fn parse_rfc3339_epoch(s: &str) -> Result<u64> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("parsing RFC 3339 timestamp: {s}"))?;
    u64::try_from(dt.timestamp()).with_context(|| format!("timestamp is negative: {s}"))
}

/// Format a byte count as a human-readable string using binary units.
pub fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * KIB;
    const GIB: f64 = 1024.0 * MIB;

    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.1} GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.1} MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else {
        format!("{} B", bytes)
    }
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
    fn test_parse_rfc3339_epoch() {
        assert_eq!(
            parse_rfc3339_epoch("2023-11-14T22:13:20Z").unwrap(),
            1700000000
        );
        assert_eq!(parse_rfc3339_epoch("1970-01-01T00:00:00Z").unwrap(), 0);
        assert!(parse_rfc3339_epoch("not-a-date").is_err());
        assert!(parse_rfc3339_epoch("1969-12-31T23:59:59Z").is_err());
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(1048576), "1.0 MiB");
        assert_eq!(format_size(1073741824), "1.0 GiB");
        assert_eq!(format_size(1610612736), "1.5 GiB");
    }

    #[test]
    fn test_get_goarch() {
        assert_eq!(get_goarch(Some("x86_64")), "amd64");
        assert_eq!(get_goarch(Some("aarch64")), "arm64");
        assert_eq!(get_goarch(Some("powerpc64")), "ppc64le");
        assert_eq!(get_goarch(Some("amd64")), "amd64"); // passthrough
        assert_eq!(get_goarch(Some("unknown")), "unknown"); // passthrough
    }
}
