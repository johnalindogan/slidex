//! Move user media to the Recycle Bin. This module never deletes a file outright.
//!
//! A path is eligible only when `GetDriveTypeW == DRIVE_FIXED` and the DOS device
//! is not a substitute for a network share. The Windows recycle calls
//! `IFileOperation` with `FOF_ALLOWUNDO` and aborts in `PreDeleteItem` unless
//! `TSF_DELETE_RECYCLE_IF_POSSIBLE` is set, so a failed recycle is not a
//! permanent delete. The `trash` crate is not used: its nuke-warning flag
//! can still remove the file when the shell cannot recycle it.

/// Win32 `GetDriveTypeW` results. Kept here so tests do not need the Windows SDK.
pub const DRIVE_UNKNOWN: u32 = 0;
#[allow(dead_code)]
pub const DRIVE_NO_ROOT_DIR: u32 = 1;
pub const DRIVE_REMOVABLE: u32 = 2;
pub const DRIVE_FIXED: u32 = 3;
pub const DRIVE_REMOTE: u32 = 4;
#[allow(dead_code)]
pub const DRIVE_CDROM: u32 = 5;
#[allow(dead_code)]
pub const DRIVE_RAMDISK: u32 = 6;

/// What the shell would see for one path. Tests pass this instead of calling Windows.
#[derive(Clone, Debug)]
pub struct DriveFacts {
    pub drive_type: u32,
    pub device_target: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveClass {
    pub path: String,
    pub drive_type: u32,
    pub device_target: String,
    pub fixed: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashReport {
    pub moved: Vec<String>,
    pub failed: Vec<String>,
    pub message: String,
}

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

/// `subst` and mapped devices that point at a share even if the drive type is wrong.
pub fn device_target_is_network(target: &str) -> bool {
    let lower = target.trim().to_ascii_lowercase();
    lower.contains(r"unc\")
        || lower.contains(r"\device\mup")
        || lower.contains("lanmanredirector")
        || lower.starts_with(r"\\")
}

fn file_label(path: &str) -> String {
    // Split on both separators so a Linux test names the file the way Windows will.
    let normalized = path.trim().replace('/', "\\");
    normalized
        .rsplit('\\')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("this file")
        .to_string()
}

fn drive_refuse_message(path: &str, facts: &DriveFacts) -> String {
    let name = file_label(path);
    if device_target_is_network(&facts.device_target) {
        return format!(
            "Couldn't move {name} to the Recycle Bin. The drive is a substitute for a network share, which has no Recycle Bin. Nothing was deleted."
        );
    }
    match facts.drive_type {
        DRIVE_REMOTE => format!(
            "Couldn't move {name} to the Recycle Bin. It's on a network drive, which has no Recycle Bin. Nothing was deleted."
        ),
        DRIVE_REMOVABLE => format!(
            "Couldn't move {name} to the Recycle Bin. It's on a removable drive, which has no Recycle Bin. Nothing was deleted."
        ),
        _ => format!(
            "Couldn't move {name} to the Recycle Bin. It's not on a fixed drive on this PC. Nothing was deleted."
        ),
    }
}

/// `None` means the path may be recycled. Anything else is left in place.
pub fn fixed_drive_block(path: &str, facts: &DriveFacts) -> Option<String> {
    if recycle_bin_unavailable(path) {
        return Some(share_refuse_message(path));
    }
    if facts.drive_type == DRIVE_FIXED && !device_target_is_network(&facts.device_target) {
        return None;
    }
    Some(drive_refuse_message(path, facts))
}

fn strip_nothing_deleted(message: &str) -> String {
    message
        .replace(" Nothing was deleted.", "")
        .replace("Nothing was deleted.", "")
        .trim()
        .to_string()
}

/// Names what moved and what was left. "Nothing was deleted" is only used when
/// every file stayed in place.
pub fn describe_trash_batch(moved: &[String], failed: &[String]) -> String {
    if moved.is_empty() {
        let body = failed
            .iter()
            .map(|m| m.trim())
            .filter(|m| !m.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        return if body.is_empty() {
            plain_refuse("Choose a file first.")
        } else {
            plain_refuse(&body)
        };
    }
    let names = moved
        .iter()
        .map(|p| file_label(p))
        .collect::<Vec<_>>()
        .join(", ");
    let moved_line = format!("Moved {names} to the Recycle Bin.");
    let fails = failed
        .iter()
        .map(|m| strip_nothing_deleted(m))
        .filter(|m| !m.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if fails.is_empty() {
        moved_line
    } else {
        format!("{moved_line} {fails}")
    }
}

pub fn classify_drives_with(
    paths: &[String],
    mut facts_for: impl FnMut(&str) -> DriveFacts,
) -> Vec<DriveClass> {
    paths
        .iter()
        .map(|path| {
            let facts = facts_for(path);
            let fixed = fixed_drive_block(path, &facts).is_none();
            DriveClass {
                path: path.clone(),
                drive_type: facts.drive_type,
                device_target: facts.device_target,
                fixed,
            }
        })
        .collect()
}

/// Recycle every fixed-drive path. A refusal or a failed recycle does not
/// delete that file and does not undo files already recycled.
pub fn trash_user_media_with(
    paths: &[String],
    mut facts_for: impl FnMut(&str) -> DriveFacts,
    mut delete: impl FnMut(&str) -> Result<(), String>,
) -> TrashReport {
    let mut moved = Vec::new();
    let mut failed = Vec::new();
    if paths.is_empty() {
        failed.push(plain_refuse("Choose a file first."));
    }
    for path in paths {
        let facts = facts_for(path);
        if let Some(reason) = fixed_drive_block(path, &facts) {
            failed.push(reason);
            continue;
        }
        match delete(path) {
            Ok(()) => moved.push(path.clone()),
            Err(err) => {
                let name = file_label(path);
                let detail = if err.contains("Nothing was deleted") || err.contains("left it in place")
                {
                    err
                } else {
                    format!(
                        "Couldn't move {name} to the Recycle Bin ({err}). It was left in place."
                    )
                };
                failed.push(detail);
            }
        }
    }
    let message = describe_trash_batch(&moved, &failed);
    TrashReport {
        moved,
        failed,
        message,
    }
}

pub fn classify_media_drives(paths: &[String]) -> Vec<DriveClass> {
    classify_drives_with(paths, probe_drive)
}

pub fn trash_user_media(paths: &[String]) -> TrashReport {
    trash_user_media_with(paths, probe_drive, recycle_one)
}

#[cfg(windows)]
fn probe_drive(path: &str) -> DriveFacts {
    windows_bin::probe(path)
}

#[cfg(not(windows))]
fn probe_drive(_path: &str) -> DriveFacts {
    DriveFacts {
        drive_type: DRIVE_UNKNOWN,
        device_target: String::new(),
    }
}

#[cfg(windows)]
fn recycle_one(path: &str) -> Result<(), String> {
    windows_bin::recycle_or_abort(path)
}

#[cfg(not(windows))]
fn recycle_one(_path: &str) -> Result<(), String> {
    Err("The Recycle Bin is only available in the Windows app. It was left in place.".into())
}

#[cfg(windows)]
mod windows_bin {
    use super::DriveFacts;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::{
        core::{implement, HRESULT, PCWSTR},
        Win32::{
            Foundation::E_ABORT,
            Storage::FileSystem::{
                GetDriveTypeW, GetVolumePathNameW, QueryDosDeviceW,
            },
            System::Com::{
                CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
            },
            UI::Shell::{
                FileOperation, IFileOperation, IFileOperationProgressSink,
                IFileOperationProgressSink_Impl, IShellItem, SHCreateItemFromParsingName,
                FOF_ALLOWUNDO, FOF_NO_UI, TSF_DELETE_RECYCLE_IF_POSSIBLE,
            },
        },
    };

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn probe(path: &str) -> DriveFacts {
        let normalized = path.trim().replace('/', "\\");
        let name = wide(&normalized);
        let mut volume = vec![0u16; 512];
        let root_ok = unsafe { GetVolumePathNameW(PCWSTR(name.as_ptr()), &mut volume) }.is_ok();
        let root = if root_ok {
            PCWSTR(volume.as_ptr())
        } else {
            PCWSTR(name.as_ptr())
        };
        let drive_type = unsafe { GetDriveTypeW(root) };
        let mut device = vec![0u16; 1024];
        let letter = volume_drive_letter(&volume).or_else(|| path_drive_letter(&normalized));
        let device_target = if let Some(letter) = letter {
            let dos = wide(&format!("{letter}:"));
            let n = unsafe { QueryDosDeviceW(PCWSTR(dos.as_ptr()), Some(device.as_mut_slice())) };
            if n > 1 {
                String::from_utf16_lossy(&device[..(n as usize).saturating_sub(1)])
                    .trim_end_matches('\0')
                    .to_string()
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        DriveFacts {
            drive_type,
            device_target,
        }
    }

    fn volume_drive_letter(volume: &[u16]) -> Option<char> {
        let text = String::from_utf16_lossy(volume);
        path_drive_letter(text.trim_end_matches('\0'))
    }

    fn path_drive_letter(path: &str) -> Option<char> {
        let trimmed = path.trim_start_matches(r"\\?\");
        let mut chars = trimmed.chars();
        let letter = chars.next()?;
        if letter.is_ascii_alphabetic() && chars.next() == Some(':') {
            Some(letter.to_ascii_uppercase())
        } else {
            None
        }
    }

    #[implement(IFileOperationProgressSink)]
    struct RecycleOnlySink;

    impl IFileOperationProgressSink_Impl for RecycleOnlySink_Impl {
        fn StartOperations(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn FinishOperations(&self, _hrresult: HRESULT) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreRenameItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<IShellItem>,
            _psznewname: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostRenameItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<IShellItem>,
            _psznewname: &PCWSTR,
            _hrrename: HRESULT,
            _psinewlycreated: windows::core::Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreMoveItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<IShellItem>,
            _psidestinationfolder: windows::core::Ref<IShellItem>,
            _psznewname: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostMoveItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<IShellItem>,
            _psidestinationfolder: windows::core::Ref<IShellItem>,
            _psznewname: &PCWSTR,
            _hrmove: HRESULT,
            _psinewlycreated: windows::core::Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreCopyItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<IShellItem>,
            _psidestinationfolder: windows::core::Ref<IShellItem>,
            _psznewname: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostCopyItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<IShellItem>,
            _psidestinationfolder: windows::core::Ref<IShellItem>,
            _psznewname: &PCWSTR,
            _hrcopy: HRESULT,
            _psinewlycreated: windows::core::Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreDeleteItem(
            &self,
            dwflags: u32,
            _psiitem: windows::core::Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            if dwflags & (TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32) != 0 {
                Ok(())
            } else {
                Err(E_ABORT.into())
            }
        }
        fn PostDeleteItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<IShellItem>,
            hrdelete: HRESULT,
            _psinewlycreated: windows::core::Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            if hrdelete.is_ok() {
                Ok(())
            } else {
                Err(E_ABORT.into())
            }
        }
        fn PreNewItem(
            &self,
            _dwflags: u32,
            _psidestinationfolder: windows::core::Ref<IShellItem>,
            _psznewname: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostNewItem(
            &self,
            _dwflags: u32,
            _psidestinationfolder: windows::core::Ref<IShellItem>,
            _psznewname: &PCWSTR,
            _psztargetname: &PCWSTR,
            _dwfileattributes: u32,
            _hrnew: HRESULT,
            _psinewitem: windows::core::Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn UpdateProgress(&self, _iworktotal: u32, _iworksofar: u32) -> windows::core::Result<()> {
            Ok(())
        }
        fn ResetTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn PauseTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn ResumeTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
    }

    /// Recycle `path` or return an error. Never calls a permanent delete.
    pub fn recycle_or_abort(path: &str) -> Result<(), String> {
        let name = Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("this file");
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_err() && hr != windows::Win32::Foundation::RPC_E_CHANGED_MODE {
            return Err(format!(
                "Couldn't move {name} to the Recycle Bin ({hr}). It was left in place."
            ));
        }
        let parsing = wide(&path.replace('/', "\\"));
        let outcome = unsafe {
            let pfo: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)
                .map_err(|err| err.to_string())?;
            pfo.SetOperationFlags(FOF_ALLOWUNDO | FOF_NO_UI)
                .map_err(|err| err.to_string())?;
            let item: IShellItem =
                SHCreateItemFromParsingName(PCWSTR(parsing.as_ptr()), None).map_err(|err| err.to_string())?;
            let sink: IFileOperationProgressSink = RecycleOnlySink.into();
            pfo.DeleteItem(&item, &sink).map_err(|err| err.to_string())?;
            pfo.PerformOperations().map_err(|err| err.to_string())?;
            pfo.GetAnyOperationsAborted()
                .map_err(|err| err.to_string())?
        };
        if outcome.as_bool() {
            return Err(format!(
                "Couldn't move {name} to the Recycle Bin. Windows would not recycle it, so SlideX left it in place."
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fixed(path: &str) -> DriveFacts {
        let _ = path;
        DriveFacts {
            drive_type: DRIVE_FIXED,
            device_target: r"\??\C:\".into(),
        }
    }

    fn typed(kind: u32) -> impl FnMut(&str) -> DriveFacts {
        move |_path| DriveFacts {
            drive_type: kind,
            device_target: String::new(),
        }
    }

    #[test]
    fn network_share_is_refused_and_the_deleter_is_not_called() {
        let calls = AtomicUsize::new(0);
        let report = trash_user_media_with(
            &[r"\\nas\media\clip.mp4".to_string()],
            fixed,
            |path| {
                calls.fetch_add(1, Ordering::SeqCst);
                panic!("deleter must not run for a share, got {path}");
            },
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(report.moved.is_empty());
        assert!(report.message.contains("clip.mp4"), "{}", report.message);
        assert!(report.message.contains("network share"), "{}", report.message);
        assert!(report.message.contains("Nothing was deleted"), "{}", report.message);
        assert!(!report.message.to_ascii_lowercase().contains("permanent"));

        let extended = trash_user_media_with(
            &[r"\\?\UNC\nas\media\still.jpg".to_string()],
            fixed,
            |_| panic!("extended UNC must not be deleted"),
        );
        assert!(extended.message.contains("still.jpg"), "{}", extended.message);

        let slashes = trash_user_media_with(
            &[r"//nas/media/other.png".to_string()],
            fixed,
            |_| panic!("slash UNC must not be deleted"),
        );
        assert!(slashes.message.contains("other.png"), "{}", slashes.message);
    }

    #[test]
    fn mapped_removable_and_subst_drives_are_refused() {
        let calls = AtomicUsize::new(0);
        let mapped = trash_user_media_with(
            &[r"Z:\Media\clip.mp4".to_string()],
            typed(DRIVE_REMOTE),
            |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                panic!("mapped drive must not be deleted");
            },
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(mapped.message.contains("network drive"), "{}", mapped.message);
        assert!(mapped.message.contains("Nothing was deleted"), "{}", mapped.message);
        assert!(mapped.moved.is_empty());

        let usb = trash_user_media_with(
            &[r"E:\DCIM\photo.jpg".to_string()],
            typed(DRIVE_REMOVABLE),
            |_| panic!("removable drive must not be deleted"),
        );
        assert!(usb.message.contains("removable drive"), "{}", usb.message);
        assert!(usb.message.contains("Nothing was deleted"), "{}", usb.message);

        let subst = trash_user_media_with(
            &[r"Z:\Media\clip.mp4".to_string()],
            |_path| DriveFacts {
                drive_type: DRIVE_FIXED,
                device_target: r"\??\UNC\nas\media".into(),
            },
            |_| panic!("subst to a share must not be deleted"),
        );
        assert!(subst.message.contains("network share"), "{}", subst.message);
        assert!(subst.moved.is_empty());
    }

    #[test]
    fn a_local_file_is_handed_to_the_recycle_bin_once() {
        let calls = AtomicUsize::new(0);
        let report = trash_user_media_with(
            &[
                r"C:\Photos\lake.jpg".to_string(),
                r"\\?\D:\Videos\clip.mp4".to_string(),
            ],
            fixed,
            |path| {
                calls.fetch_add(1, Ordering::SeqCst);
                assert!(fixed_drive_block(path, &fixed(path)).is_none(), "{path}");
                Ok(())
            },
        );
        assert!(report.failed.is_empty(), "{}", report.message);
        assert_eq!(report.moved.len(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(!report.message.contains("Nothing was deleted"), "{}", report.message);
    }

    #[test]
    fn a_partial_batch_names_what_moved_and_what_stayed() {
        let report = trash_user_media_with(
            &[
                r"C:\Photos\lake.jpg".to_string(),
                r"Z:\Media\clip.mp4".to_string(),
            ],
            |path| {
                if path.starts_with("Z:") {
                    DriveFacts {
                        drive_type: DRIVE_REMOTE,
                        device_target: String::new(),
                    }
                } else {
                    fixed(path)
                }
            },
            |_path| Ok(()),
        );
        assert_eq!(report.moved, vec![r"C:\Photos\lake.jpg".to_string()]);
        assert_eq!(report.failed.len(), 1);
        assert!(report.message.contains("Moved lake.jpg"), "{}", report.message);
        assert!(report.message.contains("clip.mp4"), "{}", report.message);
        assert!(
            !report.message.contains("Nothing was deleted"),
            "{}",
            report.message
        );
    }

    #[test]
    fn this_module_never_permanently_deletes() {
        let src = include_str!("media_trash.rs");
        let impl_src = src.split("#[cfg(test)]").next().expect("impl");
        assert!(impl_src.contains("FOF_ALLOWUNDO"));
        assert!(impl_src.contains("TSF_DELETE_RECYCLE_IF_POSSIBLE"));
        assert!(impl_src.contains("E_ABORT"));
        assert!(!impl_src.contains("trash::delete"));
        assert!(!impl_src.contains("remove_file"));
        assert!(!impl_src.contains("remove_dir"));
        assert!(!impl_src.contains("std::fs::remove"));
        assert!(!impl_src.contains("FOF_WANTNUKEWARNING"));
    }
}
