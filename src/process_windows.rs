use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

pub const IGNORE_SAVED_LOGIN_ARG: &str = "-RHIgnoreSavedLogin";
pub const ANON_SANITIZER_ARG: &str = "-ini:Engine:[OnlineSubsystem]:SanitizerPlatformService=Anon";
const LOG_COMMANDS: &str = "-LogCmds=LogBrawler Warning";

pub struct Paths {
    pub exe: PathBuf,
    pub log: PathBuf,
}

impl Paths {
    pub fn new_isolated(
        steam_root: PathBuf,
        _proton_dir: Option<&Path>,
        _compat_data: Option<&Path>,
        homedir: &str,
    ) -> Result<Self> {
        if !homedir.starts_with("DivineKnockout") || homedir.contains('/') || homedir.contains('\\')
        {
            bail!("visible-client homedir must be a simple DivineKnockout-prefixed name");
        }
        let local_app_data = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .context("LOCALAPPDATA is unavailable; cannot locate the DKO log directory")?;
        Ok(Self {
            exe: locate_game_executable(&steam_root)?,
            log: local_app_data
                .join(homedir)
                .join("Saved/Logs/DivineKnockout.log"),
        })
    }

    /// Resolve paths for the production desktop launcher from the executable
    /// explicitly selected during setup. This path never performs Steam
    /// discovery and never opens a picker during an ordinary deep-link launch.
    pub fn new_selected(homedir: &str) -> Result<Self> {
        if !homedir.starts_with("DivineKnockout") || homedir.contains('/') || homedir.contains('\\')
        {
            bail!("visible-client homedir must be a simple DivineKnockout-prefixed name");
        }
        let local_app_data = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .context("LOCALAPPDATA is unavailable; cannot locate the DKO log directory")?;
        Ok(Self {
            exe: selected_game_executable()?,
            log: local_app_data
                .join(homedir)
                .join("Saved/Logs/DivineKnockout.log"),
        })
    }

    /// Launch an explicitly selected DKO executable without changing the
    /// installation remembered by the normal desktop launcher.
    pub fn new_explicit(
        steam_root: PathBuf,
        proton_dir: Option<&Path>,
        compat_data: Option<&Path>,
        game_executable: &Path,
        homedir: &str,
    ) -> Result<Self> {
        if !is_explicit_game_executable(game_executable) {
            bail!(
                "selected game executable is not an approved Divine Knockout executable: {}",
                game_executable.display()
            );
        }
        let mut paths = Self::new_isolated(steam_root, proton_dir, compat_data, homedir)?;
        paths.exe = game_executable.to_owned();
        Ok(paths)
    }
}

fn game_executable_in_library(library: &Path) -> PathBuf {
    library
        .join("steamapps/common/Divine Knockout/DivineKnockout/Binaries/Win64/DivineKnockout.exe")
}

fn is_divine_knockout_executable(path: &Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("DivineKnockout.exe"))
}

fn resolve_divine_knockout_executable(path: PathBuf) -> PathBuf {
    let nested = path
        .parent()
        .map(|directory| directory.join("DivineKnockout/Binaries/Win64/DivineKnockout.exe"));
    nested
        .filter(|candidate| candidate.is_file())
        .unwrap_or(path)
}

fn is_explicit_game_executable(path: &Path) -> bool {
    path.is_file()
        && path.file_name().is_some_and(|name| {
            name.eq_ignore_ascii_case("DivineKnockout.exe")
                || name.eq_ignore_ascii_case(crate::game_manifest::GAME_EXECUTABLE_NAME)
        })
}

fn configured_game_executable() -> Option<PathBuf> {
    let settings = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\DKOPreservation")
        .ok()?;
    settings
        .get_value::<String, _>("GameExecutable")
        .ok()
        .map(PathBuf::from)
        .map(resolve_divine_knockout_executable)
        .filter(|path| is_divine_knockout_executable(path))
}

pub fn selected_game_executable() -> Result<PathBuf> {
    configured_game_executable().context(
        "Divine Knockout is not configured at the path selected during setup; run setup again",
    )
}

fn steam_game_executables(steam_root: &Path) -> Vec<PathBuf> {
    let mut executables = Vec::new();
    let primary = game_executable_in_library(steam_root);
    if primary.is_file() {
        executables.push(primary);
    }
    let libraries = steam_root.join("steamapps/libraryfolders.vdf");
    let Ok(contents) = std::fs::read_to_string(libraries) else {
        return executables;
    };
    let Ok(pattern) = regex::Regex::new(r#""path"\s+"([^"]+)""#) else {
        return executables;
    };
    executables.extend(
        pattern
            .captures_iter(&contents)
            .filter_map(|capture| capture.get(1))
            .map(|value| PathBuf::from(value.as_str().replace(r"\\", r"\")))
            .map(|library| game_executable_in_library(&library))
            .filter(|candidate| candidate.is_file()),
    );
    executables
}

fn supported_discovered_game_executable(steam_root: &Path) -> Option<PathBuf> {
    steam_game_executables(steam_root)
        .into_iter()
        .find(|candidate| {
            crate::game_manifest::sha256_file(candidate).ok().as_deref()
                == Some(crate::game_manifest::TARGET_EXE_SHA256)
        })
}

fn select_game_executable(suggested: Option<&Path>) -> Result<PathBuf> {
    select_game_executable_with_owner(suggested, std::ptr::null_mut())
}

fn select_game_executable_with_owner(
    suggested: Option<&Path>,
    owner: windows_sys::Win32::Foundation::HWND,
) -> Result<PathBuf> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        CommDlgExtendedError, GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
        OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };

    let mut file = vec![0u16; 32_768];
    if let Some(suggested) = suggested {
        let encoded = suggested.as_os_str().encode_wide().collect::<Vec<_>>();
        if encoded.len() < file.len() {
            file[..encoded.len()].copy_from_slice(&encoded);
        }
    }
    let filter = "DivineKnockout.exe\0DivineKnockout.exe\0Executable files (*.exe)\0*.exe\0\0"
        .encode_utf16()
        .collect::<Vec<_>>();
    let title = "Select your DivineKnockout.exe"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut dialog: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = owner;
    dialog.lpstrFilter = filter.as_ptr();
    dialog.nFilterIndex = 1;
    dialog.lpstrFile = file.as_mut_ptr();
    dialog.nMaxFile = file.len() as u32;
    dialog.lpstrTitle = title.as_ptr();
    dialog.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;

    if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
        let error = unsafe { CommDlgExtendedError() };
        if error == 0 {
            bail!("Divine Knockout was not found automatically and no executable was selected");
        }
        bail!("the Windows file picker failed with error 0x{error:08x}");
    }
    let length = file
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(file.len());
    let selected = PathBuf::from(
        String::from_utf16(&file[..length])
            .context("the selected DivineKnockout.exe path is not valid Unicode")?,
    );
    if !is_divine_knockout_executable(&selected) {
        bail!("select the retail executable named DivineKnockout.exe");
    }
    Ok(selected)
}

pub fn remember_game_executable(path: &Path) -> Result<()> {
    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let (settings, _) = current_user.create_subkey(r"Software\DKOPreservation")?;
    settings.set_value("GameExecutable", &path.to_string_lossy().as_ref())?;
    Ok(())
}

/// Prompt for a retail executable without changing the saved selection. The
/// launcher uses this for an explicit reconfiguration flow so validation can
/// finish before replacing a known-good path.
pub fn select_game_executable_candidate() -> Result<PathBuf> {
    Ok(resolve_divine_knockout_executable(select_game_executable(
        None,
    )?))
}

/// Run on the setup window thread so the picker is modal to the Open button's window.
pub fn select_game_executable_candidate_with_owner(
    owner: windows_sys::Win32::Foundation::HWND,
) -> Result<PathBuf> {
    // Open immediately even when Steam or the previous game drive is unavailable.
    Ok(resolve_divine_knockout_executable(
        select_game_executable_with_owner(None, owner)?,
    ))
}

/// Locate the Steam library as a convenience, but always let the player confirm
/// DivineKnockout.exe in the native picker. Epic discovery is intentionally not
/// supported until that installation path has its own acceptance coverage.
pub fn discover_or_select_and_remember_game_executable(steam_root: &Path) -> Result<PathBuf> {
    let executable = discover_or_select_game_executable_candidate(steam_root)?;
    remember_game_executable(&executable)?;
    Ok(executable)
}

/// Suggest a Steam installation without replacing the saved path before validation.
pub fn discover_or_select_game_executable_candidate(steam_root: &Path) -> Result<PathBuf> {
    let suggested = supported_discovered_game_executable(steam_root)
        .or_else(|| steam_game_executables(steam_root).into_iter().next());
    let executable = select_game_executable(suggested.as_deref())?;
    Ok(resolve_divine_knockout_executable(executable))
}

pub fn locate_game_executable(steam_root: &Path) -> Result<PathBuf> {
    if let Some(configured) = configured_game_executable() {
        return Ok(configured);
    }
    let suggested = supported_discovered_game_executable(steam_root)
        .or_else(|| steam_game_executables(steam_root).into_iter().next());
    let executable = select_game_executable(suggested.as_deref())?;
    remember_game_executable(&executable)?;
    Ok(executable)
}

fn game_arguments(
    probe_url: &str,
    homedir: &str,
    log_path: &Path,
    username: &str,
    extra_arguments: &[String],
) -> Vec<String> {
    let mut arguments = vec![
        "DivineKnockout".to_owned(),
        format!("-homedir={homedir}"),
        "-oss=Anon".to_owned(),
        ANON_SANITIZER_ARG.to_owned(),
        "-nosteam".to_owned(),
        "-noeac".to_owned(),
        "-hirezenv=RETAIL".to_owned(),
        IGNORE_SAVED_LOGIN_ARG.to_owned(),
        format!("-RallyHereURL={probe_url}"),
        "-nosplash".to_owned(),
        unreal_abslog_argument(log_path),
        LOG_COMMANDS.to_owned(),
        "-fileopenlog".to_owned(),
    ];
    arguments.push(format!("-AUTH_LOGIN={username}"));
    arguments.push("-AUTH_TYPE=password".to_owned());
    arguments.push("-ini:RallyHereIntegration:[/Script/RallyHereIntegration.RH_LocalPlayerLoginSubsystem]:bLoginOSSRequireOnlinePlayToLogin=false".to_owned());
    arguments.push("-ini:RallyHereIntegration:[/Script/RallyHereIntegration.RH_LocalPlayerLoginSubsystem]:NicknameOSSName=Anon".to_owned());
    arguments.push("-ini:RallyHereIntegration:[/Script/RallyHereIntegration.RH_LocalPlayerLoginSubsystem]:bNicknameOSSRequireIdentityLogin=true".to_owned());
    arguments.push("-ini:RallyHereIntegration:[/Script/RallyHereIntegration.RH_LocalPlayerLoginSubsystem]:bNicknameOSSRequireOnlinePlayToLogin=false".to_owned());
    arguments.extend(crate::without_auth_password(extra_arguments));
    arguments
}

pub fn unreal_abslog_argument(path: &Path) -> String {
    format!("-ABSLOG={}", path.display())
}

pub fn launch(
    paths: &Paths,
    probe_url: &str,
    homedir: &str,
    username: &str,
    extra_arguments: &[String],
) -> Result<Child> {
    let transport = crate::p2p::bridge::start(probe_url, homedir)?;
    if let Some(log_directory) = paths.log.parent() {
        std::fs::create_dir_all(log_directory)
            .with_context(|| format!("create client log directory {}", log_directory.display()))?;
    }
    println!(
        "[dko-client] launching {} directly on Windows",
        paths.exe.display()
    );
    let mut command = Command::new(&paths.exe);
    if let Some(directory) = paths.exe.parent() {
        command.current_dir(directory);
    }
    let child = command
        .args(game_arguments(
            probe_url,
            homedir,
            &paths.log,
            username,
            extra_arguments,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("launch DKO on Windows")?;
    if let Some(transport) = transport {
        transport.detach();
    }
    Ok(child)
}

fn process_snapshot_with_arguments() -> System {
    let mut system = System::new();
    // refresh_processes() does not request command lines in sysinfo 0.33.
    // Without this, homedir matching never finds the game and the player
    // transport exits after its startup timeout while the game is still open.
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    system
}

fn game_processes() -> Vec<(u32, Vec<OsString>)> {
    let system = process_snapshot_with_arguments();
    system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let name = process.name().to_string_lossy();
            [
                "DivineKnockout.exe",
                crate::game_manifest::GAME_EXECUTABLE_NAME,
            ]
            .iter()
            .any(|expected| name.eq_ignore_ascii_case(expected))
            .then(|| (pid.as_u32(), process.cmd().to_vec()))
        })
        .collect()
}

pub fn has_launch_argument(pid: u32, argument: &str) -> bool {
    game_processes().into_iter().any(|(candidate, command)| {
        candidate == pid
            && command
                .iter()
                .any(|value| value.to_string_lossy() == argument)
    })
}

pub fn is_game_pid(pid: u32) -> bool {
    game_processes()
        .into_iter()
        .any(|(candidate, _)| candidate == pid)
}

pub fn find_game_pid_with_homedir(homedir: &str) -> Option<u32> {
    let expected = format!("-homedir={homedir}");
    game_processes()
        .into_iter()
        .filter_map(|(pid, command)| {
            command
                .iter()
                .any(|value| value.to_string_lossy() == expected)
                .then_some(pid)
        })
        .max()
}

pub fn wait_for_game_pid_with_homedir(homedir: &str, timeout: Duration) -> Result<u32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(pid) = find_game_pid_with_homedir(homedir) {
            return Ok(pid);
        }
        thread::sleep(Duration::from_millis(250));
    }
    bail!("timed out waiting for DivineKnockout.exe homedir {homedir}")
}
