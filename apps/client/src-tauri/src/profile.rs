//! Disposable test profiles.
//!
//! By default the app keeps everything in the per-user data directory and
//! the device identity in the OS credential store under one fixed entry, so
//! two instances on one machine are the same user and the same library, and
//! an experiment can run startup cleanup against real data.
//!
//! Setting `LOCAST_TEST_PROFILE_DIR` to a directory makes ONE process use that
//! directory for its database, library, staging files, logs and webview data,
//! and a key file inside it for the device identity. Two instances started
//! with two directories are two separate users with separate libraries.
//!
//! The variable is the explicit opt-in, and it fails closed: a value that is
//! not a safe disposable location stops the app at startup, it never falls
//! back to the real profile and never quietly redirects it. A directory is
//! accepted only if it is absolute, has no `..`, is not a filesystem root,
//! the user's home or any ancestor of it, does not overlap the real app
//! directories (in either direction), and is either empty/new or already a
//! test profile (it holds the marker file this module writes). With the
//! variable unset nothing here changes behaviour: [`Profile::Default`] selects
//! the original paths and the OS keyring.
//!
//! Cleanup is [`remove_test_profile`], reachable as
//! `locast --remove-test-profile <dir>`: it applies the same checks, requires
//! the marker, and refuses anything else, so it cannot delete real data.

use std::path::{Component, Path, PathBuf};

/// The environment variable that selects a disposable profile.
pub const ENV_TEST_PROFILE_DIR: &str = "LOCAST_TEST_PROFILE_DIR";

/// The file that marks a directory as a disposable test profile.
pub const MARKER_FILE: &str = ".locast-test-profile";

/// The bundle identifier of the real app; the real data directories are
/// named after it.
pub const APP_IDENTIFIER: &str = "com.puretechteam.locast";

/// The identity key file inside a test profile.
pub const IDENTITY_FILE: &str = "identity.key";

/// Why a directory cannot be used as a test profile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("{ENV_TEST_PROFILE_DIR} is empty")]
    Empty,
    #[error("the test profile path must be absolute: {0}")]
    NotAbsolute(String),
    #[error("the test profile path must not contain '..': {0}")]
    ParentComponent(String),
    #[error("{0} is a filesystem root, the home directory or one of its ancestors; use a dedicated empty directory")]
    TooBroad(String),
    #[error("{0} overlaps the real app data location {1}; refusing to use it as a test profile")]
    OverlapsRealData(String, String),
    #[error(
        "{0} is not empty and is not a test profile (no {MARKER_FILE}); \
         use a new or empty directory"
    )]
    NotEmptyAndNotMarked(String),
    #[error("{0} is not a test profile (no {MARKER_FILE}); refusing to delete it")]
    NotMarked(String),
    #[error("could not prepare the test profile {0}: {1}")]
    Io(String, String),
}

/// Which profile this process runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Profile {
    /// The normal per-user directories and the OS credential store.
    Default,
    /// A disposable profile rooted at this (validated) directory.
    Test(PathBuf),
}

/// Locations that must never be used as, inside or above a test profile.
#[derive(Debug, Clone, Default)]
pub struct Protected {
    /// Real app directories: a profile may not be one, inside one, or above one.
    pub dirs: Vec<PathBuf>,
    /// Broader system locations (the per-user data and config bases): a
    /// profile may not be one or above one, but may sit inside one (the
    /// temp directory lives inside the local-data base).
    pub bases: Vec<PathBuf>,
    pub home: Option<PathBuf>,
}

impl Protected {
    /// The real app directories and the home directory of the current user.
    pub fn from_system() -> Self {
        let mut dirs = Vec::new();
        let mut bases = Vec::new();
        for base in [
            dirs::data_dir(),
            dirs::data_local_dir(),
            dirs::config_dir(),
            dirs::cache_dir(),
        ]
        .into_iter()
        .flatten()
        {
            dirs.push(base.join(APP_IDENTIFIER));
            bases.push(base);
        }
        Self {
            dirs,
            bases,
            home: dirs::home_dir(),
        }
    }
}

/// Pick the profile from the value of [`ENV_TEST_PROFILE_DIR`].
pub fn select(
    env_value: Option<&std::ffi::OsStr>,
    protected: &Protected,
) -> Result<Profile, ProfileError> {
    match env_value {
        None => Ok(Profile::Default),
        Some(v) => {
            let dir = validate_test_dir(Path::new(v), protected)?;
            Ok(Profile::Test(dir))
        }
    }
}

/// [`select`] from this process's environment and the real system locations.
pub fn from_env() -> Result<Profile, ProfileError> {
    select(
        std::env::var_os(ENV_TEST_PROFILE_DIR).as_deref(),
        &Protected::from_system(),
    )
}

fn norm(p: &Path) -> String {
    // Compare case-insensitively and with one separator style: Windows paths
    // are, and so would a user's data directory be on a case-insensitive
    // volume anywhere.
    p.to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_lowercase()
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if !rest.starts_with("UNC") {
            return PathBuf::from(rest.to_string());
        }
    }
    p
}

/// `a` is `b` or lies inside it.
fn within(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/"))
}

/// Check `dir` is a safe disposable location and return it with symlinks
/// resolved. Does not create anything.
pub fn validate_test_dir(dir: &Path, protected: &Protected) -> Result<PathBuf, ProfileError> {
    if dir.as_os_str().is_empty() || dir.to_string_lossy().trim().is_empty() {
        return Err(ProfileError::Empty);
    }
    let shown = dir.display().to_string();
    if !dir.is_absolute() {
        return Err(ProfileError::NotAbsolute(shown));
    }
    if dir.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(ProfileError::ParentComponent(shown));
    }
    // Resolve symlinks of what exists, so a link into real data is seen.
    let resolved = resolve(dir);
    let rn = norm(&resolved);

    // A root (`/`, `C:\`) or something so shallow it is a system folder.
    let depth = resolved
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    if depth < 2 {
        return Err(ProfileError::TooBroad(shown));
    }
    // The home directory and everything above it.
    if let Some(home) = &protected.home {
        let hn = norm(&resolve(home));
        if within(&hn, &rn) {
            return Err(ProfileError::TooBroad(shown));
        }
    }
    // The real app directories, in both directions: inside one is a leak
    // into real data, above one would let cleanup reach it.
    for real in &protected.dirs {
        let pn = norm(&resolve(real));
        if within(&rn, &pn) || within(&pn, &rn) {
            return Err(ProfileError::OverlapsRealData(
                shown,
                real.display().to_string(),
            ));
        }
    }
    for base in &protected.bases {
        if within(&norm(&resolve(base)), &rn) {
            return Err(ProfileError::TooBroad(shown));
        }
    }
    // New or empty, or already a test profile.
    if resolved.exists() {
        let marked = resolved.join(MARKER_FILE).is_file();
        let empty = std::fs::read_dir(&resolved)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if !marked && !empty {
            return Err(ProfileError::NotEmptyAndNotMarked(shown));
        }
    }
    Ok(resolved)
}

/// Resolve symlinks and `.` of the longest existing prefix of `p`, keeping
/// the not-yet-existing tail.
fn resolve(p: &Path) -> PathBuf {
    let mut existing = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(c) = existing.canonicalize() {
            let mut out = strip_verbatim(c);
            for t in tail.iter().rev() {
                out.push(t);
            }
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// Create the profile directory (and its subdirectories) and write the
/// marker. Idempotent for a directory that already holds the marker.
pub fn prepare_test_profile(dir: &Path) -> Result<(), ProfileError> {
    let io = |e: std::io::Error| ProfileError::Io(dir.display().to_string(), e.to_string());
    std::fs::create_dir_all(dir).map_err(io)?;
    let marker = dir.join(MARKER_FILE);
    if !marker.exists() {
        std::fs::write(
            &marker,
            "Disposable Locast test profile. Safe to delete with: locast --remove-test-profile <dir>\n",
        )
        .map_err(io)?;
    }
    for sub in ["webview", "logs"] {
        std::fs::create_dir_all(dir.join(sub)).map_err(io)?;
    }
    Ok(())
}

/// Delete a test profile. Applies [`validate_test_dir`] and additionally
/// requires the marker, so a real directory is refused whatever it holds.
pub fn remove_test_profile(dir: &Path, protected: &Protected) -> Result<(), ProfileError> {
    let resolved = validate_test_dir(dir, protected)?;
    if !resolved.join(MARKER_FILE).is_file() {
        return Err(ProfileError::NotMarked(dir.display().to_string()));
    }
    std::fs::remove_dir_all(&resolved)
        .map_err(|e| ProfileError::Io(dir.display().to_string(), e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn protected_for(root: &Path) -> Protected {
        Protected {
            dirs: vec![root.join("AppData").join("Roaming").join(APP_IDENTIFIER)],
            bases: vec![root.join("AppData").join("Roaming")],
            home: Some(root.to_path_buf()),
        }
    }

    #[test]
    fn without_the_variable_the_default_profile_is_selected() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(select(None, &protected_for(t.path())), Ok(Profile::Default));
    }

    #[test]
    fn a_new_directory_is_accepted_and_prepared_with_a_marker() {
        let t = tempfile::tempdir().unwrap();
        let p = protected_for(&t.path().join("home"));
        let dir = t.path().join("profiles").join("alice");
        let v = OsString::from(dir.as_os_str());
        let Profile::Test(resolved) = select(Some(&v), &p).unwrap() else {
            panic!("not a test profile");
        };
        prepare_test_profile(&resolved).unwrap();
        assert!(resolved.join(MARKER_FILE).is_file());
        // Selecting it again (now holding the marker and files) still works.
        std::fs::write(resolved.join("index.sqlite"), b"x").unwrap();
        assert!(select(Some(&v), &p).is_ok());
    }

    #[test]
    fn unsafe_locations_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let real = home.join("AppData").join("Roaming").join(APP_IDENTIFIER);
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("index.sqlite"), b"real").unwrap();
        let p = protected_for(&home);
        let check = |path: &Path| validate_test_dir(path, &p);

        assert_eq!(check(Path::new("")), Err(ProfileError::Empty));
        assert!(matches!(
            check(Path::new("relative/dir")),
            Err(ProfileError::NotAbsolute(_))
        ));
        assert!(matches!(
            check(&t.path().join("a").join("..").join("b")),
            Err(ProfileError::ParentComponent(_))
        ));
        // The real app directory, inside it, above it, and its base.
        for bad in [
            real.clone(),
            real.join("test"),
            real.parent().unwrap().to_path_buf(),
            home.clone(),
            home.join("AppData"),
            t.path().to_path_buf(),
        ] {
            let err = check(&bad).expect_err(&format!("{} must be refused", bad.display()));
            assert!(
                matches!(
                    err,
                    ProfileError::OverlapsRealData(..) | ProfileError::TooBroad(_)
                ),
                "{}: {err:?}",
                bad.display()
            );
        }
        // A directory with other files and no marker.
        let other = t.path().join("documents");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("thesis.docx"), b"x").unwrap();
        assert!(matches!(
            check(&other),
            Err(ProfileError::NotEmptyAndNotMarked(_))
        ));
        // Case differences do not get around the comparison.
        if cfg!(windows) {
            let shouty = PathBuf::from(real.to_string_lossy().to_uppercase());
            assert!(check(&shouty).is_err());
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_into_real_data_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let real = home.join("AppData").join("Roaming").join(APP_IDENTIFIER);
        std::fs::create_dir_all(&real).unwrap();
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(validate_test_dir(&link, &protected_for(&home)).is_err());
    }

    #[test]
    fn cleanup_only_deletes_marked_test_profiles() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let p = protected_for(&home);

        // Real data is never deleted, marker or not.
        let real = home.join("AppData").join("Roaming").join(APP_IDENTIFIER);
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("index.sqlite"), b"real").unwrap();
        std::fs::write(real.join(MARKER_FILE), b"forged").unwrap();
        assert!(remove_test_profile(&real, &p).is_err());
        assert!(real.join("index.sqlite").exists());

        // An unmarked directory is not deleted.
        let plain = t.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("keep.txt"), b"x").unwrap();
        assert!(remove_test_profile(&plain, &p).is_err());
        assert!(plain.join("keep.txt").exists());

        // A prepared test profile is.
        let profile = t.path().join("profiles").join("bob");
        prepare_test_profile(&profile).unwrap();
        std::fs::write(profile.join("index.sqlite"), b"x").unwrap();
        remove_test_profile(&profile, &p).unwrap();
        assert!(!profile.exists());
        // A profile's neighbours are untouched.
        assert!(t.path().join("profiles").exists());
    }

    #[test]
    fn the_system_protected_set_covers_the_real_app_directory() {
        let p = Protected::from_system();
        if let Some(d) = dirs::data_dir() {
            let real = d.join(APP_IDENTIFIER);
            assert!(validate_test_dir(&real, &p).is_err());
            assert!(validate_test_dir(&real.join("x"), &p).is_err());
        }
        if let Some(h) = dirs::home_dir() {
            assert!(validate_test_dir(&h, &p).is_err());
            assert!(validate_test_dir(h.parent().unwrap_or(&h), &p).is_err());
        }
    }
}
