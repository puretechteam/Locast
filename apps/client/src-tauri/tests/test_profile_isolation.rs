//! Two disposable test profiles are two separate users: separate identity,
//! separate database, separate files; and with no profile selected the
//! original (production) choice is unchanged.

use std::path::Path;
use std::sync::Arc;

use locast_client_lib::identity::keystore::{FileKeyring, IdentityService};
use locast_client_lib::profile::{
    prepare_test_profile, remove_test_profile, select, Profile, Protected, IDENTITY_FILE,
    MARKER_FILE,
};
use locast_client_lib::storage::Storage;

async fn open(dir: &Path) -> (Arc<IdentityService>, Storage) {
    let storage = Storage::open(&dir.join("index.sqlite"))
        .await
        .expect("storage");
    let identity = Arc::new(IdentityService::with_keyring(
        Arc::new(FileKeyring::new(dir.join(IDENTITY_FILE))),
        storage.clone(),
    ));
    (identity, storage)
}

fn protected(root: &Path) -> Protected {
    Protected {
        dirs: vec![root.join("real")],
        bases: vec![],
        home: Some(root.join("home")),
    }
}

#[tokio::test]
async fn two_test_profiles_do_not_share_an_identity_or_a_database() {
    let root = tempfile::tempdir().unwrap();
    let p = protected(root.path());
    let mut dirs = Vec::new();
    for name in ["alice", "bob"] {
        let v = root.path().join("profiles").join(name).into_os_string();
        let Profile::Test(dir) = select(Some(&v), &p).unwrap() else {
            panic!("expected a test profile")
        };
        prepare_test_profile(&dir).unwrap();
        dirs.push(dir);
    }
    let (a_id, a_db) = open(&dirs[0]).await;
    let (b_id, b_db) = open(&dirs[1]).await;
    let a = a_id.get_or_create("Alice").await.unwrap();
    let b = b_id.get_or_create("Bob").await.unwrap();
    assert_ne!(
        a.public_key, b.public_key,
        "two profiles are two identities"
    );

    // Each profile's own state is separate.
    sqlx::query("INSERT INTO settings(key, value) VALUES ('who', '\"alice\"')")
        .execute(&a_db.pool())
        .await
        .unwrap();
    let in_b: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = 'who'")
        .fetch_optional(&b_db.pool())
        .await
        .unwrap();
    assert!(
        in_b.is_none(),
        "bob's database does not hold alice's setting"
    );
    assert!(dirs[0].join("index.sqlite").exists() && dirs[1].join("index.sqlite").exists());
    assert_ne!(dirs[0], dirs[1]);

    // A restart of one profile keeps its own identity.
    drop(a_id);
    let (again, again_db) = open(&dirs[0]).await;
    assert_eq!(
        again.get_or_create("Alice").await.unwrap().public_key,
        a.public_key
    );

    // Cleanup removes exactly the profile it was given.
    // Close the databases first (an open file cannot be removed on Windows).
    for db in [&a_db, &again_db, &b_db] {
        db.pool().close().await;
    }
    drop((again, b_id));
    remove_test_profile(&dirs[0], &p).unwrap();
    assert!(!dirs[0].exists());
    assert!(dirs[1].join(MARKER_FILE).exists());
}

#[test]
fn with_no_profile_selected_the_default_paths_and_keyring_are_used() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(select(None, &protected(root.path())), Ok(Profile::Default));
    // The real system locations are refused as a profile, so an environment
    // variable cannot be pointed at the real app directory.
    let sys = Protected::from_system();
    if let Some(d) = dirs::data_dir() {
        let v = d
            .join(locast_client_lib::profile::APP_IDENTIFIER)
            .into_os_string();
        assert!(select(Some(&v), &sys).is_err());
    }
}
