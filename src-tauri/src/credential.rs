//! 云凭据生命周期（设计稿 §4.2 / D2 / D3，FR-07.2 硬约束）
//!
//! - secret key **只进系统钥匙串**（macOS Keychain / Windows Credential Manager），
//!   `settings.json` 只存 `credential_user`（endpoint|bucket|prefix 哈希）；
//! - 缺失时**明确报错**（`SYNC_NO_CREDENTIAL`），绝不静默降级为明文（D3）；
//! - 改 endpoint/bucket/prefix → 新 user 哈希 → 旧条目保留并提示可清理（避免误删无法回滚）；
//! - 日志脱敏：secret 永不入日志，AK 只打前 4 位（NFR-3.2）；
//! - N3：CLI / 测试可用 `WIZREADER_SK` 环境变量——**只允许 CLI 与测试路径读取**，
//!   应用（GUI）路径一律走 [`read_secret`]，不碰该变量。

use sha2::{Digest, Sha256};

pub const KEYRING_SERVICE: &str = "wizreader-cloud";

/// keyring 条目的 user 名：`{endpoint}|{bucket}|{prefix}` 哈希前 16 位 hex。
/// （设计稿 D2 写 SHA-1；实现用已有的 sha2 依赖取 SHA-256 前 16 位，抗碰撞更强，语义不变）
pub fn credential_user(endpoint: &str, bucket: &str, prefix: &str) -> String {
    let mut h = Sha256::new();
    h.update(endpoint.trim().trim_end_matches('/').as_bytes());
    h.update(b"|");
    h.update(bucket.as_bytes());
    h.update(b"|");
    h.update(prefix.trim_matches('/').as_bytes());
    let out = h.finalize();
    out[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn entry(user: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, user).map_err(|e| format!("SYNC_KEYRING: {e}"))
}

/// 写入/覆盖 secret（保存配置时调用；UI 只展示 ••••••••）
pub fn store_secret(user: &str, secret: &str) -> Result<(), String> {
    if secret.is_empty() {
        return Err("SYNC_CONFIG_INVALID: secret key 不能为空".into());
    }
    entry(user)?.set_password(secret).map_err(|e| format!("SYNC_KEYRING: {e}"))
}

/// 读取 secret。条目不存在 → `SYNC_NO_CREDENTIAL`（UI 引导重新输入，不降级）。
pub fn read_secret(user: &str) -> Result<String, String> {
    match entry(user)?.get_password() {
        Ok(s) => Ok(s),
        Err(keyring::Error::NoEntry) => {
            Err("SYNC_NO_CREDENTIAL: 钥匙串中无该配置的 secret，请重新输入".into())
        }
        Err(e) => Err(format!("SYNC_KEYRING: {e}")),
    }
}

/// 删除条目（「清除云凭据」按钮）。不存在视为已清除（幂等）。
pub fn delete_secret(user: &str) -> Result<(), String> {
    match entry(user)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("SYNC_KEYRING: {e}")),
    }
}

/// 旧条目清理（改配置后 UI 提示「旧凭据位于 …，可一键清理」）
pub fn delete_secret_if_exists(user: &str) -> bool {
    matches!(entry(user).ok().and_then(|e| e.delete_credential().ok()), Some(()))
}

// ---------------------------------------------------------------- 日志脱敏（NFR-3.2）

/// Access Key 只打前 4 位
pub fn redact_ak(ak: &str) -> String {
    // 固定掩码：只保留前 4 位，星号数量固定，不泄露长度
    let head: String = ak.chars().take(4).collect();
    if ak.chars().count() <= 4 {
        return head;
    }
    format!("{head}******")
}

/// secret 永不入日志：固定输出掩码
pub fn redact_secret() -> &'static str {
    "••••••••"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_credential_user_shape_and_stability() {
        let u1 = credential_user("https://minio.example.com:9000", "wiznotes", "wiznotes");
        assert_eq!(u1.len(), 16);
        assert!(u1.bytes().all(|b| b.is_ascii_hexdigit()));
        // 同参稳定
        let u2 = credential_user("https://minio.example.com:9000/", "wiznotes", "/wiznotes/");
        assert_eq!(u1, u2, "endpoint 尾斜杠 / prefix 首尾斜杠不影响哈希");
        // 变参变化（多套配置并存）
        let u3 = credential_user("https://minio.example.com:9000", "wiznotes", "backup");
        assert_ne!(u1, u3);
    }

    #[test]
    fn test_redact() {
        assert_eq!(redact_ak("WIZACCESSKEY"), "WIZA******");
        assert_eq!(redact_ak("ab"), "ab");
        assert_eq!(redact_secret(), "••••••••");
    }
}
