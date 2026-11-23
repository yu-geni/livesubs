//! --install-shortcut: a shortcut to this exe on the Desktop (run by the build script).
use windows::{
    core::*,
    Win32::{
        System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED},
        UI::Shell::{FOLDERID_Desktop, IShellLinkW, SHChangeNotify, SHGetKnownFolderPath, ShellLink, KF_FLAG_DEFAULT, SHCNE_UPDATEITEM, SHCNF_FLUSH, SHCNF_PATHW},
    },
};

use crate::core;

pub unsafe fn shortcut() -> Result<()> {
    CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    let exe = std::env::current_exe().map_err(|_| Error::from_win32())?;
    let dir = core::exe_dir();
    let desktop = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None)?; // may be redirected into OneDrive
    let desk = desktop.to_string().unwrap_or_default();
    let lnk = format!("{desk}\\ライブ字幕.lnk");
    let _ = std::fs::remove_file(format!("{desk}\\ライブ字幕 (Rust).lnk")); // from the time the app was installed beside its predecessor
    CoTaskMemFree(Some(desktop.0 as *const _));
    // Windows remembers a shortcut's picture by the path it was taken from; the build script
    // puts the icon beside the exe under a name that changes with its content.
    let icon = std::fs::read_dir(&dir).ok().and_then(|d| d.flatten().map(|e| e.path()).find(|p| {
        let n = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        n.starts_with("LiveSubs-") && n.ends_with(".ico")
    }));
    let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
    link.SetPath(&HSTRING::from(exe.as_os_str()))?;
    link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
    link.SetDescription(w!("ライブ字幕"))?;
    if let Some(icon) = &icon { link.SetIconLocation(&HSTRING::from(icon.as_os_str()), 0)?; }
    let wide = HSTRING::from(lnk.as_str());
    link.cast::<IPersistFile>()?.Save(&wide, true)?;
    SHChangeNotify(SHCNE_UPDATEITEM, SHCNF_PATHW | SHCNF_FLUSH, Some(wide.as_ptr() as *const _), None);
    core::log_init();
    core::log(&format!("shortcut written: {lnk} -> {}", exe.display()));
    Ok(())
}
