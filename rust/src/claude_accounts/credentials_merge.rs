//! Pure JSON merge of the `claudeAiOauth` key, plus atomic read/write helpers
//! for a `.credentials.json` root, used when switching the active Claude
//! account (design decision D2: switch is a JSON key merge, never a whole-file
//! copy, because `.credentials.json` also holds `mcpOAuth`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

const CREDENTIALS_FILE_NAME: &str = ".credentials.json";

/// Monotonic counter so concurrent writes never share a temp path (mirrors
/// `providers::claude::oauth::credentials_store`'s `PERSIST_TMP_COUNTER`).
static WRITE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum CredentialsMergeError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub fn credentials_file_path(dir: &Path) -> PathBuf {
    dir.join(CREDENTIALS_FILE_NAME)
}

/// Read and parse a `.credentials.json` root from `dir`.
pub fn read_root(dir: &Path) -> Result<serde_json::Value, CredentialsMergeError> {
    let path = credentials_file_path(dir);
    let content = fs::read_to_string(&path)?;
    let value: serde_json::Value = serde_json::from_str(&content)?;
    if !value.is_object() {
        return Err(CredentialsMergeError::Message(
            "credentials file is not a JSON object".to_string(),
        ));
    }
    Ok(value)
}

/// Atomically write a `.credentials.json` root to `dir` (temp file + rename).
pub fn write_root(dir: &Path, value: &serde_json::Value) -> Result<(), CredentialsMergeError> {
    let path = credentials_file_path(dir);
    let serialized = serde_json::to_string_pretty(value)?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".credentials.json.codexbar-tmp.{}.{}",
        std::process::id(),
        WRITE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&tmp, serialized.as_bytes())?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Replace only the `claudeAiOauth` key in `ambient_root` with the value from
/// `source_root`, leaving every sibling key (notably `mcpOAuth`) and every
/// other top-level field byte-identical. Pure: never touches disk, so a
/// malformed/missing ambient root fails without writing anything (Threat
/// Matrix: Destructive file writes).
pub fn merge_claude_ai_oauth(
    ambient_root: &mut serde_json::Value,
    source_root: &serde_json::Value,
) -> Result<(), CredentialsMergeError> {
    let claude_ai_oauth = source_root.get("claudeAiOauth").cloned().ok_or_else(|| {
        CredentialsMergeError::Message(
            "source credentials file is missing claudeAiOauth".to_string(),
        )
    })?;

    let ambient_obj = ambient_root.as_object_mut().ok_or_else(|| {
        CredentialsMergeError::Message("ambient credentials root is not a JSON object".to_string())
    })?;
    ambient_obj.insert("claudeAiOauth".to_string(), claude_ai_oauth);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_preserves_mcp_oauth_and_other_siblings_byte_for_byte() {
        let mut ambient: serde_json::Value = serde_json::from_str(
            r#"{
                "mcpOAuth": {"some-server": {"accessToken": "keepme"}},
                "otherTopLevel": {"nested": true},
                "claudeAiOauth": {"accessToken": "old", "refreshToken": "old-refresh"}
            }"#,
        )
        .unwrap();
        let source: serde_json::Value = serde_json::from_str(
            r#"{"claudeAiOauth": {"accessToken": "new", "refreshToken": "new-refresh", "subscriptionType": "max"}}"#,
        )
        .unwrap();

        merge_claude_ai_oauth(&mut ambient, &source).unwrap();

        assert_eq!(ambient["mcpOAuth"]["some-server"]["accessToken"], "keepme");
        assert_eq!(ambient["otherTopLevel"]["nested"], true);
        assert_eq!(ambient["claudeAiOauth"]["accessToken"], "new");
        assert_eq!(ambient["claudeAiOauth"]["refreshToken"], "new-refresh");
        assert_eq!(ambient["claudeAiOauth"]["subscriptionType"], "max");
    }

    #[test]
    fn merge_fails_without_writing_when_ambient_root_is_not_an_object() {
        let mut ambient = serde_json::Value::Null;
        let source: serde_json::Value =
            serde_json::from_str(r#"{"claudeAiOauth": {"accessToken": "x"}}"#).unwrap();

        let err = merge_claude_ai_oauth(&mut ambient, &source)
            .expect_err("merge into a non-object ambient root must fail");
        assert!(matches!(err, CredentialsMergeError::Message(_)));
        // Ambient root is untouched: still `Null`, never coerced into an
        // object as a side effect of the failed merge.
        assert!(ambient.is_null());
    }

    #[test]
    fn merge_fails_when_source_root_missing_claude_ai_oauth() {
        let mut ambient: serde_json::Value = serde_json::from_str(r#"{"mcpOAuth": {}}"#).unwrap();
        let source: serde_json::Value = serde_json::from_str(r#"{"mcpOAuth": {}}"#).unwrap();

        let err = merge_claude_ai_oauth(&mut ambient, &source)
            .expect_err("source without claudeAiOauth must fail");
        assert!(matches!(err, CredentialsMergeError::Message(_)));
        // Ambient root is untouched by the failed merge.
        assert_eq!(ambient, serde_json::json!({"mcpOAuth": {}}));
    }

    #[test]
    fn read_root_rejects_malformed_json_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(credentials_file_path(dir.path()), "not json").unwrap();
        let err = read_root(dir.path()).expect_err("malformed JSON must error");
        assert!(matches!(err, CredentialsMergeError::Json(_)));
    }

    #[test]
    fn write_root_then_read_root_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let value = serde_json::json!({"claudeAiOauth": {"accessToken": "t"}, "mcpOAuth": {}});
        write_root(dir.path(), &value).unwrap();
        let reloaded = read_root(dir.path()).unwrap();
        assert_eq!(reloaded, value);
        // No leftover temp files after a successful rename.
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("codexbar-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
