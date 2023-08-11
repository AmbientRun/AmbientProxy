use std::path::{Path, PathBuf};

pub async fn load_asset_data(
    assets_path: &PathBuf,
    key: impl AsRef<Path>,
) -> anyhow::Result<Vec<u8>> {
    // load asset from disk
    let file_path = assets_path.join(key).canonicalize()?;
    if !file_path.starts_with(assets_path) {
        tracing::warn!("Asset path is outside of assets directory: {:?}", file_path);
        return Err(anyhow::anyhow!("Asset path is outside of assets directory"));
    };
    Ok(tokio::fs::read(file_path).await?)
}

pub fn path_to_key(path: impl AsRef<Path>) -> anyhow::Result<String> {
    let mut result = String::with_capacity(path.as_ref().as_os_str().len());
    for component in path.as_ref().components() {
        match component {
            std::path::Component::Normal(s) => {
                if !result.is_empty() {
                    result.push('/');
                }
                result.push_str(s.to_str().ok_or(anyhow::anyhow!(
                    "Failed to created key from the path: {:?}",
                    path.as_ref()
                ))?);
            }
            std::path::Component::ParentDir => {
                return Err(anyhow::anyhow!(
                    "Parent directory is not allowed in the path: {:?}",
                    path.as_ref()
                ));
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(anyhow::anyhow!(
                    "Path should be relative: {:?}",
                    path.as_ref()
                ));
            }
            std::path::Component::CurDir => {
                // no-op
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_to_key() {
        let eq = |path, key| assert_eq!(path_to_key(PathBuf::from(path)).unwrap(), key);

        eq("abc", "abc");
        eq("abc/def", "abc/def");
        eq("./abc/def", "abc/def");
    }

    #[test]
    fn test_path_to_key_errors() {
        assert!(path_to_key(PathBuf::from("../abc")).is_err());
        assert!(path_to_key(PathBuf::from("abc/../def")).is_err());
        assert!(path_to_key(PathBuf::from("/abc")).is_err());
    }
}
