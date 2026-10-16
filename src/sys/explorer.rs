//! What is selected in the File Explorer window in front.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    IServiceProvider,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    IFolderView, IShellBrowser, IShellItemArray, IShellWindows, SID_STopLevelBrowser, SIGDN_FILESYSPATH, SVGIO_SELECTION,
    ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, GetForegroundWindow};
use windows::core::{Interface, PCWSTR, w};

/// The paths selected in the File Explorer window in front (its open tab); empty when the window in front
/// is not File Explorer or nothing is selected. Items that are not files on disk (Control Panel, say) are
/// left out.
pub fn selected() -> Result<Vec<PathBuf>> {
    // Shell objects need a single-threaded COM apartment; this runs on its own thread so it does not depend
    // on the caller's COM setup.
    std::thread::spawn(|| unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let r = find();
        if com {
            CoUninitialize();
        }
        r
    })
    .join()
    .map_err(|_| anyhow!("could not read File Explorer's selection"))?
}

unsafe fn find() -> Result<Vec<PathBuf>> {
    unsafe {
        let front = GetForegroundWindow();
        // Each Explorer tab has a window of this class; the first one is the tab in view
        let tab = FindWindowExW(Some(front), None, w!("ShellTabWindowClass"), PCWSTR::null()).unwrap_or_default();
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)?;
        for i in 0..windows.Count()? {
            let Ok(window) = windows.Item(&VARIANT::from(i)) else { continue };
            let Ok(services) = window.cast::<IServiceProvider>() else { continue };
            let Ok(browser) = services.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) else { continue };
            let Ok(hwnd) = browser.GetWindow() else { continue };
            if hwnd != tab && hwnd != front {
                continue;
            }
            let view: IFolderView = browser.QueryActiveShellView()?.cast()?;
            let Ok(items) = view.Items::<IShellItemArray>(SVGIO_SELECTION) else { return Ok(Vec::new()) };
            let mut out = Vec::new();
            for k in 0..items.GetCount()? {
                let Ok(path) = items.GetItemAt(k)?.GetDisplayName(SIGDN_FILESYSPATH) else { continue };
                let s = path.to_string();
                CoTaskMemFree(Some(path.0 as _));
                if let Ok(s) = s {
                    out.push(PathBuf::from(s));
                }
            }
            return Ok(out);
        }
        Ok(Vec::new())
    }
}
