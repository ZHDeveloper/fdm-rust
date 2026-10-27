use std::path::{Path, PathBuf};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};

pub trait PlatformService: Send + Sync {
    fn default_download_dir(&self) -> PathBuf;
    fn open_path(&self, path: &Path) -> bool;
    fn reveal_path(&self, path: &Path) -> bool;
    fn read_clipboard(&self) -> String;
    fn choose_dir(&self, title: &str, initial: &Path) -> Option<PathBuf>;
    fn choose_save_as(&self, title: &str, initial: &Path, default_name: &str) -> Option<PathBuf>;
    fn choose_file(&self, title: &str, filter_name: &str, extensions: &[&str]) -> Option<PathBuf>;
    fn message_box(&self, title: &str, message: &str, kind: &str, confirm: bool) -> bool;
    /// 打开 Chrome / Edge 的扩展管理页（无法启动时返回 false）
    fn open_browser_extensions(&self, browser: &str) -> bool;
    fn sync_auto_launch(&self, enabled: bool) -> bool;
    /// 注册/取消 magnet、ed2k 与 .torrent 的文件关联（Windows 当前用户）
    fn sync_file_associations(&self, enabled: bool) -> bool;
    fn extension_dir(&self) -> PathBuf;
}

pub struct SystemPlatform {
    extension_dir: PathBuf,
    allow_auto_launch: bool,
}

impl SystemPlatform {
    pub fn new(extension_dir: PathBuf, allow_auto_launch: bool) -> Self {
        Self {
            extension_dir,
            allow_auto_launch,
        }
    }
}

impl PlatformService for SystemPlatform {
    fn default_download_dir(&self) -> PathBuf {
        fdm_core::settings::system_download_dir()
    }

    fn open_path(&self, path: &Path) -> bool {
        if path.exists() {
            return open::that(path).is_ok();
        }
        if let Some(parent) = path.parent().filter(|p| p.exists()) {
            return open::that(parent).is_ok();
        }
        false
    }

    fn reveal_path(&self, path: &Path) -> bool {
        #[cfg(windows)]
        {
            if path.exists() {
                use std::process::Command;
                return Command::new("explorer")
                    .arg(format!("/select,{}", path.to_string_lossy()))
                    .spawn()
                    .is_ok();
            }
        }
        if let Some(parent) = path.parent().filter(|p| p.exists()) {
            return open::that(parent).is_ok();
        }
        self.open_path(path)
    }

    fn read_clipboard(&self) -> String {
        arboard::Clipboard::new()
            .ok()
            .and_then(|mut c| c.get_text().ok())
            .unwrap_or_default()
    }

    fn choose_dir(&self, title: &str, initial: &Path) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_title(title)
            .set_directory(initial)
            .pick_folder()
    }

    fn choose_save_as(&self, title: &str, initial: &Path, default_name: &str) -> Option<PathBuf> {
        let mut dialog = rfd::FileDialog::new()
            .set_title(title)
            .set_directory(initial);
        if !default_name.is_empty() {
            dialog = dialog.set_file_name(default_name);
        }
        dialog.save_file()
    }

    fn choose_file(&self, title: &str, filter_name: &str, extensions: &[&str]) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_title(title)
            .add_filter(filter_name, extensions)
            .pick_file()
    }

    fn message_box(&self, title: &str, message: &str, kind: &str, confirm: bool) -> bool {
        #[cfg(windows)]
        {
            return show_message_box(title, message, kind, confirm);
        }
        #[cfg(not(windows))]
        {
            let level = match kind {
                "error" => rfd::MessageLevel::Error,
                "warning" => rfd::MessageLevel::Warning,
                _ => rfd::MessageLevel::Info,
            };
            let buttons = if confirm {
                rfd::MessageButtons::OkCancel
            } else {
                rfd::MessageButtons::Ok
            };
            let result = rfd::MessageDialog::new()
                .set_title(title)
                .set_description(message)
                .set_level(level)
                .set_buttons(buttons)
                .show();
            matches!(
                result,
                rfd::MessageDialogResult::Ok | rfd::MessageDialogResult::Yes
            )
        }
    }

    fn open_browser_extensions(&self, browser: &str) -> bool {
        open_browser_extensions_impl(browser)
    }

    fn sync_auto_launch(&self, enabled: bool) -> bool {
        if !self.allow_auto_launch || std::env::var_os("FDM_DISABLE_AUTO_LAUNCH").is_some() {
            return true;
        }
        sync_auto_launch_impl(enabled)
    }

    fn sync_file_associations(&self, enabled: bool) -> bool {
        if std::env::var_os("FDM_DISABLE_FILE_ASSOC").is_some() {
            return true;
        }
        sync_file_associations_impl(enabled)
    }

    fn extension_dir(&self) -> PathBuf {
        self.extension_dir.clone()
    }
}

#[cfg(windows)]
fn show_message_box(title: &str, message: &str, kind: &str, confirm: bool) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible, MessageBoxW, SetForegroundWindow, SetWindowPos, HWND_TOPMOST, IDOK, IDYES,
        MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_OKCANCEL, MB_SETFOREGROUND,
        MB_TOPMOST, SWP_NOMOVE, SWP_NOSIZE,
    };

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    struct FindCtx {
        pid: u32,
        title: Vec<u16>,
        existing: Vec<isize>,
        found: HWND,
    }

    unsafe extern "system" fn find_dialog(hwnd: HWND, lparam: LPARAM) -> i32 {
        let ctx = &mut *(lparam as *mut FindCtx);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != ctx.pid || ctx.existing.contains(&(hwnd as isize)) || IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let mut class = [0u16; 32];
        let class_len = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
        if class_len <= 0 || String::from_utf16_lossy(&class[..class_len as usize]) != "#32770" {
            return 1;
        }
        let mut text = [0u16; 256];
        let text_len = GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32);
        if text_len <= 0 || &text[..text_len as usize] != ctx.title.as_slice() {
            return 1;
        }
        ctx.found = hwnd;
        0
    }

    struct CollectCtx {
        pid: u32,
        handles: Vec<isize>,
    }

    unsafe extern "system" fn collect_existing(hwnd: HWND, lparam: LPARAM) -> i32 {
        let ctx = &mut *(lparam as *mut CollectCtx);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == ctx.pid && IsWindowVisible(hwnd) != 0 {
            let mut class = [0u16; 32];
            let class_len = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
            if class_len > 0 && String::from_utf16_lossy(&class[..class_len as usize]) == "#32770" {
                ctx.handles.push(hwnd as isize);
            }
        }
        1
    }

    let pid = std::process::id();
    let mut expected = wide(title);
    expected.pop(); // 去掉结尾 NUL
    let mut collect = CollectCtx {
        pid,
        handles: Vec::new(),
    };
    unsafe {
        EnumWindows(
            Some(collect_existing),
            &mut collect as *mut CollectCtx as LPARAM,
        );
    }

    // 从 Tokio 工作线程调用 MessageBoxW 时 Windows 会丢掉 WS_EX_TOPMOST，
    // 因此另起守护线程在对话框出现后强制置顶并拉到前台。
    let done = Arc::new(AtomicBool::new(false));
    let watch_done = done.clone();
    let watcher = std::thread::spawn(move || {
        let mut ctx = FindCtx {
            pid,
            title: expected,
            existing: collect.handles,
            found: std::ptr::null_mut(),
        };
        for _ in 0..300 {
            if watch_done.load(Ordering::SeqCst) {
                return;
            }
            ctx.found = std::ptr::null_mut();
            unsafe {
                EnumWindows(Some(find_dialog), &mut ctx as *mut FindCtx as LPARAM);
            }
            if !ctx.found.is_null() {
                unsafe {
                    SetWindowPos(ctx.found, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
                    SetForegroundWindow(ctx.found);
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });

    let owner: HWND = unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_null() {
            std::ptr::null_mut()
        } else {
            let mut pid = 0u32;
            GetWindowThreadProcessId(foreground, &mut pid);
            if pid == std::process::id() {
                foreground
            } else {
                std::ptr::null_mut()
            }
        }
    };
    let icon = match kind {
        "error" => MB_ICONERROR,
        "warning" => MB_ICONWARNING,
        _ => MB_ICONINFORMATION,
    };
    let buttons = if confirm { MB_OKCANCEL } else { MB_OK };
    let text = wide(message);
    let caption = wide(title);
    let result = unsafe {
        MessageBoxW(
            owner,
            text.as_ptr(),
            caption.as_ptr(),
            buttons | icon | MB_TOPMOST | MB_SETFOREGROUND,
        )
    };
    done.store(true, Ordering::SeqCst);
    let _ = watcher.join();
    matches!(result, IDOK | IDYES)
}
#[cfg(windows)]
fn join_key(base: &str, sub: &str) -> String {
    if base.is_empty() {
        sub.to_string()
    } else {
        format!("{base}\\{sub}")
    }
}

#[cfg(windows)]
fn sync_file_associations_impl(enabled: bool) -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = exe.canonicalize().unwrap_or(exe);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    sync_file_associations_at(&hkcu, "", &exe, enabled)
}

/// 在指定注册表根下注册 / 注销文件关联。`base` 为空时使用 HKCU 根，
/// 单元测试会传入临时前缀，避免影响真实关联。
#[cfg(windows)]
fn sync_file_associations_at(hkcu: &winreg::RegKey, base: &str, exe: &Path, enabled: bool) -> bool {
    use winreg::enums::KEY_ALL_ACCESS;

    let exe_text = exe.to_string_lossy().into_owned();
    let command = format!("\"{exe_text}\" \"%1\"");
    let icon = format!("{exe_text},0");
    let app_file = exe
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("fdm-desktop.exe")
        .to_string();

    if enabled {
        let mut ok = true;

        for (prog, label, proto) in [
            ("FDM.magnet", "FDM 磁力链接", "magnet"),
            ("FDM.ed2k", "FDM ED2K 链接", "ed2k"),
        ] {
            let prog_path = join_key(base, &format!(r"Software\Classes\{prog}"));
            match hkcu.create_subkey(&prog_path) {
                Ok((key, _)) => {
                    ok &= key.set_value("", &label).is_ok();
                    ok &= key.set_value("URL Protocol", &"").is_ok();
                    ok &= key.set_value("FriendlyTypeName", &label).is_ok();
                }
                Err(_) => ok = false,
            }
            if let Ok((key, _)) = hkcu.create_subkey(&join_key(&prog_path, "DefaultIcon")) {
                ok &= key.set_value("", &icon).is_ok();
            }
            if let Ok((key, _)) = hkcu.create_subkey(&join_key(&prog_path, r"shell\open\command")) {
                ok &= key.set_value("", &command).is_ok();
            }

            // 协议类指向 FDM 的 ProgID。若系统已有 UserChoice，Windows 仍以用户选择为准。
            let proto_path = join_key(base, &format!(r"Software\Classes\{proto}"));
            match hkcu.create_subkey(&proto_path) {
                Ok((key, _)) => {
                    ok &= key.set_value("", &prog).is_ok();
                    ok &= key.set_value("URL Protocol", &"").is_ok();
                }
                Err(_) => ok = false,
            }
        }

        let torrent_prog = join_key(base, r"Software\Classes\FDM.torrent");
        match hkcu.create_subkey(&torrent_prog) {
            Ok((key, _)) => {
                ok &= key.set_value("", &"FDM Torrent 文件").is_ok();
                ok &= key
                    .set_value("FriendlyTypeName", &"FDM Torrent 文件")
                    .is_ok();
            }
            Err(_) => ok = false,
        }
        if let Ok((key, _)) = hkcu.create_subkey(&join_key(&torrent_prog, "DefaultIcon")) {
            ok &= key.set_value("", &icon).is_ok();
        }
        if let Ok((key, _)) = hkcu.create_subkey(&join_key(&torrent_prog, r"shell\open\command")) {
            ok &= key.set_value("", &command).is_ok();
        }

        // .torrent 扩展名以及“打开方式”候选
        let ext_path = join_key(base, r"Software\Classes\.torrent");
        match hkcu.create_subkey(&ext_path) {
            Ok((key, _)) => {
                ok &= key.set_value("", &"FDM.torrent").is_ok();
                if let Ok((progids, _)) =
                    hkcu.create_subkey(&join_key(&ext_path, "OpenWithProgids"))
                {
                    ok &= progids.set_value("FDM.torrent", &"").is_ok();
                }
            }
            Err(_) => ok = false,
        }
        if let Ok((key, _)) = hkcu.create_subkey(&join_key(
            base,
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.torrent\OpenWithProgids",
        )) {
            ok &= key.set_value("FDM.torrent", &"").is_ok();
        }

        // 在“打开方式”里显示 FDM
        let app_key = join_key(base, &format!(r"Software\Classes\Applications\{app_file}"));
        match hkcu.create_subkey(&app_key) {
            Ok((key, _)) => {
                ok &= key.set_value("FriendlyAppName", &"FDM").is_ok();
                if let Ok((types, _)) = hkcu.create_subkey(&join_key(&app_key, "SupportedTypes")) {
                    ok &= types.set_value(".torrent", &"").is_ok();
                }
                if let Ok((cmd, _)) = hkcu.create_subkey(&join_key(&app_key, r"shell\open\command"))
                {
                    ok &= cmd.set_value("", &command).is_ok();
                }
            }
            Err(_) => ok = false,
        }

        // 让 FDM 出现在 Windows“默认应用”页面
        let capabilities = join_key(base, r"Software\FDM\Capabilities");
        match hkcu.create_subkey(&capabilities) {
            Ok((key, _)) => {
                ok &= key.set_value("ApplicationName", &"FDM").is_ok();
                ok &= key
                    .set_value("ApplicationDescription", &"FDM 极速多线程下载器")
                    .is_ok();
                ok &= key.set_value("ApplicationIcon", &icon).is_ok();
                if let Ok((file_assoc, _)) =
                    hkcu.create_subkey(&join_key(&capabilities, "FileAssociations"))
                {
                    ok &= file_assoc.set_value(".torrent", &"FDM.torrent").is_ok();
                }
                if let Ok((url_assoc, _)) =
                    hkcu.create_subkey(&join_key(&capabilities, "URLAssociations"))
                {
                    ok &= url_assoc.set_value("magnet", &"FDM.magnet").is_ok();
                    ok &= url_assoc.set_value("ed2k", &"FDM.ed2k").is_ok();
                }
            }
            Err(_) => ok = false,
        }
        if let Ok((key, _)) =
            hkcu.create_subkey(&join_key(base, r"Software\RegisteredApplications"))
        {
            ok &= key.set_value("FDM", &r"Software\FDM\Capabilities").is_ok();
        }

        ok
    } else {
        let mut ok = true;

        // 只有当前值仍指向 FDM 时才清除，避免误删其他程序的设置
        for (proto, prog) in [("magnet", "FDM.magnet"), ("ed2k", "FDM.ed2k")] {
            let path = join_key(base, &format!(r"Software\Classes\{proto}"));
            if let Ok(key) = hkcu.open_subkey_with_flags(&path, KEY_ALL_ACCESS) {
                let current: Option<String> = key.get_value("").ok();
                if current.as_deref() == Some(prog) {
                    let _ = key.delete_value("");
                    let _ = key.delete_value("URL Protocol");
                }
            }
        }

        let ext_path = join_key(base, r"Software\Classes\.torrent");
        if let Ok(key) = hkcu.open_subkey_with_flags(&ext_path, KEY_ALL_ACCESS) {
            let current: Option<String> = key.get_value("").ok();
            if current.as_deref() == Some("FDM.torrent") {
                let _ = key.delete_value("");
            }
            if let Ok(progids) = key.open_subkey_with_flags("OpenWithProgids", KEY_ALL_ACCESS) {
                let _ = progids.delete_value("FDM.torrent");
            }
        }
        if let Ok(key) = hkcu.open_subkey_with_flags(
            &join_key(
                base,
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.torrent\OpenWithProgids",
            ),
            KEY_ALL_ACCESS,
        ) {
            let _ = key.delete_value("FDM.torrent");
        }

        for path in [
            join_key(base, r"Software\Classes\FDM.magnet"),
            join_key(base, r"Software\Classes\FDM.ed2k"),
            join_key(base, r"Software\Classes\FDM.torrent"),
            join_key(base, &format!(r"Software\Classes\Applications\{app_file}")),
            join_key(base, r"Software\FDM\Capabilities"),
        ] {
            match hkcu.delete_subkey_all(&path) {
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => ok = false,
            }
        }
        if let Ok(key) = hkcu.open_subkey_with_flags(
            &join_key(base, r"Software\RegisteredApplications"),
            KEY_ALL_ACCESS,
        ) {
            match key.delete_value("FDM") {
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => ok = false,
            }
        }

        ok
    }
}

#[cfg(target_os = "macos")]
fn sync_file_associations_impl(enabled: bool) -> bool {
    if !enabled {
        // macOS 没有“清除默认处理程序”的公开 API；关闭时停止后续接管，
        // 已设置的默认应用由用户在“系统设置 → 默认应用”中修改。
        return true;
    }

    use std::ffi::CString;
    use std::os::raw::{c_char, c_void};

    type CFStringRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(value: *const c_void);
    }

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn LSSetDefaultHandlerForURLScheme(scheme: CFStringRef, handler: CFStringRef) -> i32;
        fn LSSetDefaultRoleHandlerForContentType(
            content_type: CFStringRef,
            role: u32,
            handler: CFStringRef,
        ) -> i32;
    }

    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const LS_ROLES_VIEWER: u32 = 0x0000_0002;

    unsafe fn make_cfstring(value: &str) -> Option<CFStringRef> {
        let raw = CString::new(value).ok()?;
        let handle =
            CFStringCreateWithCString(std::ptr::null(), raw.as_ptr(), CF_STRING_ENCODING_UTF8);
        if handle.is_null() {
            None
        } else {
            Some(handle)
        }
    }

    let bundle_id = "com.fdm.desktop";
    unsafe {
        let Some(handler) = make_cfstring(bundle_id) else {
            return false;
        };
        let mut ok = true;
        for scheme in ["magnet", "ed2k"] {
            match make_cfstring(scheme) {
                Some(value) => {
                    ok &= LSSetDefaultHandlerForURLScheme(value, handler) == 0;
                    CFRelease(value);
                }
                None => ok = false,
            }
        }
        match make_cfstring("org.bittorrent.torrent") {
            Some(content_type) => {
                ok &= LSSetDefaultRoleHandlerForContentType(content_type, LS_ROLES_VIEWER, handler)
                    == 0;
                CFRelease(content_type);
            }
            None => ok = false,
        }
        CFRelease(handler);
        ok
    }
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn sync_file_associations_impl(_enabled: bool) -> bool {
    false
}

#[cfg(windows)]
fn sync_auto_launch_impl(enabled: bool) -> bool {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    use winreg::RegKey;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let command = format!("\"{}\" --background", exe.display());
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey_with_flags(
        r"Software\Microsoft\Windows\CurrentVersion\Run",
        KEY_SET_VALUE,
    ) else {
        return false;
    };
    if enabled {
        key.set_value("FDM", &command).is_ok()
    } else {
        match key.delete_value("FDM") {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    }
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "macos")]
fn sync_auto_launch_impl(enabled: bool) -> bool {
    use std::process::{Command, Stdio};
    let Ok(home) = std::env::var("HOME") else {
        return false;
    };
    let plist = PathBuf::from(home).join("Library/LaunchAgents/com.fdm.desktop.plist");
    if !enabled {
        let _ = Command::new("launchctl")
            .args(["unload", "-w"])
            .arg(&plist)
            .status();
        let _ = std::fs::remove_file(&plist);
        return true;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    if let Some(parent) = plist.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 不要直接执行 .app 内的二进制：那会绕过 LaunchServices，被系统当成第二个 App，
    // 在程序坞留下重复图标。优先用 open -a <bundle> --args 启动。
    let bundle = exe
        .ancestors()
        .find(|path| path.extension().map(|ext| ext == "app").unwrap_or(false));
    let (program, program_args) = match bundle {
        Some(bundle) => (
            "/usr/bin/open".to_string(),
            vec![
                "-g".to_string(),
                "-a".to_string(),
                bundle.to_string_lossy().into_owned(),
                "--args".to_string(),
                "--background".to_string(),
            ],
        ),
        None => (
            exe.to_string_lossy().into_owned(),
            vec!["--background".to_string()],
        ),
    };
    let mut program_xml = format!("<string>{}</string>", xml_escape(&program));
    for arg in program_args {
        program_xml.push_str(&format!("<string>{}</string>", xml_escape(&arg)));
    }
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.fdm.desktop</string>
<key>ProgramArguments</key><array>{program_xml}</array>
<key>RunAtLoad</key><true/><key>KeepAlive</key><false/>
</dict></plist>"#
    );
    let loaded = Command::new("launchctl")
        .args(["list", "com.fdm.desktop"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    let current = std::fs::read_to_string(&plist).map(|old| old == xml).unwrap_or(false);
    if current && loaded {
        return true;
    }
    if loaded {
        // 已注册但内容有变化（例如升级后参数变化）：只更新文件，下次登录生效，
        // 避免立即 reload 时 RunAtLoad 再次拉起应用并触发 Reopen 显示主窗口。
        return std::fs::write(&plist, xml).is_ok();
    }
    // 未注册时用 RunAtLoad=false 的临时 plist 完成注册，避免在本次启动中再拉起一次应用；
    // 注册后恢复真实 plist，下次登录由 launchd 正常启动。
    let idle = xml.replace(
        "<key>RunAtLoad</key><true/>",
        "<key>RunAtLoad</key><false/>",
    );
    if std::fs::write(&plist, &idle).is_err() {
        return false;
    }
    let registered = Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&plist)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if std::fs::write(&plist, xml).is_err() {
        return false;
    }
    registered
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn sync_auto_launch_impl(enabled: bool) -> bool {
    let Ok(home) = std::env::var("HOME") else {
        return false;
    };
    let path = PathBuf::from(home).join(".config/autostart/fdm.desktop");
    if !enabled {
        return std::fs::remove_file(path).is_ok() || true;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    std::fs::write(path, format!("[Desktop Entry]\nType=Application\nName=FDM\nExec=\"{}\" --background\nX-GNOME-Autostart-enabled=true\n", exe.display())).is_ok()
}

fn browser_target(browser: &str) -> (&'static str, &'static str, &'static str) {
    if browser.eq_ignore_ascii_case("edge") {
        ("msedge.exe", "Microsoft Edge", "edge://extensions")
    } else {
        ("chrome.exe", "Google Chrome", "chrome://extensions")
    }
}

#[cfg(windows)]
fn open_browser_extensions_impl(browser: &str) -> bool {
    use std::process::Command;
    use std::thread;
    use std::time::Duration;

    let (exe_name, _, page) = browser_target(browser);

    // Chrome/Edge 在已有实例运行时会忽略命令行里的 chrome:// 地址，
    // 因此先找到已打开的浏览器窗口，再用地址栏输入目标地址。
    if find_browser_window(exe_name).is_none() {
        let Some(exe) = find_browser_exe(exe_name) else {
            return false;
        };
        if Command::new(exe).arg("--new-window").spawn().is_err() {
            return false;
        }
        thread::sleep(Duration::from_millis(900));
    }

    let Some(hwnd) = find_browser_window(exe_name) else {
        return false;
    };
    navigate_browser_window(hwnd, page)
}

#[cfg(windows)]
struct BrowserWindowSearch<'a> {
    exe_name: &'a str,
    hwnd: Option<HWND>,
}

#[cfg(windows)]
fn find_browser_window(exe_name: &str) -> Option<HWND> {
    use std::ffi::c_void;

    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, IsWindowVisible,
    };

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = &mut *(lparam as *mut BrowserWindowSearch<'_>);
        if search.hwnd.is_some() {
            return 0;
        }
        if IsWindowVisible(hwnd) == 0 || GetWindowTextLengthW(hwnd) <= 0 {
            return 1;
        }
        if window_belongs_to_exe(hwnd, search.exe_name) {
            search.hwnd = Some(hwnd);
            return 0;
        }
        1
    }

    let mut search = BrowserWindowSearch {
        exe_name,
        hwnd: None,
    };
    unsafe {
        EnumWindows(
            Some(enum_proc),
            &mut search as *mut BrowserWindowSearch<'_> as *mut c_void as isize,
        );
    }
    search.hwnd
}

#[cfg(windows)]
fn window_belongs_to_exe(hwnd: HWND, exe_name: &str) -> bool {
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return false;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut size);
        CloseHandle(process);
        if ok == 0 {
            return false;
        }
        let path =
            std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..size as usize]));
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.eq_ignore_ascii_case(exe_name))
            .unwrap_or(false)
    }
}

#[cfg(windows)]
fn navigate_browser_window(hwnd: HWND, page: &str) -> bool {
    use std::thread;
    use std::time::Duration;

    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VK_CONTROL, VK_RETURN,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetForegroundWindow, ShowWindow, SW_MAXIMIZE,
    };

    // 优先用剪贴板粘贴地址，避免逐字模拟输入在部分输入法/布局下丢字符。
    let mut clipboard = arboard::Clipboard::new().ok();
    let original_text = clipboard.as_mut().and_then(|clip| clip.get_text().ok());
    let clipboard_ready = clipboard
        .as_mut()
        .map(|clip| clip.set_text(page.to_owned()).is_ok())
        .unwrap_or(false);

    unsafe {
        // 扩展页在浏览器普通窗口里打开，顺手最大化，避免用户还要手动放大。
        ShowWindow(hwnd, SW_MAXIMIZE);
        SetForegroundWindow(hwnd);
    }
    thread::sleep(Duration::from_millis(180));

    unsafe {
        keybd_event(VK_CONTROL as u8, 0, 0, 0);
        keybd_event(b'L', 0, 0, 0);
        keybd_event(b'L', 0, KEYEVENTF_KEYUP, 0);
        keybd_event(VK_CONTROL as u8, 0, KEYEVENTF_KEYUP, 0);
    }
    thread::sleep(Duration::from_millis(120));

    if clipboard_ready {
        unsafe {
            keybd_event(VK_CONTROL as u8, 0, 0, 0);
            keybd_event(b'V', 0, 0, 0);
            keybd_event(b'V', 0, KEYEVENTF_KEYUP, 0);
            keybd_event(VK_CONTROL as u8, 0, KEYEVENTF_KEYUP, 0);
        }
    } else {
        // 剪贴板不可用时退回逐字符输入。
        for byte in page.as_bytes() {
            unsafe {
                keybd_event(0, *byte, KEYEVENTF_UNICODE, 0);
                keybd_event(0, *byte, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP, 0);
            }
            thread::sleep(Duration::from_millis(8));
        }
    }

    thread::sleep(Duration::from_millis(100));
    unsafe {
        keybd_event(VK_RETURN as u8, 0, 0, 0);
        keybd_event(VK_RETURN as u8, 0, KEYEVENTF_KEYUP, 0);
    }
    thread::sleep(Duration::from_millis(220));

    if let Some(original) = original_text {
        if let Some(clip) = clipboard.as_mut() {
            let _ = clip.set_text(original);
        }
    }

    true
}

#[cfg(windows)]
fn find_browser_exe(exe_name: &str) -> Option<PathBuf> {
    use std::env;

    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    for hkey in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let subkey = format!(
            r"Software\Microsoft\Windows\CurrentVersion\App Paths\{}",
            exe_name
        );
        if let Ok(key) = RegKey::predef(hkey).open_subkey(subkey) {
            if let Ok(raw) = key.get_value::<String, _>("") {
                let path = PathBuf::from(raw.trim_matches('"'));
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }

    let mut candidates = Vec::new();
    if exe_name.eq_ignore_ascii_case("chrome.exe") {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = env::var_os(var) {
                candidates.push(PathBuf::from(base).join(r"Google\Chrome\Application\chrome.exe"));
            }
        }
    } else {
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(base) = env::var_os(var) {
                candidates.push(PathBuf::from(base).join(r"Microsoft\Edge\Application\msedge.exe"));
            }
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(target_os = "macos")]
fn open_browser_extensions_impl(browser: &str) -> bool {
    use std::process::Command;
    let (_, app_name, page) = browser_target(browser);
    Command::new("open")
        .args(["-a", app_name, page])
        .spawn()
        .is_ok()
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn open_browser_extensions_impl(_browser: &str) -> bool {
    false
}

#[cfg(all(windows, test))]
mod registry_tests {
    use super::*;
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    #[test]
    fn file_associations_register_and_unregister() {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let base = format!(r"Software\FDM\__assoc_test_{}", std::process::id());
        let _ = hkcu.delete_subkey_all(&base);
        let exe = PathBuf::from(r"C:\Program Files\FDM\fdm-desktop.exe");

        assert!(sync_file_associations_at(&hkcu, &base, &exe, true));
        let magnet_cmd: String = hkcu
            .open_subkey(format!(
                r"{base}\Software\Classes\FDM.magnet\shell\open\command"
            ))
            .unwrap()
            .get_value("")
            .unwrap();
        assert!(magnet_cmd.contains("fdm-desktop.exe"));
        assert!(magnet_cmd.contains("\"%1\""));
        let torrent_default: String = hkcu
            .open_subkey(format!(r"{base}\Software\Classes\.torrent"))
            .unwrap()
            .get_value("")
            .unwrap();
        assert_eq!(torrent_default, "FDM.torrent");
        let registered: String = hkcu
            .open_subkey(format!(r"{base}\Software\RegisteredApplications"))
            .unwrap()
            .get_value("FDM")
            .unwrap();
        assert_eq!(registered, r"Software\FDM\Capabilities");

        assert!(sync_file_associations_at(&hkcu, &base, &exe, false));
        assert!(hkcu
            .open_subkey(format!(r"{base}\Software\Classes\FDM.magnet"))
            .is_err());
        let torrent_key = hkcu
            .open_subkey(format!(r"{base}\Software\Classes\.torrent"))
            .unwrap();
        assert_eq!(
            torrent_key.get_value::<String, _>("").unwrap_or_default(),
            ""
        );

        let _ = hkcu.delete_subkey_all(&base);
    }
}
