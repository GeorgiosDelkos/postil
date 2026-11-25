//! The free space on a volume, which keeps the threads' worktrees in bounds (docs/design.md,
//! "Keeping the disk in bounds").

use std::io;
use std::path::Path;

/// The bytes free on the volume that holds `path`, which must exist: the larger of what macOS
/// would free for an important use (`NSURLVolumeAvailableCapacityForImportantUsageKey`, which
/// counts the space it can purge, caches and local snapshots, as Finder does) and what is
/// free now (`NSURLVolumeAvailableCapacityKey`, as `statfs` counts it). The first is the
/// measure; it reads 0 in a process sandboxed away from the service that computes it (one
/// under Claude Code's sandbox, measured 2026-11-25, where the second read 339 GB), and it is
/// never below the second, so the larger is right in both. Free space, not `du`, which counts a
/// block shared by clones once per clone.
///
/// # Errors
///
/// When `path` cannot be made a file URL, Foundation's error, and a value that is not a
/// number.
#[cfg(target_os = "macos")]
pub(crate) fn available(path: &Path) -> io::Result<u64> {
    use objc2_foundation::{NSArray, NSNumber, NSString, NSURL};

    let url = NSURL::from_file_path(path)
        .ok_or_else(|| io::Error::other(format!("{} cannot be made a file URL", path.display())))?;
    // The keys' values are their names (checked with Swift on macOS 27.2): reading the
    // framework's constants takes `unsafe`, which the crate denies outside `clone.rs`.
    let keys = [
        NSString::from_str("NSURLVolumeAvailableCapacityForImportantUsageKey"),
        NSString::from_str("NSURLVolumeAvailableCapacityKey"),
    ];
    let values = url
        .resourceValuesForKeys_error(&NSArray::from_retained_slice(&keys))
        .map_err(|error| io::Error::other(error.localizedDescription().to_string()))?;
    let mut free = None;
    for key in &keys {
        let Some(value) = values.objectForKey(key) else {
            continue;
        };
        let number = value
            .downcast::<NSNumber>()
            .map_err(|_| io::Error::other(format!("{key} is not a number")))?;
        free = free.max(Some(number.unsignedLongLongValue()));
    }
    // Neither value is not 0 bytes free: it is a volume that cannot be measured.
    free.ok_or_else(|| {
        io::Error::other(format!(
            "the volume of {} reports no free space",
            path.display()
        ))
    })
}

/// Off macOS the space is not measured, and the floor is not kept.
#[cfg(not(target_os = "macos"))]
pub(crate) fn available(_path: &Path) -> io::Result<u64> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn the_free_space_is_at_least_what_statfs_counts_and_below_the_volume_s_size() {
        let dir = std::env::temp_dir();
        // `df -k` prints the size and the space free now in KiB, as `statfs` counts them.
        let output = std::process::Command::new("df")
            .arg("-k")
            .arg(&dir)
            .output()
            .unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        let fields: Vec<u64> = text
            .lines()
            .nth(1)
            .unwrap()
            .split_whitespace()
            .filter_map(|field| field.parse().ok())
            .collect();
        let (size, free_now) = (fields[0] * 1024, fields[2] * 1024);

        let free = available(&dir).unwrap();

        // Other processes write meanwhile: a margin of 1 GB.
        assert!(free + 1_000_000_000 >= free_now, "{free} < {free_now}");
        assert!(free <= size, "{free} > {size}");
    }
}
