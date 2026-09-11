fn main() {
    let configuration = configure_build();
    println!("cargo:rerun-if-changed=assets/favicon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resources = winres::WindowsResource::new();
        resources.set_icon("assets/favicon.ico");
        let version = &configuration["KNOCKOUT_LAUNCHER_VERSION"];
        let mut parts = version
            .split('.')
            .map(|part| part.parse::<u64>().unwrap())
            .collect::<Vec<_>>();
        parts.push(0);
        let packed_version = parts
            .into_iter()
            .fold(0, |result, part| (result << 16) | part);
        resources.set("FileVersion", version);
        resources.set("ProductVersion", version);
        resources.set_version_info(winres::VersionInfo::FILEVERSION, packed_version);
        resources.set_version_info(winres::VersionInfo::PRODUCTVERSION, packed_version);
        resources.set("FileDescription", "Project KNOCKOUT Launcher");
        resources.set("ProductName", "Project KNOCKOUT");
        resources.set("InternalName", "Project-KNOCKOUT.exe");
        resources.set("OriginalFilename", "Project-KNOCKOUT.exe");
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu")
            && !std::env::var("HOST")
                .unwrap_or_default()
                .contains("windows")
        {
            resources
                .set_windres_path("x86_64-w64-mingw32-windres")
                .set_ar_path("x86_64-w64-mingw32-ar");
        }
        resources
            .compile()
            .expect("embed the Project KNOCKOUT launcher icon");
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
            let output = std::path::PathBuf::from(
                std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR for Windows resources"),
            )
            .join("resource.o");
            println!("cargo:rustc-link-arg-bins={}", output.display());
        }
    }
}

// Environment variables take precedence over the untracked local build file.
fn configure_build() -> std::collections::HashMap<String, String> {
    let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let path = root.join(".env");
    println!("cargo:rerun-if-changed={}", path.display());
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => panic!("cannot read local build configuration: {error}"),
    };
    let mut local = std::collections::HashMap::new();
    for line in contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let (key, value) = line
            .split_once('=')
            .expect(".env entries must be KEY=value");
        assert!(
            local.insert(key.trim(), value.trim()).is_none(),
            "duplicate .env key"
        );
    }
    let mut configuration = std::collections::HashMap::new();
    for key in [
        "KNOCKOUT_GAME_MANIFEST_PATH",
        "KNOCKOUT_GAME_FILES_PREFIX",
        "KNOCKOUT_GAME_EXECUTABLE_NAME",
        "KNOCKOUT_LAUNCHER_MANIFEST_PATH",
        "KNOCKOUT_LAUNCHER_FILES_PREFIX",
        "KNOCKOUT_LAUNCH_PREFIX",
        "KNOCKOUT_LAUNCH_ACK_PATH",
        "KNOCKOUT_LAUNCH_STATUS_PATH",
        "KNOCKOUT_TRANSPORT_PATH",
        "KNOCKOUT_GAME_HOMEDIR",
        "KNOCKOUT_RUNTIME_DIRECTORY",
        "KNOCKOUT_TARGET_BUILD",
        "KNOCKOUT_TARGET_EXE_SHA256",
        "KNOCKOUT_TARGET_PAK_SHA256",
        "KNOCKOUT_LAUNCHER_VERSION",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
        let mut value = std::env::var(key)
            .ok()
            .or_else(|| local.get(key).map(|s| s.to_string()))
            .unwrap_or_else(|| {
                panic!("set {key} in the environment or a local .env before building")
            });
        assert!(
            !value.is_empty() && !value.chars().any(char::is_control),
            "invalid {key}"
        );
        if key.ends_with("_SHA256") {
            assert!(
                value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit()),
                "invalid {key}"
            );
            value.make_ascii_lowercase();
        } else if key == "KNOCKOUT_TARGET_BUILD" || key == "KNOCKOUT_LAUNCHER_VERSION" {
            let parts = value.split('.').collect::<Vec<_>>();
            let count = if key == "KNOCKOUT_TARGET_BUILD" { 4 } else { 3 };
            assert!(
                parts.len() == count
                    && parts.iter().all(|part| {
                        !part.is_empty()
                            && part.bytes().all(|c| c.is_ascii_digit())
                            && part.parse::<u16>().is_ok()
                    }),
                "invalid {key}"
            );
        } else if key.ends_with("_HOMEDIR") || key.ends_with("_DIRECTORY") {
            assert!(
                !value.contains(['/', '\\', ':']) && value != "." && value != "..",
                "invalid {key}"
            );
            if key.ends_with("_HOMEDIR") {
                assert!(value.starts_with("DivineKnockout"), "invalid {key}");
            }
        } else if key.ends_with("_NAME") {
            assert!(
                !value.contains(['/', '\\', ':']) && value.ends_with(".exe"),
                "invalid {key}"
            );
        } else {
            assert!(
                value.starts_with('/')
                    && !value.starts_with("//")
                    && !value.contains(['?', '#', '\\', '%', ':'])
                    && !value.split('/').any(|part| part == "." || part == ".."),
                "invalid {key}"
            );
            if key.ends_with("_PREFIX") {
                assert!(value.ends_with('/'), "{key} must end with /");
            }
        }
        println!("cargo:rustc-env={key}={value}");
        configuration.insert(key.to_owned(), value);
    }
    configuration
}
