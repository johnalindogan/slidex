//! Move user media to the Recycle Bin. This module never deletes a file outright.

use std::path::Path;

/// Shown when a path cannot go to the Recycle Bin. The file is left in place.
pub fn share_refuse_message(path: &str) -> String {
    let name = file_label(path);
    format!(
        "Couldn't move {name} to the Recycle Bin. It's on a network share, which has no Recycle Bin. Nothing was deleted."
    )
}

fn plain_refuse(message: &str) -> String {
    if message.contains("Nothing was deleted") {
        message.to_string()
    } else {
        format!("{message} Nothing was deleted.")
    }
}

/// UNC (`\\server\share\...`) and `\\?\UNC\...` have no Recycle Bin.
/// A drive-letter path, including `\\?\C:\...`, can.
pub fn recycle_bin_unavailable(path: &str) -> bool {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return true;
    }
    let slash = trimmed.replace('/', "\\");
    let lower = slash.to_ascii_lowercase();
    if lower.starts_with(r"\\?\unc\") {
        return true;
    }
    if let Some(rest) = lower.strip_prefix(r"\\?\") {
        let mut chars = rest.chars();
        if let (Some(drive), Some(':')) = (chars.next(), chars.next()) {
            return !drive.is_ascii_alphabetic();
        }
        return true;
    }
    slash.starts_with(r"\\")
}

fn file_label(path: &str) -> String {
    let normalized = path.trim().replace('/', "\\");
    Path::new(&normalized)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("this file")
        .to_string()
}

fn recycle_failed_message(path: &str, err: &str) -> String {
    let name = file_label(path);
    plain_refuse(&format!(
        "Couldn't move {name} to the Recycle Bin ({err})."
    ))
}

/// Send every path to the Recycle Bin, or refuse the whole batch.
/// `delete` is `trash::delete` in the app. Tests pass a fake so nothing is removed.
pub fn trash_user_media_with(
    paths: &[String],
    mut delete: impl FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    if paths.is_empty() {
        return Err(plain_refuse("Choose a file first."));
    }
    for path in paths {
        if recycle_bin_unavailable(path) {
            return Err(share_refuse_message(path));
        }
    }
    for path in paths {
        delete(path).map_err(|err| {
            if err.contains("Nothing was deleted") {
                err
            } else {
                recycle_failed_message(path, &err)
            }
        })?;
    }
    Ok(())
}

/// Move user media to the Windows Recycle Bin via the `trash` crate.
pub fn trash_user_media(paths: &[String]) -> Result<(), String> {
    trash_user_media_with(paths, |path| {
        trash::delete(path).map_err(|err| err.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn network_share_is_refused_and_the_deleter_is_not_called() {
        let calls = AtomicUsize::new(0);
        let err = trash_user_media_with(
            &[
                r"C:\Photos\keep.jpg".to_string(),
                r"\\nas\media\clip.mp4".to_string(),
            ],
            |path| {
                calls.fetch_add(1, Ordering::SeqCst);
                panic!("deleter must not run for a share, got {path}");
            },
        )
        .expect_err("a UNC path has no Recycle Bin");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(err.contains("clip.mp4"), "{err}");
        assert!(err.contains("network share"), "{err}");
        assert!(err.contains("Nothing was deleted"), "{err}");
        assert!(!err.to_ascii_lowercase().contains("permanent"));

        let extended = trash_user_media_with(
            &[r"\\?\UNC\nas\media\still.jpg".to_string()],
            |_| panic!("extended UNC must not be deleted"),
        )
        .expect_err("extended UNC");
        assert!(extended.contains("still.jpg"), "{extended}");

        let slashes = trash_user_media_with(
            &[r"//nas/media/other.png".to_string()],
            |_| panic!("slash UNC must not be deleted"),
        )
        .expect_err("forward-slash UNC");
        assert!(slashes.contains("other.png"), "{slashes}");
    }

    #[test]
    fn a_local_file_is_handed_to_the_recycle_bin_once() {
        let calls = AtomicUsize::new(0);
        trash_user_media_with(
            &[
                r"C:\Photos\lake.jpg".to_string(),
                r"\\?\D:\Videos\clip.mp4".to_string(),
            ],
            |path| {
                calls.fetch_add(1, Ordering::SeqCst);
                assert!(!recycle_bin_unavailable(path), "{path}");
                Ok(())
            },
        )
        .expect("local paths can use the Recycle Bin");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(!recycle_bin_unavailable(r"C:\Photos\lake.jpg"));
        assert!(!recycle_bin_unavailable(r"\\?\D:\Videos\clip.mp4"));
        assert!(recycle_bin_unavailable(""));
        assert!(recycle_bin_unavailable(r"\\server\share"));
    }

    #[test]
    fn this_module_never_permanently_deletes() {
        let src = include_str!("media_trash.rs");
        let impl_src = src.split("#[cfg(test)]").next().expect("impl");
        assert!(impl_src.contains("trash::delete"));
        assert!(!impl_src.contains("remove_file"));
        assert!(!impl_src.contains("remove_dir"));
        assert!(!impl_src.contains("std::fs::remove"));
    }
}
