use std::path::{Path, PathBuf};

pub fn command_in_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };

    find_executable_in_paths(program, std::env::split_paths(&path)).is_some()
}

pub fn find_executable_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_executable_in_paths(program, std::env::split_paths(&path))
}

fn find_executable_in_paths(
    program: &str,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    paths
        .into_iter()
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cue-path-lookup-{name}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_file(path: &Path) {
        std::fs::write(path, "#!/bin/sh\n").expect("write test file");
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path)
            .expect("stat test file")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod test file");
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}

    #[test]
    fn finds_executable_in_ordered_paths() {
        let first = temp_dir("first");
        let second = temp_dir("second");
        let target = second.join("cue-foo");
        write_file(&target);
        make_executable(&target);

        assert_eq!(
            find_executable_in_paths("cue-foo", [first.clone(), second.clone()]),
            Some(target)
        );

        std::fs::remove_dir_all(first).expect("remove first temp dir");
        std::fs::remove_dir_all(second).expect("remove second temp dir");
    }

    #[cfg(unix)]
    #[test]
    fn ignores_non_executable_files_on_unix() {
        let dir = temp_dir("non-executable");
        let target = dir.join("cue-foo");
        write_file(&target);

        assert_eq!(find_executable_in_paths("cue-foo", [dir.clone()]), None);

        std::fs::remove_dir_all(dir).expect("remove temp dir");
    }
}
