//! Fixed community dist artifacts for packages not yet published to npm.
//! Hashes are pinned from magpie-community/plugins' dist/index.json.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha512};
use std::io::Read;

fn integrity(name: &str, version: &str) -> Option<&'static str> {
    match (name, version) {
        ("@magpie-community/opencode-codearts-auth", "0.3.0") => Some(
            "pF8VFhweHlOPJnskCQBD+Lq3R0gqOqTaYfeA0rRuEUBZQCTJCsWDhpBu5u1OdDtnggY3O0mHamjLsTU0LjkDpg==",
        ),
        ("@magpie-community/opencode-raccoon-auth", "0.1.5") => Some(
            "mngd3cAA9l5VMxjAQzyteGqRhKyEtM3dQO4ZbQ+my4uXE27+1tsHBjVsYGiUjFsji0qeC56OWzJ5BvIi3SNlAQ==",
        ),
        _ => None,
    }
}
fn verify(bytes: &[u8], expected: &str) -> Result<()> {
    if STANDARD.encode(Sha512::digest(bytes)) != expected {
        return Err("社区插件发布包 SHA-512 校验失败，未安装".into());
    }
    Ok(())
}
pub(super) fn archive(name: &str, version: &str, candidate: &Path) -> Result<Option<PathBuf>> {
    let Some(expected) = integrity(name, version) else {
        return Ok(None);
    };
    let short = name
        .strip_prefix("@magpie-community/")
        .ok_or("无效社区插件名")?;
    let url = format!(
        "https://raw.githubusercontent.com/magpie-community/plugins/dist/tgz/magpie-community-{short}-{version}.tgz"
    );
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "无法创建社区插件下载请求")?
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|_| "社区插件下载失败，请检查 GitHub 网络连接")?;
    let mut bytes = Vec::new();
    response
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取社区插件发布包")?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("社区插件发布包过大".into());
    }
    verify(&bytes, expected)?;
    let path = candidate.join("release.tgz");
    fs::write(&path, bytes).map_err(|_| "无法保存社区插件发布包")?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_reviewed_versions_use_dist_and_corrupt_bytes_are_rejected() {
        assert!(integrity("@magpie-community/opencode-codearts-auth", "0.3.1").is_none());
        assert!(integrity("other", "0.3.0").is_none());
        for (name, version) in [("codearts", "0.3.0"), ("raccoon", "0.1.5")] {
            let expected =
                integrity(&format!("@magpie-community/opencode-{name}-auth"), version).unwrap();
            assert_eq!(STANDARD.decode(expected).unwrap().len(), 64);
            assert!(verify(b"corrupt artifact", expected).is_err());
        }
        let digest = STANDARD.encode(Sha512::digest(b"fixture"));
        assert!(verify(b"fixture", &digest).is_ok());
        assert!(verify(b"fixture changed", &digest).is_err());
    }
}
