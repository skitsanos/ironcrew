use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::sync::Arc;

use tempfile::TempDir;

use super::*;

fn write(root: &Path, relative: &str, contents: impl AsRef<[u8]>) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[cfg(unix)]
#[test]
fn source_order_is_stable_and_path_or_content_changes_it() {
    let left = TempDir::new().unwrap();
    let right = TempDir::new().unwrap();
    write(left.path(), "crew.lua", "return 1");
    write(left.path(), "agents/b.lua", "return 2");
    write(right.path(), "agents/b.lua", "return 2");
    write(right.path(), "crew.lua", "return 1");
    let baseline = flow_source_fingerprint(left.path()).unwrap();
    assert_eq!(baseline, flow_source_fingerprint(right.path()).unwrap());

    fs::rename(
        right.path().join("agents/b.lua"),
        right.path().join("agents/c.lua"),
    )
    .unwrap();
    assert_ne!(baseline, flow_source_fingerprint(right.path()).unwrap());
    fs::rename(
        right.path().join("agents/c.lua"),
        right.path().join("agents/b.lua"),
    )
    .unwrap();
    write(right.path(), "agents/b.lua", "return 3");
    assert_ne!(baseline, flow_source_fingerprint(right.path()).unwrap());
}

#[cfg(unix)]
#[test]
fn snapshot_roles_and_child_context_are_lexically_contained() {
    let directory = TempDir::new().unwrap();
    write(directory.path(), "crew.lua", "return 1");
    write(
        directory.path(),
        "agents/root.lua",
        "return {name='root', goal='root'}",
    );
    write(directory.path(), "nested/child.lua", "return 2");
    write(
        directory.path(),
        "nested/agents/child.lua",
        "return {name='child', goal='child'}",
    );
    let snapshot = Arc::new(capture_flow_source(directory.path()).unwrap());
    let roles = snapshot.roles().unwrap();
    assert_eq!(roles.agents.len(), 1, "nested agents are not root roles");

    let root = ConversationSourceContext::root(snapshot);
    assert!(root.source("../outside.lua").is_err());
    assert!(root.source("/outside.lua").is_err());
    let child = root.source("nested/child.lua").unwrap().unwrap();
    let child_context = root.child_for_source(&child).unwrap();
    assert_eq!(child_context.logical_dir(), Path::new("nested"));
    assert_eq!(child_context.direct_children("agents").unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn non_lua_and_secret_files_are_not_read() {
    let directory = TempDir::new().unwrap();
    write(directory.path(), "crew.lua", "return 1");
    let baseline = flow_source_fingerprint(directory.path()).unwrap();
    write(directory.path(), ".env", [0xff, 0xfe]);
    write(directory.path(), "credentials.json", [0xff, 0xfe]);
    write(directory.path(), "private.pem", [0xff, 0xfe]);
    assert_eq!(baseline, flow_source_fingerprint(directory.path()).unwrap());
}

#[cfg(unix)]
#[test]
fn flow_tree_symlinks_are_rejected() {
    let directory = TempDir::new().unwrap();
    write(directory.path(), "crew.lua", "return 1");
    std::os::unix::fs::symlink("crew.lua", directory.path().join("alias.lua")).unwrap();
    let error = flow_source_fingerprint(directory.path()).unwrap_err();
    assert!(error.to_string().contains("symlink"));

    fs::remove_file(directory.path().join("alias.lua")).unwrap();
    fs::create_dir(directory.path().join("lib-source")).unwrap();
    std::os::unix::fs::symlink("lib-source", directory.path().join("_lib")).unwrap();
    let error = flow_source_fingerprint(directory.path()).unwrap_err();
    assert!(error.to_string().contains("symlink"));
}

#[cfg(unix)]
#[test]
fn concurrent_symlink_swap_never_hashes_an_outside_target() {
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    const SWAPS: usize = 512;
    const HASHES: usize = 256;
    let workspace = TempDir::new().unwrap();
    let flow = workspace.path().join("flow");
    let reference = workspace.path().join("reference");
    fs::create_dir_all(&flow).unwrap();
    fs::create_dir_all(&reference).unwrap();
    let safe = b"return 'safe'";
    let secret = b"return 'outside-secret-sentinel'";
    write(&flow, "crew.lua", safe);
    write(&reference, "crew.lua", secret);
    let baseline = flow_source_fingerprint(&flow).unwrap();
    let outside_digest = flow_source_fingerprint(&reference).unwrap();
    assert_ne!(baseline, outside_digest);

    let barrier = Arc::new(Barrier::new(2));
    let swap_barrier = barrier.clone();
    let swap_flow = flow.clone();
    let outside = reference.join("crew.lua");
    let candidate = workspace.path().join("candidate");
    let swapper = thread::spawn(move || {
        swap_barrier.wait();
        for index in 0..SWAPS {
            if index % 2 == 0 {
                std::os::unix::fs::symlink(&outside, &candidate).unwrap();
            } else {
                fs::write(&candidate, safe).unwrap();
            }
            fs::rename(&candidate, swap_flow.join("crew.lua")).unwrap();
            thread::sleep(Duration::from_micros(50));
        }
    });

    barrier.wait();
    for _ in 0..HASHES {
        if let Ok(observed) = flow_source_fingerprint(&flow) {
            assert_eq!(observed, baseline, "outside-tree source was hashed");
            assert_ne!(observed, outside_digest);
        }
    }
    swapper.join().unwrap();
    assert_eq!(flow_source_fingerprint(&flow).unwrap(), baseline);
}

#[cfg(unix)]
#[test]
fn sql_files_participate_in_the_source_fingerprint() {
    let left = TempDir::new().unwrap();
    let right = TempDir::new().unwrap();
    for dir in [&left, &right] {
        write(dir.path(), "crew.lua", "return 1");
        write(dir.path(), "sql/save.sql", "-- ironcrew:op\nSELECT 1;");
    }
    let a = capture_flow_source(left.path()).unwrap();
    let b = capture_flow_source(right.path()).unwrap();
    assert_eq!(
        a.fingerprint(),
        b.fingerprint(),
        "identical trees must match"
    );
    assert_eq!(a.sql_sources().len(), 1);
    assert_eq!(a.sql_sources()[0].0, "save");

    write(right.path(), "sql/save.sql", "-- ironcrew:op\nSELECT 2;");
    let c = capture_flow_source(right.path()).unwrap();
    assert_ne!(
        a.fingerprint(),
        c.fingerprint(),
        "sql edit must change the fingerprint"
    );
}

#[cfg(unix)]
#[test]
fn sql_sources_scopes_to_the_sql_directory_while_fingerprint_covers_all_sql_files() {
    let directory = TempDir::new().unwrap();
    write(directory.path(), "crew.lua", "return 1");
    write(
        directory.path(),
        "sql/save.sql",
        "-- ironcrew:op\nSELECT 1;",
    );
    write(
        directory.path(),
        "sql/sub/nested.sql",
        "-- ironcrew:op\nSELECT 2;",
    );
    write(
        directory.path(),
        "queries/outside.sql",
        "-- ironcrew:op\nSELECT 3;",
    );
    let snapshot = capture_flow_source(directory.path()).unwrap();

    let mut stems: Vec<_> = snapshot
        .sql_sources()
        .into_iter()
        .map(|(stem, _)| stem)
        .collect();
    stems.sort();
    assert_eq!(
        stems,
        vec!["nested", "save"],
        "sql_sources includes sql/ and its nested paths"
    );

    let baseline = snapshot.fingerprint().to_owned();
    write(
        directory.path(),
        "queries/outside.sql",
        "-- ironcrew:op\nSELECT 4;",
    );
    let changed = capture_flow_source(directory.path()).unwrap();
    assert_ne!(
        baseline,
        changed.fingerprint(),
        "queries/ sql files still participate in the fingerprint"
    );
    assert_eq!(
        changed.sql_sources().len(),
        2,
        "queries/ sql files are excluded from sql_sources()"
    );
}

#[cfg(unix)]
#[test]
fn misplaced_sql_files_do_not_enter_lua_roles() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), "crew.lua", "return 1");
    write(dir.path(), "agents/helper.lua", "return 2");
    write(dir.path(), "agents/lookup.sql", "-- ironcrew:op\nSELECT 1;");
    let snapshot = capture_flow_source(dir.path()).unwrap();
    let roles = snapshot.roles().unwrap();
    let agent_paths: Vec<_> = roles
        .agents
        .iter()
        .map(|source| source.relative_path().to_path_buf())
        .collect();
    assert_eq!(agent_paths, vec![Path::new("agents/helper.lua")]);
    // The stray file still participates in the fingerprint.
    assert_eq!(
        snapshot.sql_sources().len(),
        0,
        "not under sql/ so not an operation"
    );
}

#[cfg(not(unix))]
#[test]
fn platforms_without_guaranteed_no_follow_fail_closed() {
    let directory = TempDir::new().unwrap();
    write(directory.path(), "crew.lua", "return 1");
    let error = flow_source_fingerprint(directory.path()).unwrap_err();
    assert!(error.to_string().contains("no-follow"));
}
