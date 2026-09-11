use anyhow::{bail, Context, Result};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const IGNORE_SAVED_LOGIN_ARG: &str = "-RHIgnoreSavedLogin";
pub const ANON_SANITIZER_ARG: &str = "-ini:Engine:[OnlineSubsystem]:SanitizerPlatformService=Anon";
const LOG_COMMANDS: &str = "-LogCmds=LogBrawler Warning";

pub struct Paths {
    pub steam_root: PathBuf,
    pub exe: PathBuf,
    pub proton: PathBuf,
    pub proton_name: String,
    pub compat_data: PathBuf,
    pub prefix: PathBuf,
    pub log: PathBuf,
}

impl Paths {
    pub fn new_isolated(
        steam_root: PathBuf,
        proton_dir: Option<&Path>,
        compat_data: Option<&Path>,
        homedir: &str,
    ) -> Result<Self> {
        if !homedir.starts_with("DivineKnockout") || homedir.contains('/') || homedir.contains('\\')
        {
            bail!("visible-client homedir must be a simple DivineKnockout-prefixed name");
        }
        let game_library = locate_game_library(&steam_root);
        let compat_data = compat_data
            .map(|path| {
                if path.is_absolute() {
                    path.to_owned()
                } else {
                    std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .join(path)
                }
            })
            .unwrap_or_else(|| game_library.join("steamapps/compatdata/1294660"));
        let prefix = compat_data.join("pfx");
        let (proton_dir, proton_name) = resolve_proton_tool(&steam_root, proton_dir)?;
        let proton = proton_dir.join("proton");
        if !proton.is_file() {
            bail!("selected Proton tool has no launcher: {}", proton.display());
        }
        Ok(Self {
            exe: game_executable_in_library(&game_library),
            proton,
            proton_name,
            log: prefix.join(format!(
                "drive_c/users/steamuser/AppData/Local/{homedir}/Saved/Logs/DivineKnockout.log"
            )),
            steam_root,
            compat_data,
            prefix,
        })
    }

    /// Resolve Proton and Steam state normally while using the exact retail
    /// executable selected during desktop-launcher setup.
    pub fn new_selected(
        steam_root: PathBuf,
        proton_dir: Option<&Path>,
        game_executable: &Path,
        homedir: &str,
    ) -> Result<Self> {
        if !game_executable.is_file()
            || !game_executable
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("DivineKnockout.exe"))
        {
            bail!(
                "selected game executable is not DivineKnockout.exe: {}",
                game_executable.display()
            );
        }
        let mut paths = Self::new_isolated(steam_root, proton_dir, None, homedir)?;
        paths.exe = game_executable.to_owned();

        // A selected EXE in a secondary Steam library must use that library's
        // app-specific prefix while Proton itself remains selected by the main
        // Steam configuration.
        if let Some(steamapps) = game_executable
            .ancestors()
            .find(|path| path.file_name().is_some_and(|name| name == "steamapps"))
        {
            if let Some(library) = steamapps.parent() {
                paths.compat_data = library.join("steamapps/compatdata/1294660");
                paths.prefix = paths.compat_data.join("pfx");
                paths.log = paths.prefix.join(format!(
                    "drive_c/users/steamuser/AppData/Local/{homedir}/Saved/Logs/DivineKnockout.log"
                ));
            }
        }
        Ok(paths)
    }

    /// Resolve Proton normally while launching an explicitly selected DKO
    /// executable. Unlike `new_selected`, an explicit compatibility-data path
    /// is preserved so archived builds can use an isolated Proton prefix.
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

fn is_explicit_game_executable(path: &Path) -> bool {
    path.is_file()
        && path.file_name().is_some_and(|name| {
            name.eq_ignore_ascii_case("DivineKnockout.exe")
                || name.eq_ignore_ascii_case(crate::game_manifest::GAME_EXECUTABLE_NAME)
        })
}

fn game_executable_in_library(library: &Path) -> PathBuf {
    library
        .join("steamapps/common/Divine Knockout/DivineKnockout/Binaries/Win64/DivineKnockout.exe")
}

fn steam_library_roots(steam_root: &Path) -> Vec<PathBuf> {
    let mut libraries = vec![steam_root.to_owned()];
    for relative in ["steamapps/libraryfolders.vdf", "config/libraryfolders.vdf"] {
        let Some(document) = std::fs::read_to_string(steam_root.join(relative))
            .ok()
            .and_then(|contents| parse_vdf(&contents))
        else {
            continue;
        };
        let Some(VdfValue::Object(entries)) = document.get("libraryfolders") else {
            continue;
        };
        for (index, value) in entries {
            if index.parse::<u32>().is_err() {
                continue;
            }
            // New Steam uses objects with a path field; older files store
            // library paths directly under their numeric index.
            let Some(path) = value
                .get("path")
                .and_then(VdfValue::text)
                .or_else(|| value.text())
            else {
                continue;
            };
            let library = PathBuf::from(path);
            if library.is_absolute() && !libraries.contains(&library) {
                libraries.push(library);
            }
        }
    }
    libraries
}

fn locate_game_library(steam_root: &Path) -> PathBuf {
    steam_library_roots(steam_root)
        .into_iter()
        .find(|library| game_executable_in_library(library).is_file())
        .unwrap_or_else(|| steam_root.to_owned())
}

// Steam tool IDs are metadata keys, not installation directory names.
#[derive(Debug)]
enum VdfValue {
    Text(String),
    Object(Vec<(String, VdfValue)>),
}

impl VdfValue {
    fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(entries) => entries
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    fn text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            _ => None,
        }
    }
}

fn parse_vdf(contents: &str) -> Option<VdfValue> {
    // Comments must be tokenized alongside strings: // inside a quoted path is data.
    let pattern = Regex::new(r#"//[^\n]*|"((?:\\.|[^"\\])*)"|[{}]|[^\s"{}]+"#).ok()?;
    let tokens: Vec<(String, bool)> = pattern
        .captures_iter(contents)
        .filter_map(|capture| {
            let token = capture.get(0)?.as_str();
            if token.starts_with("//") {
                return None;
            }
            Some((
                capture
                    .get(1)
                    .map(|value| value.as_str().replace(r#"\""#, "\"").replace(r"\\", r"\"))
                    .unwrap_or_else(|| token.to_owned()),
                capture.get(1).is_some(),
            ))
        })
        .collect();
    fn object(
        tokens: &[(String, bool)],
        cursor: &mut usize,
        nested: bool,
        depth: usize,
    ) -> Option<VdfValue> {
        if depth > 32 {
            return None;
        }
        let mut entries = Vec::new();
        while let Some(key) = tokens.get(*cursor) {
            if !key.1 && key.0 == "}" {
                if !nested {
                    return None;
                }
                *cursor += 1;
                return Some(VdfValue::Object(entries));
            }
            if !key.1 && key.0 == "{" {
                return None;
            }
            *cursor += 1;
            let token = tokens.get(*cursor)?;
            *cursor += 1;
            let value = if !token.1 && token.0 == "{" {
                object(tokens, cursor, true, depth + 1)?
            } else if !token.1 && token.0 == "}" {
                return None;
            } else {
                VdfValue::Text(token.0.clone())
            };
            entries.push((key.0.clone(), value));
        }
        if nested {
            None
        } else {
            Some(VdfValue::Object(entries))
        }
    }
    object(&tokens, &mut 0, false, 0)
}

fn compat_tool_name(config: &str, app_id: &str) -> Option<String> {
    let config = parse_vdf(config)?;
    let mapping = config
        .get("InstallConfigStore")?
        .get("Software")?
        .get("Valve")?
        .get("Steam")?
        .get("CompatToolMapping")?;
    mapping
        .get(app_id)?
        .get("name")?
        .text()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn proton_search_roots(steam_root: &Path) -> Vec<PathBuf> {
    let mut roots = vec![steam_root.join("compatibilitytools.d")];
    for library in steam_library_roots(steam_root) {
        roots.push(library.join("steamapps/common"));
        roots.push(library.join("compatibilitytools.d"));
    }
    roots.extend(
        [
            "/usr/local/share/steam/compatibilitytools.d",
            "/usr/share/steam/compatibilitytools.d",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    if let Some(extra) = std::env::var_os("STEAM_EXTRA_COMPAT_TOOLS_PATHS") {
        roots.extend(std::env::split_paths(&extra));
    }
    let mut seen = std::collections::HashSet::new();
    roots.retain(|root| seen.insert(root.clone()));
    roots
}

fn installed_proton_tools(roots: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let mut tools = Vec::new();
    for root in roots {
        let mut directories = vec![root.clone()];
        if let Ok(entries) = std::fs::read_dir(root) {
            let mut children: Vec<_> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|p| p.is_dir())
                .collect();
            children.sort();
            directories.extend(children);
        }
        for directory in directories {
            let metadata = std::fs::read_to_string(directory.join("compatibilitytool.vdf"))
                .ok()
                .and_then(|text| parse_vdf(&text));
            if let Some(VdfValue::Object(entries)) = metadata
                .as_ref()
                .and_then(|v| v.get("compatibilitytools"))
                .and_then(|v| v.get("compat_tools"))
            {
                for (name, entry) in entries {
                    let path = directory.join(
                        entry
                            .get("install_path")
                            .and_then(VdfValue::text)
                            .unwrap_or("."),
                    );
                    if path.join("proton").is_file() {
                        tools.push((path, name.clone()));
                    }
                }
            }
            if directory.join("proton").is_file() {
                if let Some(name) = directory.file_name().and_then(|name| name.to_str()) {
                    tools.push((directory.clone(), name.to_owned()));
                    // Valve's downloaded releases may omit compatibilitytool.vdf.
                    // Derive their Steam IDs from the installed release directory.
                    let id = match name {
                        "Proton - Experimental" => Some("proton_experimental".to_owned()),
                        "Proton Hotfix" => Some("proton_hotfix".to_owned()),
                        "Proton Next" => Some("proton_next".to_owned()),
                        _ => name.strip_prefix("Proton ").and_then(|version| {
                            let (major, minor) = version.split_once('.')?;
                            if !major.chars().all(|c| c.is_ascii_digit())
                                || !minor.chars().all(|c| c.is_ascii_digit())
                            {
                                return None;
                            }
                            Some(if major.parse::<u32>().ok()? >= 5 {
                                format!("proton_{major}")
                            } else {
                                format!("proton_{major}{minor}")
                            })
                        }),
                    };
                    if let Some(id) = id {
                        tools.push((directory, id));
                    }
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    tools.retain(|tool| seen.insert(tool.clone()));
    tools
}

fn resolve_proton_in_roots(steam_root: &Path, roots: &[PathBuf]) -> Result<(PathBuf, String)> {
    let config_path = steam_root.join("config/config.vdf");
    let config = match std::fs::read_to_string(&config_path) {
        Ok(config) => {
            if parse_vdf(&config).is_none() {
                bail!("Could not parse Steam Proton configuration at {}. Restart Steam and select Proton again in Divine Knockout > Properties > Compatibility.", config_path.display());
            }
            config
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Read Steam Proton configuration at {}",
                    config_path.display()
                )
            })
        }
    };
    let selected = compat_tool_name(&config, "1294660").or_else(|| compat_tool_name(&config, "0"));
    let tools = installed_proton_tools(roots);
    if let Some(name) = selected {
        if let Some(tool) = tools.iter().find(|(_, id)| id == &name) {
            return Ok(tool.clone());
        }
        bail!("Steam selected compatibility tool {name:?}, but its Proton launcher was not found. Install that version in Steam Library > Tools, or select an installed Proton version in Divine Knockout > Properties > Compatibility. Steam configuration: {}. Searched: {}. Detected tools: {}",
            config_path.display(), roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
            tools.iter().map(|(_, name)| name.as_str()).collect::<Vec<_>>().join(", "));
    }
    // Steam does not always persist a default mapping. Prefer a stable Valve
    // release when it is absent; never silently replace an explicit selection.
    let stable = tools
        .iter()
        .filter_map(|tool| {
            let version = tool.0.file_name()?.to_str()?.strip_prefix("Proton ")?;
            let (major, minor) = version.split_once('.')?;
            Some((
                (major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?),
                tool,
            ))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, tool)| tool);
    if let Some(tool) = stable
        .or_else(|| tools.iter().find(|(_, id)| id == "proton_experimental"))
        .or_else(|| tools.first())
    {
        return Ok(tool.clone());
    }
    bail!("No installed Proton launcher was found. Install Proton in Steam Library > Tools, then select it in Divine Knockout > Properties > Compatibility and retry. Steam configuration: {}. Searched: {}",
        config_path.display(), roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))
}

fn resolve_proton_tool(
    steam_root: &Path,
    override_dir: Option<&Path>,
) -> Result<(PathBuf, String)> {
    if let Some(directory) = override_dir {
        let directory = if directory.is_absolute() {
            directory.to_owned()
        } else {
            std::env::current_dir()?.join(directory)
        };
        let name = directory
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("custom-proton")
            .to_owned();
        if !directory.join("proton").is_file() {
            bail!("Selected Proton tool has no launcher: {}. Choose the directory containing the proton launcher.", directory.display());
        }
        return Ok((directory, name));
    }
    resolve_proton_in_roots(steam_root, &proton_search_roots(steam_root))
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
    let windows_path = path.to_string_lossy().replace('/', "\\");
    format!("-ABSLOG=Z:{windows_path}")
}

pub fn launch(
    paths: &Paths,
    probe_url: &str,
    homedir: &str,
    username: &str,
    extra_arguments: &[String],
) -> Result<Child> {
    let transport = crate::p2p::bridge::start(probe_url, homedir)?;
    std::fs::create_dir_all(&paths.compat_data)
        .with_context(|| format!("create client prefix {}", paths.compat_data.display()))?;
    if let Some(log_directory) = paths.log.parent() {
        std::fs::create_dir_all(log_directory)
            .with_context(|| format!("create client log directory {}", log_directory.display()))?;
    }
    println!("[dko-client] launching {}", paths.exe.display());
    println!(
        "[dko-client] using Steam compatibility tool {} ({})",
        paths.proton_name,
        paths.proton.display()
    );
    let mut command = Command::new(&paths.proton);
    command
        .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", &paths.steam_root)
        .env("STEAM_COMPAT_DATA_PATH", &paths.compat_data)
        .env("SteamAppId", "1294660")
        .env("SteamGameId", "1294660");
    command
        .env_remove("VK_ICD_FILENAMES")
        .env_remove("VK_DRIVER_FILES");
    let child = command
        .arg("run")
        .arg(&paths.exe)
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
        .context("launch DKO through Proton")?;
    if let Some(transport) = transport {
        transport.detach();
    }
    Ok(child)
}

pub fn has_launch_argument(pid: u32, argument: &str) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|cmdline| {
            cmdline
                .split(|byte| *byte == 0)
                .any(|value| value == argument.as_bytes())
        })
        .unwrap_or(false)
}

pub fn is_game_pid(pid: u32) -> bool {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    is_game_process_identity(&comm, &cmdline)
}

fn is_game_process_identity(comm: &str, cmdline: &[u8]) -> bool {
    comm.trim() == "GameThread"
        && [
            b"DivineKnockout.exe".as_slice(),
            crate::game_manifest::GAME_EXECUTABLE_NAME.as_bytes(),
        ]
        .into_iter()
        .any(|executable| {
            cmdline
                .windows(executable.len())
                .any(|window| window == executable)
        })
}

pub fn find_game_pid_with_homedir(homedir: &str) -> Option<u32> {
    let expected = format!("-homedir={homedir}");
    let mut choices = std::fs::read_dir("/proc")
        .ok()?
        .filter_map(|entry| {
            let pid = entry
                .ok()?
                .file_name()
                .to_string_lossy()
                .parse::<u32>()
                .ok()?;
            (is_game_pid(pid) && has_launch_argument(pid, &expected)).then_some(pid)
        })
        .collect::<Vec<_>>();
    choices.sort_unstable();
    choices.pop()
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
