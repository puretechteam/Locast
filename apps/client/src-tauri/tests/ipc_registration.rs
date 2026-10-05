//! IPC wiring contract: the Tauri `invoke_handler` list, the commands the Rust
//! crate defines, and the commands the React client calls must agree.
//!
//! Two bugs of the same shape shipped before this test existed: a command was
//! written and called from the webview but never added to `generate_handler!`,
//! so every call failed at runtime with "command not found" (the leave-room
//! Keep / Delete commands, and others). Nothing at compile time links the
//! three places, so this test reads the sources and checks both directions:
//!
//! - every `#[tauri::command]` function in the crate is registered, and
//! - every command name the frontend invokes by string literal is registered.
//!
//! It runs on every host: it only reads files, so it does not need a webview.
//! A command that is registered but never called from the frontend is allowed
//! (several are reserved for UI that does not exist yet).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn files_under(dir: &Path, extensions: &[&str], out: &mut Vec<PathBuf>) {
    for entry in
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
    {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            files_under(&path, extensions, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| extensions.contains(&e))
        {
            out.push(path);
        }
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Last path segment of every entry in the `generate_handler![ ... ]` list.
/// Commented-out lines are dropped so a disabled entry cannot satisfy the check.
fn registered_commands(lib_rs: &str) -> BTreeSet<String> {
    let start = lib_rs
        .find("tauri::generate_handler![")
        .expect("lib.rs has a generate_handler! list")
        + "tauri::generate_handler![".len();
    let end = lib_rs[start..]
        .find(']')
        .map(|e| start + e)
        .expect("generate_handler! list is closed");
    lib_rs[start..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .flat_map(|l| l.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|path| path.rsplit("::").next().unwrap_or(path).to_string())
        .collect()
}

/// Names of every function directly under a `#[tauri::command]` attribute.
fn defined_commands(src: &str) -> Vec<String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut names = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "#[tauri::command]" {
            continue;
        }
        for next in &lines[i + 1..] {
            let t = next.trim();
            if t.starts_with("#[") || t.starts_with("///") {
                continue;
            }
            let rest = t
                .strip_prefix("pub async fn ")
                .or_else(|| t.strip_prefix("pub fn "))
                .or_else(|| t.strip_prefix("async fn "))
                .or_else(|| t.strip_prefix("fn "))
                .unwrap_or_else(|| panic!("expected a fn after #[tauri::command], got: {t}"));
            names.push(
                rest.chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect(),
            );
            break;
        }
    }
    names
}

/// Command names passed as a string literal to `invoke(...)` or the
/// bindings' `__TAURI_INVOKE(...)`, with an optional `<T>` type argument.
/// `plugin:` names address Tauri plugins, not this crate's handler.
fn invoked_commands(ts: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for needle in ["__TAURI_INVOKE", "invoke"] {
        let mut from = 0;
        while let Some(rel) = ts[from..].find(needle) {
            let at = from + rel;
            from = at + needle.len();
            // `invoke` must be a whole identifier: not `emitInvoke`, `invokeLog`.
            let before = ts[..at].chars().next_back();
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$') {
                continue;
            }
            let mut rest = ts[from..].trim_start();
            if rest.starts_with('<') {
                match rest.find('>') {
                    Some(close) => rest = rest[close + 1..].trim_start(),
                    None => continue,
                }
            }
            let Some(args) = rest.strip_prefix('(') else {
                continue;
            };
            let Some(literal) = args.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some(close) = literal.find('"') else {
                continue;
            };
            let name = &literal[..close];
            if !name.is_empty() && !name.starts_with("plugin:") {
                names.insert(name.to_string());
            }
        }
    }
    names
}

#[test]
fn the_scanners_find_what_they_claim_to() {
    let handler = "tauri::generate_handler![\n    commands::a,\n    // commands::off,\n    \
                   commands::sub::b, room::report::c,\n]";
    assert_eq!(
        registered_commands(handler),
        ["a", "b", "c"].map(String::from).into_iter().collect()
    );

    let rust = "#[tauri::command]\n#[specta::specta]\n/// doc\npub async fn one(\n) {}\n\n\
                #[tauri::command]\npub fn two() {}\nfn helper() {}\n";
    assert_eq!(defined_commands(rust), ["one", "two"]);

    let ts = r#"
        await __TAURI_INVOKE("plain");
        await __TAURI_INVOKE<Foo>("typed", { x });
        const v = await invoke ( "spaced" );
        await invoke("plugin:dialog|open", {});
        emitInvoke("not_a_call");
        const invokeLog = ["nope"];
        await invoke(dynamicName);
    "#;
    assert_eq!(
        invoked_commands(ts),
        ["plain", "typed", "spaced"]
            .map(String::from)
            .into_iter()
            .collect()
    );
}

#[test]
fn every_tauri_command_in_the_crate_is_registered() {
    let registered = registered_commands(&read(&manifest_dir().join("src/lib.rs")));
    let mut sources = Vec::new();
    files_under(&manifest_dir().join("src"), &["rs"], &mut sources);

    let mut defined = Vec::new();
    for file in &sources {
        for name in defined_commands(&read(file)) {
            defined.push((name, file.clone()));
        }
    }
    assert!(
        defined.len() >= 30,
        "scanner found only {} #[tauri::command] functions; it is broken",
        defined.len()
    );

    let missing: Vec<String> = defined
        .iter()
        .filter(|(name, _)| !registered.contains(name))
        .map(|(name, file)| format!("{name} ({})", file.display()))
        .collect();
    assert!(
        missing.is_empty(),
        "#[tauri::command] functions missing from generate_handler! in lib.rs: {missing:?}"
    );
}

#[test]
fn every_command_the_frontend_invokes_is_registered() {
    let registered = registered_commands(&read(&manifest_dir().join("src/lib.rs")));
    let mut sources = Vec::new();
    files_under(&manifest_dir().join("../src"), &["ts", "tsx"], &mut sources);

    let mut invoked: BTreeSet<String> = BTreeSet::new();
    for file in &sources {
        invoked.extend(invoked_commands(&read(file)));
    }
    assert!(
        invoked.len() >= 30,
        "scanner found only {} invoked commands; it is broken",
        invoked.len()
    );

    let unregistered: Vec<&String> = invoked
        .iter()
        .filter(|name| !registered.contains(*name))
        .collect();
    assert!(
        unregistered.is_empty(),
        "the frontend invokes commands that are not in generate_handler!: {unregistered:?}"
    );
}
