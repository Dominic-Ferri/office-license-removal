//! Path guard applied before every rename or delete.
//!
//! Defence against an empty or wrong home / system root: a target must be an
//! absolute path strictly inside its base, must not be `/`, and must not
//! contain `..` components.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// Check that `home` is usable as the base for per-user deletes.
pub fn check_home(home: &Path) -> Result<(), String> {
    if home.as_os_str().is_empty() {
        return Err("home directory is empty".into());
    }
    if !home.is_absolute() {
        return Err(format!(
            "home directory '{}' is not absolute",
            home.display()
        ));
    }
    if home == Path::new("/") {
        return Err("home directory resolves to '/'".into());
    }
    if has_parent_dir(home) {
        return Err(format!("home directory '{}' contains '..'", home.display()));
    }
    Ok(())
}

/// Check that `path` may be renamed or deleted: it must be absolute, not `/`,
/// free of `..`, and strictly below `base` (component-wise).
pub fn check(path: &Path, base: &Path) -> Result<(), String> {
    if base.as_os_str().is_empty() {
        return Err(format!(
            "refusing '{}': base directory is empty",
            path.display()
        ));
    }
    if !base.is_absolute() || has_parent_dir(base) {
        return Err(format!(
            "refusing '{}': base '{}' is not a clean absolute path",
            path.display(),
            base.display()
        ));
    }
    if path.as_os_str().is_empty() {
        return Err("refusing an empty path".into());
    }
    if !path.is_absolute() {
        return Err(format!("refusing relative path '{}'", path.display()));
    }
    if path == Path::new("/") {
        return Err("refusing '/'".into());
    }
    if has_parent_dir(path) {
        return Err(format!("refusing '{}': contains '..'", path.display()));
    }
    if !path.starts_with(base) || path == base {
        return Err(format!(
            "refusing '{}': not inside '{}'",
            path.display(),
            base.display()
        ));
    }
    Ok(())
}

/// `check` is lexical, so a symlinked directory between `base` and `path`
/// would send a root delete or write outside `base`. This refuses any symlink
/// from the canonical base down to the target's parent, and a final symlink
/// that points outside the base. Returns the path to operate on: the
/// canonical parent joined with the target's file name.
pub fn resolve(path: &Path, base: &Path) -> Result<PathBuf, String> {
    check(path, base)?;
    let refuse = |why: String| format!("refusing '{}': {why}", path.display());
    let canon_base = fs::canonicalize(base)
        .map_err(|e| refuse(format!("cannot resolve '{}': {e}", base.display())))?;
    let rel = path.strip_prefix(base).map_err(|e| refuse(e.to_string()))?;
    let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else {
        return Err(refuse("no file name".into()));
    };
    let mut dir = canon_base.clone();
    for c in parent.components() {
        dir.push(c);
        let md = fs::symlink_metadata(&dir)
            .map_err(|e| refuse(format!("cannot inspect '{}': {e}", dir.display())))?;
        if md.file_type().is_symlink() {
            return Err(refuse(format!("'{}' is a symlink", dir.display())));
        }
    }
    let dir = fs::canonicalize(&dir)
        .map_err(|e| refuse(format!("cannot resolve '{}': {e}", dir.display())))?;
    let target = dir.join(name);
    check(&target, &canon_base)?;
    if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
        let dest = fs::canonicalize(&target)
            .map_err(|e| refuse(format!("cannot resolve symlink: {e}")))?;
        check(&dest, &canon_base).map_err(|e| refuse(format!("symlink target: {e}")))?;
    }
    Ok(target)
}

fn has_parent_dir(p: &Path) -> bool {
    p.components().any(|c| matches!(c, Component::ParentDir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_paths_inside_base() {
        let home = Path::new("/Users/alice");
        assert!(check(
            Path::new("/Users/alice/Library/Group Containers/UBF8T346G9.Office/Licenses"),
            home
        )
        .is_ok());
        assert!(check(
            Path::new("/Library/Preferences/com.microsoft.office.licensingV2.plist"),
            Path::new("/")
        )
        .is_ok());
        assert!(check_home(home).is_ok());
    }

    #[test]
    fn refuses_empty_root_and_outside_paths() {
        let home = Path::new("/Users/alice");
        // Empty path, `/`, and the base itself.
        assert!(check(Path::new(""), home).is_err());
        assert!(check(Path::new("/"), home).is_err());
        assert!(check(Path::new("/"), Path::new("/")).is_err());
        assert!(check(home, home).is_err());
        // Empty base: what an empty-$HOME bug would produce.
        assert!(check(Path::new("/Library/Group Containers"), Path::new("")).is_err());
        // Outside the base, sibling-prefix trick, relative, and `..` escapes.
        assert!(check(Path::new("/Users/bob/Library"), home).is_err());
        assert!(check(Path::new("/Users/alice2/Library"), home).is_err());
        assert!(check(Path::new("Library/Preferences"), home).is_err());
        assert!(check(Path::new("/Users/alice/../bob/Library"), home).is_err());
        assert!(check(Path::new("/Users/alice/Library"), Path::new("relative")).is_err());
        // Unusable homes.
        assert!(check_home(Path::new("")).is_err());
        assert!(check_home(Path::new("/")).is_err());
        assert!(check_home(Path::new("//")).is_err());
        assert!(check_home(Path::new("Users/alice")).is_err());
        assert!(check_home(Path::new("/Users/../")).is_err());
    }

    #[test]
    fn resolve_refuses_symlinks_that_leave_base() {
        // keep(): fixtures are left on disk (no deletes in this workspace).
        let dir = tempfile::Builder::new()
            .prefix("olr-unit-resolve-")
            .tempdir()
            .unwrap()
            .keep();
        let base = dir.join("home");
        let outside = dir.join("outside");
        fs::create_dir_all(base.join("real")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(base.join("real/f"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, base.join("linkdir")).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("real/out")).unwrap();
        std::os::unix::fs::symlink(base.join("real/f"), base.join("real/in")).unwrap();

        let canon = fs::canonicalize(&base).unwrap();
        assert_eq!(
            resolve(&base.join("real/f"), &base).unwrap(),
            canon.join("real/f")
        );
        assert!(resolve(&base.join("real/in"), &base).is_ok());
        assert!(resolve(&base.join("real/absent"), &base).is_ok());
        assert!(resolve(&base.join("linkdir/f"), &base).is_err());
        assert!(resolve(&base.join("real/out"), &base).is_err());
    }
}
