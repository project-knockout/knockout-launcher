use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use std::path::Path;

const SETUP_FILENAME_PREFIX: &str = "DKO-Setup--";
const SETUP_FILENAME_SUFFIX: &str = ".exe";
const MAX_SERVER_URL_BYTES: usize = 160;
const SERVER_BINDING_MAGIC: &[u8] = b"PROJECT_KNOCKOUT_SERVER_V1\0";
fn server_url_from_embedded_binding(path: &Path) -> Result<String> {
    let body = std::fs::read(path).with_context(|| format!("read setup {}", path.display()))?;
    let length = body
        .get(body.len().saturating_sub(2)..)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_le_bytes)
        .map(usize::from)
        .context("setup has no embedded server binding")?;
    if length == 0
        || length > MAX_SERVER_URL_BYTES
        || body.len() < length + 2 + SERVER_BINDING_MAGIC.len()
    {
        bail!("setup has an invalid embedded server binding");
    }
    let url_start = body.len() - 2 - length;
    let magic_start = url_start - SERVER_BINDING_MAGIC.len();
    if &body[magic_start..url_start] != SERVER_BINDING_MAGIC {
        bail!("setup has no embedded server binding");
    }
    String::from_utf8(body[url_start..body.len() - 2].to_vec())
        .context("setup server binding is not valid UTF-8")
}

pub fn server_url_from_setup_executable(path: &Path) -> Result<String> {
    if let Ok(server_url) = server_url_from_embedded_binding(path) {
        return Ok(server_url);
    }
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("Windows setup filename is not valid Unicode")?;
    let stem = filename
        .strip_suffix(SETUP_FILENAME_SUFFIX)
        .context("Windows setup filename must end in .exe")?;
    let stem = strip_browser_duplicate_suffix(stem);
    let encoded = stem.strip_prefix(SETUP_FILENAME_PREFIX).context(
        "This is not a server-bound DKO setup download. Download it again from the DKO portal.",
    )?;
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .context("DKO setup filename contains an invalid server binding")?;
    if decoded.is_empty() || decoded.len() > MAX_SERVER_URL_BYTES {
        bail!("DKO setup server binding is empty or too long");
    }
    String::from_utf8(decoded).context("DKO setup server binding is not valid UTF-8")
}

fn strip_browser_duplicate_suffix(stem: &str) -> &str {
    let Some((candidate, suffix)) = stem.rsplit_once(" (") else {
        return stem;
    };
    let Some(number) = suffix.strip_suffix(')') else {
        return stem;
    };
    if !number.is_empty() && number.bytes().all(|value| value.is_ascii_digit()) {
        candidate
    } else {
        stem
    }
}
