use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;

pub const INDEX_DIR_NAME: &str = ".codex/index";
pub const MANIFEST_FILE: &str = "manifest.json";
pub const ANALYTICS_FILE: &str = "analytics.json";
pub const META_FILE: &str = "meta.jsonl";
pub const LOCK_FILE: &str = "lock";
pub const VECTORS_BASENAME: &str = "vectors";

#[derive(Debug, Clone)]
pub struct IndexPaths {
    pub project_root: PathBuf,
    pub index_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub analytics_path: PathBuf,
    pub meta_path: PathBuf,
    pub lock_path: PathBuf,
}

impl IndexPaths {
    pub fn from_root(root: PathBuf) -> Self {
        let index_dir = root.join(INDEX_DIR_NAME);
        Self {
            project_root: root,
            index_dir: index_dir.clone(),
            manifest_path: index_dir.join(MANIFEST_FILE),
            analytics_path: index_dir.join(ANALYTICS_FILE),
            meta_path: index_dir.join(META_FILE),
            lock_path: index_dir.join(LOCK_FILE),
        }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.index_dir).with_context(|| {
            format!(
                "unable to create index directory at {}",
                self.index_dir.display()
            )
        })
    }

    pub fn basename(&self) -> &'static str {
        VECTORS_BASENAME
    }

    pub fn strip_to_relative<'a>(&self, path: &'a Path) -> &'a Path {
        path.strip_prefix(&self.project_root).unwrap_or(path)
    }
}
