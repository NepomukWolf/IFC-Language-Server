//! Server configuration parsing and local schema path expansion.
//! This module converts LSP-provided config values into typed settings and expands additive local
//! schema sources from configured files or directories.

use std::fs;
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::LSPAny;

use crate::document::DEFAULT_AST_FILE_SIZE_LIMIT_BYTES;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub overwrite_exp_schema_with_local: Option<PathBuf>,
    pub add_local_schema_to_selection: Vec<PathBuf>,
    pub ast_file_size_limit_bytes: usize,
    pub semantic_tokens_enabled: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            overwrite_exp_schema_with_local: None,
            add_local_schema_to_selection: Vec::new(),
            ast_file_size_limit_bytes: DEFAULT_AST_FILE_SIZE_LIMIT_BYTES,
            semantic_tokens_enabled: true,
        }
    }
}

pub fn parse_server_config(value: &LSPAny) -> ServerConfig {
    let Some(object) = value.as_object() else {
        return ServerConfig::default();
    };

    ServerConfig {
        overwrite_exp_schema_with_local: object
            .get("overwriteExpSchemaWithLocal")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        add_local_schema_to_selection: object
            .get("addLocalSchemaToSelection")
            .map(parse_path_list)
            .unwrap_or_default(),
        ast_file_size_limit_bytes: object
            .get("astFileSizeLimitMb")
            .and_then(parse_size_limit_mb)
            .unwrap_or(DEFAULT_AST_FILE_SIZE_LIMIT_BYTES),
        semantic_tokens_enabled: object
            .get("semanticTokensEnabled")
            .and_then(|value| value.as_bool())
            .unwrap_or(true),
    }
}

fn parse_size_limit_mb(value: &LSPAny) -> Option<usize> {
    let mb = value.as_f64()?;
    if !mb.is_finite() || mb <= 0.0 {
        return None;
    }

    let bytes = mb * 1024.0 * 1024.0;
    (bytes <= usize::MAX as f64).then_some(bytes as usize)
}

fn parse_path_list(value: &LSPAny) -> Vec<PathBuf> {
    if let Some(path) = value.as_str() {
        return vec![PathBuf::from(path)];
    }

    value
        .as_array()
        .into_iter()
        .flat_map(|items| items.iter())
        .filter_map(|item| item.as_str())
        .filter(|item| !item.is_empty())
        .map(PathBuf::from)
        .collect()
}

pub fn expand_schema_candidates(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }

    if !path.is_dir() {
        return Vec::new();
    }

    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };

    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|candidate| {
            candidate.is_file()
                && candidate
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("exp"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_tokens_default_to_enabled() {
        let value = "{}".parse::<LSPAny>().unwrap();

        let config = parse_server_config(&value);

        assert!(config.semantic_tokens_enabled);
    }

    #[test]
    fn parses_semantic_tokens_enabled_flag() {
        let value = r#"{"semanticTokensEnabled":false}"#.parse::<LSPAny>().unwrap();

        let config = parse_server_config(&value);

        assert!(!config.semantic_tokens_enabled);
    }
}
