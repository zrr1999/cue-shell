use std::fs;
use std::path::{Component, Path, PathBuf};

#[test]
fn markdown_local_links_resolve() {
    let repo_root = repo_root();
    let markdown_files = collect_markdown_files(&repo_root);
    let mut broken = Vec::new();

    for file in markdown_files {
        let text = fs::read_to_string(&file)
            .unwrap_or_else(|err| panic!("read {}: {err}", file.display()));

        for (line_no, line) in text.lines().enumerate() {
            for target in markdown_link_targets(line) {
                let Some(target_path) = local_path_target(target) else {
                    continue;
                };

                let resolved = normalize_path(file.parent().unwrap().join(target_path));
                if !resolved.exists() {
                    broken.push(format!(
                        "{}:{} -> {}",
                        file.strip_prefix(&repo_root).unwrap().display(),
                        line_no + 1,
                        target
                    ));
                }
            }
        }
    }

    assert!(
        broken.is_empty(),
        "broken local Markdown links:\n{}",
        broken.join("\n")
    );
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("cue-core lives under crates/")
        .to_path_buf()
}

fn collect_markdown_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_markdown_files_from(root, &mut files);
    files.sort();
    files
}

fn collect_markdown_files_from(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|err| panic!("read {}: {err}", dir.display()));

    for entry in entries {
        let entry = entry.unwrap_or_else(|err| panic!("read entry in {}: {err}", dir.display()));
        let path = entry.path();
        let name = entry.file_name();

        if path.is_dir() {
            if name == ".git" || name == "target" {
                continue;
            }
            collect_markdown_files_from(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            files.push(path);
        }
    }
}

fn markdown_link_targets(line: &str) -> Vec<&str> {
    let mut targets = Vec::new();
    let mut rest = line;

    while let Some(start) = rest.find("](") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find(')') else {
            break;
        };
        targets.push(&rest[..end]);
        rest = &rest[end + 1..];
    }

    targets
}

fn local_path_target(target: &str) -> Option<&str> {
    let target = target.trim();

    if target.is_empty()
        || target.starts_with('#')
        || target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
        || target.contains("://")
    {
        return None;
    }

    let without_fragment = target.split_once('#').map_or(target, |(path, _)| path);
    if without_fragment.is_empty() {
        None
    } else {
        Some(without_fragment)
    }
}

fn normalize_path(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }

    normalized
}
