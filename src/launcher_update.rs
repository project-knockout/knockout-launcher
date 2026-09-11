use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub const MANIFEST_PATH: &str = env!("KNOCKOUT_LAUNCHER_MANIFEST_PATH");
pub const FILE_PATH_PREFIX: &str = env!("KNOCKOUT_LAUNCHER_FILES_PREFIX");

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LauncherArtifact {
    pub platform: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LauncherManifest {
    pub schema: u32,
    pub version: String,
    pub target_build: String,
    pub target_exe_sha256: String,
    pub target_pak_sha256: String,
    pub launchers: Vec<LauncherArtifact>,
}

pub fn validate_manifest(manifest: &LauncherManifest) -> Result<()> {
    if manifest.schema != 1
        || manifest.target_build != crate::game_manifest::TARGET_BUILD
        || manifest.target_exe_sha256 != crate::game_manifest::TARGET_EXE_SHA256
        || manifest.target_pak_sha256 != crate::game_manifest::TARGET_PAK_SHA256
    {
        bail!("launcher manifest is not for the supported DKO build");
    }
    if manifest.launchers.is_empty() {
        bail!("launcher manifest contains no platform artifacts");
    }
    for artifact in &manifest.launchers {
        if !matches!(
            artifact.platform.as_str(),
            "windows-x86_64" | "linux-x86_64"
        ) || artifact.size == 0
            || artifact.sha256.len() != 64
            || !artifact
                .sha256
                .bytes()
                .all(|value| value.is_ascii_hexdigit())
        {
            bail!("launcher manifest contains invalid artifact metadata");
        }
    }
    Ok(())
}
