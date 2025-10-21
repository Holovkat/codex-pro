use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use ignore::WalkBuilder;

/// Collects all indexable files from `project_root`, respecting ignore rules and skipping
/// generated index artifacts.
pub fn collect_indexable_files(project_root: &Path, index_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut builder = WalkBuilder::new(project_root);
    builder.hidden(false);
    builder.git_ignore(true);
    builder.git_global(true);
    builder.git_exclude(true);
    builder.add_custom_ignore_filename(".index-ignore");

    let mut files = Vec::new();
    for entry in builder.build() {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        if path.starts_with(index_dir) {
            continue;
        }
        if should_skip(path) {
            continue;
        }
        files.push(path.to_path_buf());
    }
    files.sort();
    Ok(files)
}

fn should_skip(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|n| n.to_str())
        && (name == "Cargo.lock" || name.ends_with(".log"))
    {
        return true;
    }
    if let Some(ancestor) = path.parent()
        && ancestor.components().any(|comp| {
            matches!(
                comp.as_os_str().to_str(),
                Some(".git") | Some("target") | Some("node_modules")
            )
        })
    {
        return true;
    }
    false
}
