//! Secrets are stored in private files, never in generated TOML or launch arguments.
use super::{Error, Result, private_dir, private_write};
use std::path::Path;

pub fn put(root: &Path, key: &str) -> Result<String> {
    if key.trim().is_empty() || key.len() > 65536 || key.chars().any(char::is_control) {
        return Err(Error::message(
            "API Key 必须非空、不超过 64 KiB 且不含控制字符",
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let directory = root.join("keys");
    private_dir(&directory)?;
    private_write(&directory.join(&id), key.as_bytes())?;
    Ok(format!("file:{id}"))
}

pub fn get(root: &Path, reference: &str) -> Result<String> {
    let (kind, id) = reference
        .split_once(':')
        .ok_or_else(|| Error::message("无效的密钥引用"))?;
    uuid::Uuid::parse_str(id).map_err(|_| Error::message("无效的密钥 ID"))?;
    match kind {
        "file" => Ok(std::fs::read_to_string(root.join("keys").join(id))?),
        "keychain" => Err(Error::message(
            "此配置使用旧版钥匙串密钥；请编辑账号重新输入 API Key，或从 CC Switch 重新导入",
        )),
        _ => Err(Error::message("未知的密钥存储方式")),
    }
}
