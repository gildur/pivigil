use anyhow::{Context, Result};
use std::{
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, fs::DirBuilderExt},
    path::Path,
};

pub fn prepare_recording(settings: &crate::config::Recording) -> Result<()> {
    if settings.enabled {
        prepare(&settings.output_dir)
    } else {
        Ok(())
    }
}

pub fn prepare(output_dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o750)
        .create(output_dir)
        .with_context(|| format!("create output directory {}", output_dir.display()))
}

pub fn free_bytes(path: &Path) -> Result<u64> {
    let path = CString::new(path.as_os_str().as_bytes()).context("output path contains NUL")?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is NUL-terminated and stat points to writable, correctly sized memory.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("query recording disk space");
    }
    // SAFETY: successful statvfs initialized stat.
    let stat = unsafe { stat.assume_init() };
    Ok((stat.f_bavail as u128 * stat.f_frsize as u128).min(u64::MAX as u128) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn disabled_recording_does_not_touch_output_storage() {
        let settings = crate::config::Recording {
            enabled: false,
            output_dir: "/dev/null/unavailable".into(),
            ..Default::default()
        };
        prepare_recording(&settings).unwrap();
    }

    #[test]
    fn preparing_storage_is_repeatable_and_only_creates_the_directory() {
        let root = std::env::temp_dir().join(format!(
            "pivigil-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        prepare(&root).unwrap();
        assert!(root.is_dir());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        prepare(&root).unwrap();
        assert!(free_bytes(&root).unwrap() > 0);
        let recorded = root.join("2026-11-10_21-48-04.mp4");
        fs::write(&recorded, b"video").unwrap();
        prepare(&root).unwrap();
        assert_eq!(fs::read(&recorded).unwrap(), b"video");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}
