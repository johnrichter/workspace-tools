//! The reachable-directory-set resolver: the launched-in `cwd` plus every
//! directory a session's merged settings actually attach via
//! `permissions.additionalDirectories`.
//!
//! This is deliberately narrower than "every sibling directory on disk" —
//! an unattached sibling is not reachable via `Glob`/`Grep`/`Read` this
//! session, so a search/lint/find pass must never see it.
//!
//! This module is not a [`crate::skipset`] consumer: it only lists
//! top-level reachable directories (from settings + `--dir`), it never
//! recursively walks their contents. The skip-set applies to a *content*
//! walk inside a reachable directory (which files/subdirs to scan); that
//! walk lives in [`crate::mdwalk`], the enumerator that consumes
//! [`crate::skipset`].

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// File-system locations of the four settings scopes that can grant
/// `additionalDirectories`, contributed in this fixed order (later scopes
/// are appended after earlier ones, so earlier entries win any tie during
/// dedup).
///
/// `managed` and `user` are injected explicitly rather than hardcoded so
/// tests can point them at a fixture tree; `project_root` is discovered
/// per-call by [`find_project_root`] and is not part of this struct.
#[derive(Debug, Clone, Default)]
pub struct ScopePaths {
    /// System-wide policy settings. Optional: most machines have none.
    pub managed: Option<PathBuf>,
    /// The current user's global settings (`~/.claude/settings.json`).
    pub user: Option<PathBuf>,
}

impl ScopePaths {
    /// The real managed-policy and per-user settings locations for the
    /// running OS, for production use. Tests should build a `ScopePaths`
    /// directly instead, pointed at a fixture tree.
    pub fn for_current_os() -> Self {
        ScopePaths {
            managed: Some(managed_settings_path()),
            user: user_settings_path(),
        }
    }
}

#[cfg(target_os = "macos")]
fn managed_settings_path() -> PathBuf {
    PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
}

#[cfg(not(target_os = "macos"))]
fn managed_settings_path() -> PathBuf {
    PathBuf::from("/etc/claude-code/managed-settings.json")
}

fn user_settings_path() -> Option<PathBuf> {
    dirs_home().map(|home| home.join(".claude").join("settings.json"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Resolves the reachable directory set for a session launched in `cwd`.
///
/// **Contract:**
/// - `cwd` is always the first entry, unconditionally present (never
///   dropped, never existence-checked — the caller's cwd is reachable by
///   definition even if it has since been deleted).
/// - Each settings scope's `permissions.additionalDirectories` array
///   contributes entries, in this order: `scopes.managed`, `scopes.user`,
///   the walked-up project `.claude/settings.json`, then that same
///   project's `.claude/settings.local.json`.
/// - A leading `~` or `~/...` in any scope's entry expands against the
///   current user's home directory before the absolute/relative check
///   below. A `~otheruser`-style entry is left untouched -- this resolver
///   has no notion of another account's home directory, only the current
///   process's.
/// - A relative entry from the project/project-local scopes resolves
///   against the project root (the directory holding the `.claude/` that
///   supplied it); a relative entry from the managed/user scopes resolves
///   against `cwd`, since those files carry no project-root concept of
///   their own. Absolute entries (including an expanded `~` entry) are
///   used as-is.
/// - `extra_dirs` (e.g. from a `--dir` flag) are appended last, verbatim —
///   the caller's job, not this resolver's, to know what they mean.
/// - The final list is deduplicated (first occurrence wins, stable order)
///   and filtered to entries that exist on disk and are directories —
///   except `cwd`, which is always kept.
/// - Every settings scope is optional: a missing, unreadable, or
///   unparseable settings file — or one missing the key — contributes no
///   entries, silently. This never fails the resolution.
///
/// Errors if `cwd` cannot be made absolute (e.g. the process has no
/// working directory and `cwd` is relative), or if some scope carries a
/// `~`-relative `additionalDirectories` entry and `$HOME` cannot be
/// resolved -- that case must surface as a visible failure rather than
/// silently vanish the entry from the reachable set.
pub fn reachable_set(
    cwd: &Path,
    scopes: &ScopePaths,
    extra_dirs: &[PathBuf],
) -> std::io::Result<Vec<PathBuf>> {
    reachable_set_with_home(cwd, scopes, extra_dirs, dirs_home().as_deref())
}

/// [`reachable_set`] with the home directory supplied explicitly instead of
/// read from `$HOME`. Production goes through `reachable_set` (which reads
/// the environment once); tests call this directly so they can exercise
/// present/absent-home behavior without mutating the process-global `$HOME`
/// (which would race under parallel `cargo test`).
fn reachable_set_with_home(
    cwd: &Path,
    scopes: &ScopePaths,
    extra_dirs: &[PathBuf],
    home: Option<&Path>,
) -> std::io::Result<Vec<PathBuf>> {
    let abs_cwd = std::path::absolute(cwd)?;
    let abs_cwd = clean(&abs_cwd);

    let mut candidates = Vec::new();
    if let Some(managed) = &scopes.managed {
        candidates.extend(additional_directories(managed, &abs_cwd, home)?);
    }
    if let Some(user) = &scopes.user {
        candidates.extend(additional_directories(user, &abs_cwd, home)?);
    }
    if let Some(root) = find_project_root(&abs_cwd) {
        let claude_dir = root.join(".claude");
        candidates.extend(additional_directories(
            &claude_dir.join("settings.json"),
            &root,
            home,
        )?);
        candidates.extend(additional_directories(
            &claude_dir.join("settings.local.json"),
            &root,
            home,
        )?);
    }
    candidates.extend(extra_dirs.iter().cloned());

    let mut reachable = vec![abs_cwd.clone()];
    let mut seen = std::collections::HashSet::new();
    seen.insert(abs_cwd);
    for candidate in candidates {
        if !seen.insert(candidate.clone()) {
            continue;
        }
        if candidate.is_dir() {
            reachable.push(candidate);
        }
    }
    Ok(reachable)
}

/// Walks up from `dir` (inclusive) to the nearest ancestor holding a
/// `.claude/` subdirectory — the root that project/project-local settings
/// resolve against. Returns `None` if no ancestor up to the filesystem
/// root has one, in which case those two scopes contribute nothing.
fn find_project_root(dir: &Path) -> Option<PathBuf> {
    let mut current = dir;
    loop {
        if current.join(".claude").is_dir() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

#[derive(Deserialize, Default)]
struct Settings {
    #[serde(default)]
    permissions: Permissions,
}

#[derive(Deserialize, Default)]
struct Permissions {
    #[serde(default, rename = "additionalDirectories")]
    additional_directories: Vec<PathBuf>,
}

/// Reads `path`'s `permissions.additionalDirectories`, expands a leading
/// `~`/`~/...` in each entry against `home`, then resolves the result
/// against `base_dir` if it's still relative. Any settings-file failure —
/// missing, unreadable, unparseable JSON, absent key — yields no entries;
/// every settings scope is optional and must degrade silently.
///
/// Errors only if some entry needs `~`-expansion and `home` is `None`.
fn additional_directories(
    path: &Path,
    base_dir: &Path,
    home: Option<&Path>,
) -> std::io::Result<Vec<PathBuf>> {
    let Ok(data) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    let Ok(settings) = serde_json::from_str::<Settings>(&data) else {
        return Ok(Vec::new());
    };
    settings
        .permissions
        .additional_directories
        .into_iter()
        .map(|entry| {
            let expanded = expand_tilde(entry, home)?;
            let joined = if expanded.is_absolute() {
                expanded
            } else {
                base_dir.join(expanded)
            };
            Ok(clean(&joined))
        })
        .collect()
}

/// Expands a bare `~` or a leading `~/...` in `entry` against `home`.
/// Every other entry — already-absolute, plain-relative, or a
/// `~otheruser`-style form — passes through unchanged.
///
/// Errors if `entry` needs expansion and `home` is `None`: a `~`-relative
/// additionalDirectories entry with no resolvable home directory must
/// surface as a visible failure, not silently disappear from the
/// reachable set.
fn expand_tilde(entry: PathBuf, home: Option<&Path>) -> std::io::Result<PathBuf> {
    use std::path::Component;

    let mut components = entry.components();
    let Some(Component::Normal(first)) = components.next() else {
        return Ok(entry);
    };
    if first != "~" {
        return Ok(entry);
    }
    let Some(home) = home else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "cannot expand '~' in additionalDirectories entry {}: HOME is not set",
                entry.display()
            ),
        ));
    };
    let rest = components.as_path();
    if rest.as_os_str().is_empty() {
        Ok(home.to_path_buf())
    } else {
        Ok(home.join(rest))
    }
}

/// Lexically normalizes `path`: collapses `.` and resolves `..` against
/// its preceding component, without touching the filesystem (no symlink
/// resolution) — matching how a settings-file path is meant to be
/// interpreted (plain path arithmetic, not a sandboxed lookup).
fn clean(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    let mut rooted = false;
    // Count of poppable `Normal` components on `out`. A `..` may only pop one
    // of these — never a root/prefix, and never a leading `..` of a relative
    // path (those must be preserved). At the root with none left, a `..` is
    // absorbed (root's parent is root), matching how the OS reads `/..`.
    let mut normals: usize = 0;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normals > 0 {
                    out.pop();
                    normals -= 1;
                } else if !rooted {
                    out.push(Component::ParentDir);
                }
            }
            Component::Prefix(_) | Component::RootDir => {
                out.push(component);
                rooted = true;
            }
            Component::Normal(_) => {
                out.push(component);
                normals += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_settings(path: &Path, dirs: &[&Path]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let entries: Vec<String> = dirs
            .iter()
            .map(|d| serde_json::to_string(&d.to_string_lossy()).unwrap())
            .collect();
        fs::write(
            path,
            format!(
                r#"{{"permissions":{{"additionalDirectories":[{}]}}}}"#,
                entries.join(",")
            ),
        )
        .unwrap();
    }

    fn scopes_with(managed: Option<&Path>, user: Option<&Path>) -> ScopePaths {
        ScopePaths {
            managed: managed.map(Path::to_path_buf),
            user: user.map(Path::to_path_buf),
        }
    }

    #[test]
    fn cwd_always_first_and_present_with_no_scopes() {
        let root = TempDir::new().unwrap();
        let got = reachable_set(root.path(), &ScopePaths::default(), &[]).unwrap();
        assert_eq!(got, vec![clean(&std::path::absolute(root.path()).unwrap())]);
    }

    #[test]
    fn merges_all_four_scopes_in_order() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        let cwd = project.join("sub").join("deep");
        let managed_dir = root.path().join("managed-attached");
        let user_dir = root.path().join("user-attached");
        let project_dir = root.path().join("project-attached");
        let local_dir = root.path().join("local-attached");
        for d in [&cwd, &managed_dir, &user_dir, &project_dir, &local_dir] {
            fs::create_dir_all(d).unwrap();
        }

        let managed_path = root.path().join("managed-settings.json");
        write_settings(&managed_path, &[&managed_dir]);
        let user_path = root.path().join("user-settings.json");
        write_settings(&user_path, &[&user_dir]);
        write_settings(
            &project.join(".claude").join("settings.json"),
            &[&project_dir],
        );
        write_settings(
            &project.join(".claude").join("settings.local.json"),
            &[&local_dir],
        );

        let got = reachable_set(
            &cwd,
            &scopes_with(Some(&managed_path), Some(&user_path)),
            &[],
        )
        .unwrap();

        let want = [&cwd, &managed_dir, &user_dir, &project_dir, &local_dir]
            .map(|p| clean(&std::path::absolute(p).unwrap()));
        assert_eq!(got, want);
    }

    #[test]
    fn dedups_first_occurrence_wins() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("project");
        let shared = root.path().join("shared");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&shared).unwrap();

        let managed_path = root.path().join("managed-settings.json");
        write_settings(&managed_path, &[&shared]);
        write_settings(&cwd.join(".claude").join("settings.json"), &[&shared]);

        let got = reachable_set(&cwd, &scopes_with(Some(&managed_path), None), &[]).unwrap();
        let want = [&cwd, &shared].map(|p| clean(&std::path::absolute(p).unwrap()));
        assert_eq!(got, want);
    }

    #[test]
    fn nonexistent_and_non_dir_entries_are_dropped() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let missing = root.path().join("does-not-exist");
        let a_file = root.path().join("plain-file.txt");
        fs::write(&a_file, "not a directory").unwrap();

        let managed_path = root.path().join("managed-settings.json");
        write_settings(&managed_path, &[&missing, &a_file]);

        let got = reachable_set(&cwd, &scopes_with(Some(&managed_path), None), &[]).unwrap();
        assert_eq!(got, vec![clean(&std::path::absolute(&cwd).unwrap())]);
    }

    #[test]
    fn strictly_drops_unattached_siblings() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("cwd");
        let unattached = root.path().join("unattached-sibling");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&unattached).unwrap();

        let got = reachable_set(&cwd, &ScopePaths::default(), &[]).unwrap();
        assert_eq!(got, vec![clean(&std::path::absolute(&cwd).unwrap())]);
    }

    // The subtle rule: managed/user-scope relative entries resolve against
    // cwd, project/local-scope relative entries resolve against the project
    // root — and these must differ when cwd sits below the project root.
    #[test]
    fn relative_base_differs_by_scope() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        let cwd = project.join("sub").join("deep");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(project.join(".claude")).unwrap();

        let sibling_of_cwd = project.join("sub").join("sibling-of-cwd");
        let sibling_of_project = root.path().join("sibling-of-project");
        fs::create_dir_all(&sibling_of_cwd).unwrap();
        fs::create_dir_all(&sibling_of_project).unwrap();

        let managed_path = root.path().join("managed-settings.json");
        fs::write(
            &managed_path,
            r#"{"permissions":{"additionalDirectories":["../sibling-of-cwd"]}}"#,
        )
        .unwrap();
        fs::write(
            project.join(".claude").join("settings.json"),
            r#"{"permissions":{"additionalDirectories":["../sibling-of-project"]}}"#,
        )
        .unwrap();

        let got = reachable_set(&cwd, &scopes_with(Some(&managed_path), None), &[]).unwrap();
        let want = [&cwd, &sibling_of_cwd, &sibling_of_project]
            .map(|p| clean(&std::path::absolute(p).unwrap()));
        assert_eq!(got, want);
    }

    #[test]
    fn tilde_relative_user_scope_entry_expands_against_home() {
        let root = TempDir::new().unwrap();
        let home = root.path().join("fake-home");
        let kb = home.join("kb");
        fs::create_dir_all(&kb).unwrap();
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).unwrap();

        let user_path = home.join("settings.json");
        fs::write(
            &user_path,
            r#"{"permissions":{"additionalDirectories":["~/kb"]}}"#,
        )
        .unwrap();

        let got =
            reachable_set_with_home(&cwd, &scopes_with(None, Some(&user_path)), &[], Some(&home))
                .unwrap();
        let want = [&cwd, &kb].map(|p| clean(&std::path::absolute(p).unwrap()));
        assert_eq!(got, want);
    }

    #[test]
    fn bare_tilde_entry_expands_to_home_itself() {
        let root = TempDir::new().unwrap();
        let home = root.path().join("fake-home");
        fs::create_dir_all(&home).unwrap();
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).unwrap();

        let user_path = home.join("settings.json");
        fs::write(
            &user_path,
            r#"{"permissions":{"additionalDirectories":["~"]}}"#,
        )
        .unwrap();

        let got =
            reachable_set_with_home(&cwd, &scopes_with(None, Some(&user_path)), &[], Some(&home))
                .unwrap();
        let want = [&cwd, &home].map(|p| clean(&std::path::absolute(p).unwrap()));
        assert_eq!(got, want);
    }

    #[test]
    fn other_user_tilde_form_is_not_expanded_and_is_dropped_as_relative() {
        let root = TempDir::new().unwrap();
        let home = root.path().join("fake-home");
        fs::create_dir_all(&home).unwrap();
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).unwrap();

        let user_path = home.join("settings.json");
        fs::write(
            &user_path,
            r#"{"permissions":{"additionalDirectories":["~otheruser/kb"]}}"#,
        )
        .unwrap();

        let got =
            reachable_set_with_home(&cwd, &scopes_with(None, Some(&user_path)), &[], Some(&home))
                .unwrap();
        // `~otheruser/kb` isn't expanded and resolves relative to cwd (the
        // user scope's relative base), where it doesn't exist -- dropped
        // like any other nonexistent candidate.
        assert_eq!(got, vec![clean(&std::path::absolute(&cwd).unwrap())]);
    }

    #[test]
    fn missing_home_with_no_tilde_entry_does_not_error() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let target = root.path().join("plain-target");
        fs::create_dir_all(&target).unwrap();

        let managed_path = root.path().join("managed-settings.json");
        write_settings(&managed_path, &[&target]);

        let got = reachable_set_with_home(&cwd, &scopes_with(Some(&managed_path), None), &[], None)
            .unwrap();
        let want = [&cwd, &target].map(|p| clean(&std::path::absolute(p).unwrap()));
        assert_eq!(got, want);
    }

    #[test]
    fn missing_home_with_tilde_entry_present_errors_loudly() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).unwrap();

        let managed_path = root.path().join("managed-settings.json");
        fs::write(
            &managed_path,
            r#"{"permissions":{"additionalDirectories":["~/kb"]}}"#,
        )
        .unwrap();

        let result =
            reachable_set_with_home(&cwd, &scopes_with(Some(&managed_path), None), &[], None);
        assert!(result.is_err(), "expected an error, got {result:?}");
    }

    #[test]
    fn absolute_entries_used_as_is_across_all_scopes() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("project");
        let managed_target = root.path().join("managed-abs-target");
        let user_target = root.path().join("user-abs-target");
        for d in [&cwd, &managed_target, &user_target] {
            fs::create_dir_all(d).unwrap();
        }
        fs::create_dir_all(cwd.join(".claude")).unwrap();

        let managed_path = root.path().join("managed-settings.json");
        write_settings(&managed_path, &[&managed_target]);
        let user_path = root.path().join("user-settings.json");
        write_settings(&user_path, &[&user_target]);

        let got = reachable_set(
            &cwd,
            &scopes_with(Some(&managed_path), Some(&user_path)),
            &[],
        )
        .unwrap();
        let want =
            [&cwd, &managed_target, &user_target].map(|p| clean(&std::path::absolute(p).unwrap()));
        assert_eq!(got, want);
    }

    #[test]
    fn missing_or_garbage_settings_file_is_skipped_silently() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).unwrap();

        let missing_managed = root.path().join("does-not-exist.json");
        let garbage_user = root.path().join("garbage-user.json");
        fs::write(&garbage_user, "not json at all {{{").unwrap();

        let got = reachable_set(
            &cwd,
            &scopes_with(Some(&missing_managed), Some(&garbage_user)),
            &[],
        )
        .unwrap();
        assert_eq!(got, vec![clean(&std::path::absolute(&cwd).unwrap())]);
    }

    #[test]
    fn missing_additional_directories_key_contributes_nothing() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let managed_path = root.path().join("managed-settings.json");
        fs::write(&managed_path, r#"{"permissions":{}}"#).unwrap();

        let got = reachable_set(&cwd, &scopes_with(Some(&managed_path), None), &[]).unwrap();
        assert_eq!(got, vec![clean(&std::path::absolute(&cwd).unwrap())]);
    }

    #[test]
    fn find_project_root_walk_up_from_subdir_matches_root_launch() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        let sub = project.join("sub").join("deep");
        fs::create_dir_all(&sub).unwrap();
        fs::create_dir_all(project.join(".claude")).unwrap();

        assert_eq!(
            find_project_root(&clean(&std::path::absolute(&sub).unwrap())),
            Some(clean(&std::path::absolute(&project).unwrap()))
        );
        assert_eq!(
            find_project_root(&clean(&std::path::absolute(&project).unwrap())),
            Some(clean(&std::path::absolute(&project).unwrap()))
        );
    }

    #[test]
    fn find_project_root_never_falsely_finds_a_claude_within_a_tree_that_has_none() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("no-claude-anywhere-up");
        fs::create_dir_all(&cwd).unwrap();
        // The walk climbs to the filesystem root, so on a machine that keeps
        // a `.claude` at the temp-dir root it may legitimately stop there --
        // the invariant that stays hermetic is that it never reports a root
        // *inside* this temp tree, which has no `.claude` anywhere in it.
        let found = find_project_root(&std::path::absolute(&cwd).unwrap());
        if let Some(found) = found {
            assert!(
                !found.starts_with(root.path()),
                "find_project_root found a .claude inside a tree that has none: {}",
                found.display()
            );
        }
    }

    #[test]
    fn claude_dir_present_but_empty_contributes_no_entries() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("project").join("sub");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(root.path().join("project").join(".claude")).unwrap();

        let got = reachable_set(&cwd, &ScopePaths::default(), &[]).unwrap();
        assert_eq!(got, vec![clean(&std::path::absolute(&cwd).unwrap())]);
    }

    #[test]
    fn extra_dirs_extend_the_set() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("project");
        let extra = root.path().join("add-dir-target");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&extra).unwrap();

        let got =
            reachable_set(&cwd, &ScopePaths::default(), std::slice::from_ref(&extra)).unwrap();
        let want = [&cwd, &extra].map(|p| clean(&std::path::absolute(p).unwrap()));
        assert_eq!(got, want);
    }

    #[test]
    fn extra_dirs_that_do_not_exist_are_still_dropped() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let missing = root.path().join("nonexistent-add-dir");

        let got = reachable_set(&cwd, &ScopePaths::default(), &[missing]).unwrap();
        assert_eq!(got, vec![clean(&std::path::absolute(&cwd).unwrap())]);
    }

    #[test]
    fn upward_escaping_relative_entry_resolves_lexically() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("a").join("b").join("c");
        fs::create_dir_all(project.join(".claude")).unwrap();
        let target_outside = root.path().join("target-outside");
        fs::create_dir_all(&target_outside).unwrap();
        fs::write(
            project.join(".claude").join("settings.json"),
            r#"{"permissions":{"additionalDirectories":["../../../target-outside"]}}"#,
        )
        .unwrap();

        let got = reachable_set(&project, &ScopePaths::default(), &[]).unwrap();
        let want = [&project, &target_outside].map(|p| clean(&std::path::absolute(p).unwrap()));
        assert_eq!(got, want);
    }

    #[test]
    fn nonexistent_cwd_is_kept_unconditionally() {
        let root = TempDir::new().unwrap();
        let nonexistent = root.path().join("deleted-before-launch");
        let got = reachable_set(&nonexistent, &ScopePaths::default(), &[]).unwrap();
        assert_eq!(
            got,
            vec![clean(&std::path::absolute(&nonexistent).unwrap())]
        );
    }

    // Parity with the Go SPEC's symlink test: a project root reached only
    // via a symlinked ancestor must still resolve relative entries — the
    // walk-up (`current.join(".claude").is_dir()`) follows the symlink to
    // find `.claude`, and the returned root is the symlinked path itself
    // (not its target), matching `find_project_root`'s doc contract.
    #[test]
    fn symlinked_project_root_resolves_relative_entries() {
        let root = TempDir::new().unwrap();
        let real_project = root.path().join("real-project");
        fs::create_dir_all(real_project.join(".claude")).unwrap();
        fs::write(
            real_project.join(".claude").join("settings.json"),
            r#"{"permissions":{"additionalDirectories":["../sibling"]}}"#,
        )
        .unwrap();
        let sibling = root.path().join("sibling");
        fs::create_dir_all(&sibling).unwrap();

        let symlinked_project = root.path().join("link-project");
        #[cfg(unix)]
        let symlink_result = std::os::unix::fs::symlink(&real_project, &symlinked_project);
        #[cfg(not(unix))]
        let symlink_result: std::io::Result<()> = Err(std::io::Error::other("unsupported"));
        if symlink_result.is_err() {
            eprintln!("skipping: symlinks unsupported on this filesystem");
            return;
        }

        let got = reachable_set(&symlinked_project, &ScopePaths::default(), &[]).unwrap();
        let want = [&symlinked_project, &sibling].map(|p| clean(&std::path::absolute(p).unwrap()));
        assert_eq!(got, want);
    }

    // Deeper than the 3-level upward_escaping test: the entry escapes past
    // the temp root entirely into nonexistent literal ".." components that
    // clean() cannot pop away (out is already empty). The candidate must
    // still be existence-checked like any other, and dropped when it
    // doesn't resolve to a real directory — no panic on the malformed path.
    #[test]
    fn deeply_nested_escape_past_existing_ancestors_is_dropped_if_target_missing() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("a").join("b");
        fs::create_dir_all(project.join(".claude")).unwrap();
        fs::write(
            project.join(".claude").join("settings.json"),
            r#"{"permissions":{"additionalDirectories":["../../../../../../nonexistent-far-outside"]}}"#,
        )
        .unwrap();

        let got = reachable_set(&project, &ScopePaths::default(), &[]).unwrap();
        assert_eq!(got, vec![clean(&std::path::absolute(&project).unwrap())]);
    }

    // `clean` must match Go's `filepath.Clean` lexical semantics, including
    // the subtle root case: a `..` at the filesystem root is absorbed (root's
    // parent is root), while a leading `..` of a *relative* path is preserved.
    // Getting the root case wrong makes an escape-past-root entry resolve to a
    // different literal path than the SPEC, which can defeat dedup.
    #[test]
    fn clean_matches_filepath_clean_semantics() {
        let cases = [
            ("/..", "/"),
            ("/../foo", "/foo"),
            ("/a/b/../../../../foo", "/foo"),
            ("/a/./b", "/a/b"),
            ("/a//b", "/a/b"),
            ("../a", "../a"),
            ("../../foo", "../../foo"),
            ("a/../..", ".."),
        ];
        for (input, want) in cases {
            assert_eq!(
                clean(Path::new(input)),
                PathBuf::from(want),
                "clean({input:?})"
            );
        }
    }

    // Wrong-typed additionalDirectories (a bare string instead of an array)
    // fails Settings deserialization the same way garbage JSON does — must
    // degrade to zero entries, not panic.
    #[test]
    fn wrong_typed_additional_directories_string_is_skipped_silently() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let managed_path = root.path().join("managed-settings.json");
        fs::write(
            &managed_path,
            r#"{"permissions":{"additionalDirectories":"not-an-array"}}"#,
        )
        .unwrap();

        let got = reachable_set(&cwd, &scopes_with(Some(&managed_path), None), &[]).unwrap();
        assert_eq!(got, vec![clean(&std::path::absolute(&cwd).unwrap())]);
    }

    // Adversarial: a tilde entry that then walks back out of home via `..`
    // (`~/../escape`) must still expand the leading `~` against home, then
    // let `clean()` collapse the `..` lexically against the expanded path —
    // same as any other post-expansion path arithmetic, no special-casing.
    #[test]
    fn tilde_entry_with_trailing_parent_dir_escapes_home_after_expansion() {
        let root = TempDir::new().unwrap();
        let home = root.path().join("fake-home");
        fs::create_dir_all(&home).unwrap();
        let escape_target = root.path().join("escape");
        fs::create_dir_all(&escape_target).unwrap();
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).unwrap();

        let user_path = home.join("settings.json");
        fs::write(
            &user_path,
            r#"{"permissions":{"additionalDirectories":["~/../escape"]}}"#,
        )
        .unwrap();

        let got =
            reachable_set_with_home(&cwd, &scopes_with(None, Some(&user_path)), &[], Some(&home))
                .unwrap();
        let want = [&cwd, &escape_target].map(|p| clean(&std::path::absolute(p).unwrap()));
        assert_eq!(got, want);
    }

    // Adversarial: `~` only expands when it's the *leading* component — a
    // `~` appearing mid-path (`foo/~/bar`) is not a home-expansion form at
    // all (no shell treats it as one), so `expand_tilde` must leave it
    // untouched and let it resolve as an ordinary relative path segment.
    #[test]
    fn mid_path_tilde_component_is_not_expanded() {
        let root = TempDir::new().unwrap();
        let home = root.path().join("fake-home");
        fs::create_dir_all(&home).unwrap();
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        // The literal relative path `foo/~/bar` resolved against cwd (the
        // user scope's relative base).
        let literal_target = cwd.join("foo").join("~").join("bar");
        fs::create_dir_all(&literal_target).unwrap();

        let user_path = home.join("settings.json");
        fs::write(
            &user_path,
            r#"{"permissions":{"additionalDirectories":["foo/~/bar"]}}"#,
        )
        .unwrap();

        let got =
            reachable_set_with_home(&cwd, &scopes_with(None, Some(&user_path)), &[], Some(&home))
                .unwrap();
        let want = [&cwd, &literal_target].map(|p| clean(&std::path::absolute(p).unwrap()));
        assert_eq!(got, want);
    }

    // Array of non-strings (numbers) also fails deserialization of
    // Vec<PathBuf> element-wise — same silent-skip contract.
    #[test]
    fn wrong_typed_additional_directories_non_string_elements_are_skipped_silently() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let managed_path = root.path().join("managed-settings.json");
        fs::write(
            &managed_path,
            r#"{"permissions":{"additionalDirectories":[1,2,3]}}"#,
        )
        .unwrap();

        let got = reachable_set(&cwd, &scopes_with(Some(&managed_path), None), &[]).unwrap();
        assert_eq!(got, vec![clean(&std::path::absolute(&cwd).unwrap())]);
    }
}
