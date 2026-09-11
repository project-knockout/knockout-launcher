//! The server supplies the complete list of files owned by the launcher.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fs, path::Path};

pub const MANIFEST_PATH: &str = env!("KNOCKOUT_GAME_MANIFEST_PATH");
pub const FILE_PATH_PREFIX: &str = env!("KNOCKOUT_GAME_FILES_PREFIX");
pub const GAME_EXECUTABLE_NAME: &str = env!("KNOCKOUT_GAME_EXECUTABLE_NAME");
pub const TARGET_BUILD: &str = env!("KNOCKOUT_TARGET_BUILD");
pub const TARGET_EXE_SHA256: &str = env!("KNOCKOUT_TARGET_EXE_SHA256");
pub const TARGET_PAK_SHA256: &str = env!("KNOCKOUT_TARGET_PAK_SHA256");

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ManagedFile {
    /// Path relative to the isolated game runtime.
    pub path: String,
    pub size: u64,
    pub sha256: String,
    /// Same-origin, hash-addressed server path.
    pub download_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct GameManifest {
    pub schema: u32,
    // Preserve the server's existing wire field without content-specific logic.
    #[serde(rename = "patch_version")]
    pub version: String,
    pub target_build: String,
    pub target_exe_sha256: String,
    pub target_pak_sha256: String,
    pub managed_files: Vec<ManagedFile>,
}

pub fn validate_manifest(manifest: &GameManifest) -> Result<()> {
    if manifest.schema != 2
        || manifest.target_build != TARGET_BUILD
        || manifest.target_exe_sha256 != TARGET_EXE_SHA256
        || manifest.target_pak_sha256 != TARGET_PAK_SHA256
        || manifest.version.is_empty()
        || manifest.managed_files.is_empty()
    {
        bail!("game manifest is not for the supported DKO build");
    }
    let mut paths = HashSet::new();
    let mut total = 0u64;
    for file in &manifest.managed_files {
        validate_relative_path(Path::new(&file.path))?;
        if !file.path.starts_with("DivineKnockout/")
            || !paths.insert(file.path.to_ascii_lowercase())
            || file.size == 0
            || file.sha256.len() != 64
            || !file.sha256.bytes().all(|c| c.is_ascii_hexdigit())
            || file.download_path != format!("{FILE_PATH_PREFIX}{}", file.sha256)
        {
            bail!("game manifest contains invalid file metadata");
        }
        total = total
            .checked_add(file.size)
            .context("game download size overflow")?;
    }
    // Windows treats paths case-insensitively; reject file/directory collisions too.
    for path in &paths {
        for (index, _) in path.match_indices('/') {
            if paths.contains(&path[..index]) {
                bail!("game manifest contains overlapping file paths");
            }
        }
    }
    client_executable(manifest)?;
    Ok(())
}

pub fn client_executable(manifest: &GameManifest) -> Result<&ManagedFile> {
    let mut files = manifest.managed_files.iter().filter(|file| {
        file.path == format!("DivineKnockout/Binaries/Win64/{GAME_EXECUTABLE_NAME}")
    });
    let executable = files
        .next()
        .context("server manifest omitted the game executable")?;
    if files.next().is_some() {
        bail!("server manifest repeats the game executable");
    }
    Ok(executable)
}

pub fn validate_relative_path(path: &Path) -> Result<()> {
    let value = path.to_str().context("game file path is not valid UTF-8")?;
    if value.is_empty()
        || value.contains(['\\', ':', '%', '?', '*', '"', '<', '>', '|'])
        || value.chars().any(char::is_control)
    {
        bail!("unsafe game file path");
    }
    for part in value.split('/') {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|suffix| {
                    suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
                })
            });
        if part.is_empty() || part == "." || part == ".." || part.ends_with(['.', ' ']) || reserved
        {
            bail!("unsafe game file path");
        }
    }
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest)?;
    Ok(hex::encode(digest.finalize()))
}
