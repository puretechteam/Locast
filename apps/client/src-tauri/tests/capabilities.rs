//! Capability (Tauri ACL) contract for the main window.
//!
//! `capabilities/default.json` is the only thing that decides which plugin
//! commands the webview may call. It was trimmed to the least-privilege set
//! the React client actually uses:
//!
//!   core:event:allow-listen    `listen` from `@tauri-apps/api/event`
//!   core:event:allow-unlisten  the unlisten function `listen` returns
//!   dialog:allow-open          `invoke("plugin:dialog|open")`, the import picker
//!
//! App commands (the typed bindings) are not ACL-gated: `build.rs` declares no
//! app manifest. Media is served by the `locast://` protocol, which needs no
//! permission. Nothing else should reach the webview, so a grant that creeps
//! back in (or a frontend call that needs one) must fail a test, not ship.
//!
//! Two layers:
//!
//! - Static (every host, only reads files): the capability file has exactly
//!   the expected shape and permissions, and the permissions match what the
//!   non-test frontend sources actually use, in both directions (a missing
//!   grant and a stale grant both fail).
//! - Runtime (`mod acl`, Linux and macOS): a mock app built from the real
//!   `tauri::generate_context!()`, so the ACL is the one `tauri-build`
//!   resolved from `capabilities/`, answers allowed and denied plugin commands
//!   through Tauri's real invoke path.
//!
//! Run with `cargo test -p locast-client --test capabilities -j 1`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The full grant for the `main` window. Changing this list is a security
/// decision: adding a permission needs the justification docs/ARCHITECTURE.md
/// asks for (which webview code needs it, and why Rust cannot do the work).
const EXPECTED_PERMISSIONS: [&str; 3] = [
    "core:event:allow-listen",
    "core:event:allow-unlisten",
    "dialog:allow-open",
];

/// `@tauri-apps/*` modules the webview may import, each with the permissions
/// its use implies. `core` is `invoke` (app commands are not ACL-gated) and
/// `convertFileSrc` (no IPC at all); `event` is `listen` and its unlisten.
const ALLOWED_TAURI_IMPORTS: [(&str, &[&str]); 2] = [
    ("@tauri-apps/api/core", &[]),
    (
        "@tauri-apps/api/event",
        &["core:event:allow-listen", "core:event:allow-unlisten"],
    ),
];

/// Tauri's built-in plugins. Their permissions carry a `core:` prefix.
const CORE_PLUGINS: [&str; 9] = [
    "app",
    "event",
    "image",
    "menu",
    "path",
    "resources",
    "tray",
    "webview",
    "window",
];

const GRANT_HINT: &str = "adding or widening a capability needs a justification per \
     docs/ARCHITECTURE.md (which webview code needs it and why the work cannot stay in Rust); \
     update EXPECTED_PERMISSIONS in tests/capabilities.rs only together with that justification";

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&read(path)).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn expected() -> BTreeSet<String> {
    EXPECTED_PERMISSIONS.iter().map(|s| s.to_string()).collect()
}

/// `plugin:<name>|<cmd>` -> the `allow-` permission that grants it.
fn permission_for(plugin_command: &str) -> String {
    let rest = plugin_command
        .strip_prefix("plugin:")
        .unwrap_or_else(|| panic!("not a plugin command: {plugin_command}"));
    let (plugin, cmd) = rest
        .split_once('|')
        .unwrap_or_else(|| panic!("plugin command without `|`: {plugin_command}"));
    assert!(
        !plugin.is_empty() && !cmd.is_empty() && !cmd.contains('|'),
        "malformed plugin command literal: {plugin_command}"
    );
    let cmd = cmd.replace('_', "-");
    if CORE_PLUGINS.contains(&plugin) {
        format!("core:{plugin}:allow-{cmd}")
    } else {
        format!("{plugin}:allow-{cmd}")
    }
}

/// Every quoted string (`"`, `'` or backtick) whose contents start with
/// `prefix`. A string with no closing quote on the same line is skipped.
fn quoted_with_prefix(src: &str, prefix: &str) -> Vec<String> {
    let mut found = Vec::new();
    for quote in ['"', '\'', '`'] {
        let needle = format!("{quote}{prefix}");
        let mut from = 0;
        while let Some(rel) = src[from..].find(&needle) {
            let start = from + rel + 1;
            from = start;
            let Some(len) = src[start..].find([quote, '\n']) else {
                continue;
            };
            if src[start + len..].starts_with(quote) {
                found.push(src[start..start + len].to_string());
            }
        }
    }
    found
}

/// Non-test frontend sources: `.ts`/`.tsx` under `apps/client/src`, minus
/// `*.test.*` / `*.spec.*` files and test-only directories (test mocks may
/// import anything without needing a grant).
fn frontend_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    const SKIP_DIRS: [&str; 6] = [
        "__tests__",
        "__mocks__",
        "__fixtures__",
        "fixtures",
        "test",
        "tests",
    ];
    for entry in
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
    {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name) {
                frontend_sources(&path, out);
            }
        } else if (name.ends_with(".ts") || name.ends_with(".tsx"))
            && !name.contains(".test.")
            && !name.contains(".spec.")
        {
            out.push(path);
        }
    }
}

struct Usage {
    /// `(file, "plugin:<name>|<cmd>")` for every plugin command literal.
    plugin_commands: Vec<(PathBuf, String)>,
    /// `(file, specifier)` for every `@tauri-apps/...` module reference.
    tauri_imports: Vec<(PathBuf, String)>,
}

fn frontend_usage() -> Usage {
    let src_dir = manifest_dir().join("../src");
    let mut files = Vec::new();
    frontend_sources(&src_dir, &mut files);
    assert!(
        files.len() > 10,
        "found only {} frontend sources under {}; the walk is broken",
        files.len(),
        src_dir.display()
    );
    let mut usage = Usage {
        plugin_commands: Vec::new(),
        tauri_imports: Vec::new(),
    };
    for file in files {
        let text = read(&file);
        for lit in quoted_with_prefix(&text, "plugin:") {
            usage.plugin_commands.push((file.clone(), lit));
        }
        for spec in quoted_with_prefix(&text, "@tauri-apps/") {
            usage.tauri_imports.push((file.clone(), spec));
        }
    }
    usage
}

// ---------------------------------------------------------------------------
// Static checks (every host)
// ---------------------------------------------------------------------------

#[test]
fn capabilities_dir_holds_only_default_json() {
    let dir = manifest_dir().join("capabilities");
    let entries: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    // tauri-build loads every file under capabilities/, so a second file is a
    // second grant even if nothing references it.
    assert_eq!(
        entries,
        vec!["default.json".to_string()],
        "capabilities/ must contain only default.json; {GRANT_HINT}"
    );
}

#[test]
fn tauri_conf_declares_no_inline_capabilities() {
    // `app.security.capabilities` in tauri.conf.json is another way to grant.
    let conf = read_json(&manifest_dir().join("tauri.conf.json"));
    let inline = &conf["app"]["security"]["capabilities"];
    assert!(
        inline.is_null() || inline.as_array().is_some_and(Vec::is_empty),
        "tauri.conf.json declares inline capabilities: {inline}; {GRANT_HINT}"
    );
}

#[test]
fn default_capability_is_exactly_the_least_privilege_set() {
    let cap = read_json(&manifest_dir().join("capabilities/default.json"));
    let obj = cap.as_object().expect("capability is a JSON object");

    assert_eq!(cap["identifier"], "default");
    assert_eq!(
        cap["windows"],
        serde_json::json!(["main"]),
        "the capability must target only the main window; {GRANT_HINT}"
    );
    // `remote` would expose these commands to non-local URLs; `platforms`
    // would hide a platform-specific variant of the grant.
    assert!(
        !obj.contains_key("remote"),
        "no remote access; {GRANT_HINT}"
    );
    assert!(
        !obj.contains_key("platforms"),
        "no platform split; {GRANT_HINT}"
    );
    // Anything else (`webviews`, `local`, ...) changes who gets the grant.
    for key in obj.keys() {
        assert!(
            [
                "$schema",
                "identifier",
                "description",
                "windows",
                "permissions"
            ]
            .contains(&key.as_str()),
            "unexpected capability key `{key}`; {GRANT_HINT}"
        );
    }

    let list = cap["permissions"]
        .as_array()
        .expect("permissions is an array");
    let mut granted = BTreeSet::new();
    for p in list {
        // Object form carries a scope (`{ "identifier": ..., "allow": [...] }`).
        let id = p.as_str().unwrap_or_else(|| {
            panic!("scoped/object permission entry {p} is not allowed; {GRANT_HINT}")
        });
        assert!(granted.insert(id.to_string()), "duplicate permission {id}");
    }
    assert_eq!(
        granted,
        expected(),
        "capabilities/default.json permissions drifted; {GRANT_HINT}"
    );
}

#[test]
fn every_frontend_tauri_import_is_allowlisted() {
    let allowed: Vec<&str> = ALLOWED_TAURI_IMPORTS.iter().map(|(m, _)| *m).collect();
    let usage = frontend_usage();
    for (file, spec) in &usage.tauri_imports {
        assert!(
            allowed.contains(&spec.as_str()),
            "{} imports `{spec}`, which is not in ALLOWED_TAURI_IMPORTS. A Tauri plugin's \
             JS package calls plugin commands and needs a grant; {GRANT_HINT}",
            file.display()
        );
    }
}

#[test]
fn granted_permissions_match_frontend_usage() {
    let usage = frontend_usage();
    let mut needed = BTreeSet::new();
    for (file, lit) in &usage.plugin_commands {
        assert!(
            !lit.contains("${"),
            "{} builds a plugin command name dynamically (`{lit}`); use a literal so \
             its permission can be checked",
            file.display()
        );
        needed.insert(permission_for(lit));
    }
    for (_, spec) in &usage.tauri_imports {
        if let Some((_, perms)) = ALLOWED_TAURI_IMPORTS.iter().find(|(m, _)| m == spec) {
            needed.extend(perms.iter().map(|p| p.to_string()));
        }
    }

    let granted = expected();
    let missing: Vec<_> = needed.difference(&granted).collect();
    assert!(
        missing.is_empty(),
        "the frontend uses plugin commands without a grant: {missing:?}. Prefer moving the \
         work behind a typed app command in Rust; otherwise {GRANT_HINT}"
    );
    let stale: Vec<_> = granted.difference(&needed).collect();
    assert!(
        stale.is_empty(),
        "granted but unused by any non-test frontend source: {stale:?}; remove the grant \
         from capabilities/default.json and EXPECTED_PERMISSIONS"
    );
}

#[test]
fn the_scanners_find_what_they_claim_to() {
    let ts = "import { a } from \"@tauri-apps/api/core\";\n\
              import('@tauri-apps/plugin-fs');\n\
              invoke(`plugin:fs|read_text_file`, {});\n\
              invoke(\"plugin:dialog|open\");\n\
              const s = \"plugin:unterminated\n\";";
    assert_eq!(
        quoted_with_prefix(ts, "@tauri-apps/"),
        vec!["@tauri-apps/api/core", "@tauri-apps/plugin-fs"]
    );
    assert_eq!(
        quoted_with_prefix(ts, "plugin:"),
        vec!["plugin:dialog|open", "plugin:fs|read_text_file"]
    );
    assert_eq!(permission_for("plugin:dialog|open"), "dialog:allow-open");
    assert_eq!(
        permission_for("plugin:fs|read_text_file"),
        "fs:allow-read-text-file"
    );
    assert_eq!(
        permission_for("plugin:event|listen"),
        "core:event:allow-listen"
    );
}

/// A registered plugin injects its init script whether or not it has a grant,
/// and some of those scripts call the plugin's commands on their own
/// (notification checks its permission on every page load, opener takes over
/// link clicks). Only plugins the app uses are depended on and registered:
/// log (Rust-side, no webview grant) and dialog (`dialog:allow-open`).
#[test]
fn only_used_plugins_are_registered() {
    let cargo = read(&manifest_dir().join("Cargo.toml"));
    let deps: BTreeSet<&str> = cargo
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("tauri-plugin-"))
        .filter_map(|l| l.split([' ', '=']).next())
        .collect();
    assert_eq!(
        deps,
        BTreeSet::from(["dialog", "log"]),
        "tauri-plugin-* dependencies drifted; a new plugin needs a webview or Rust \
         consumer, and {GRANT_HINT}"
    );

    let lib = read(&manifest_dir().join("src/lib.rs"));
    let registered: Vec<&str> = lib
        .match_indices(".plugin(")
        .map(|(i, m)| {
            let rest = &lib[i + m.len()..];
            rest[..rest.find(')').expect("closing paren")].trim()
        })
        .collect();
    assert_eq!(
        registered,
        vec!["log_plugin", "tauri_plugin_dialog::init("],
        "plugin registration in src/lib.rs drifted"
    );
}

// ---------------------------------------------------------------------------
// Runtime ACL (Linux and macOS)
// ---------------------------------------------------------------------------

/// Drives plugin commands through `Webview::on_message`, the same path a real
/// `invoke()` takes. The ACL check there runs before the plugin is looked up
/// (tauri 2.11.5 `src/webview/mod.rs`, the `invoke.acl.is_none()` branch
/// returns before `manager.extend_api`), so a denied command never reaches its
/// handler. As a second guard every request carries `POISON`, a body that
/// fails argument deserialisation for each command used here, so even an
/// allowed or wrongly allowed command rejects before its body runs (no native
/// file dialog, no notification, no browser).
///
/// Like `library_ipc.rs`, this binary cannot start on Windows hosts whose
/// bundled `WebView2Loader.dll` is incompatible with the installed WebView2
/// runtime (`STATUS_ENTRYPOINT_NOT_FOUND`), so it is compiled only on Linux
/// and macOS, where CI runs it.
#[cfg(not(target_os = "windows"))]
mod acl {
    use serde_json::{json, Value};
    use tauri::ipc::{CallbackFn, InvokeBody};
    use tauri::test::{get_ipc_response, MockRuntime, INVOKE_KEY};
    use tauri::webview::InvokeRequest;
    use tauri::{WebviewWindow, WebviewWindowBuilder};

    fn window() -> WebviewWindow<MockRuntime> {
        // The plugins lib.rs registers, minus log (its init installs the
        // process-global logger). Commands of plugins that are not
        // registered (fs, process, ...) are still refused by the ACL, with
        // "Plugin not found" as the detail.
        let app = tauri::test::mock_builder()
            .plugin(tauri_plugin_dialog::init())
            .invoke_handler(tauri::generate_handler![locast_client_lib::commands::greet])
            // The real config and the ACL tauri-build resolved from
            // capabilities/ (read from OUT_DIR). Assets are not needed.
            .build(tauri::generate_context!(
                "tauri.conf.json",
                assets = tauri::test::noop_assets(),
                test = true
            ))
            .expect("mock app builds from the real context");
        let window = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("mock main window builds");
        // Keep the app alive for the lifetime of the window handle.
        std::mem::forget(app);
        window
    }

    /// Wrong-typed values for every argument name the commands below take
    /// (`event`, `options`, `path`, `url`, `label`, ...), so each fails at
    /// deserialisation, including `Option` arguments such as window
    /// `close`'s `label`. Commands with no arguments (`os|platform`,
    /// `app|version`) are read-only.
    fn poison() -> Value {
        json!({
            "event": 0, "eventId": "x", "target": 0, "handler": "x",
            "options": 0, "path": 0, "paths": 0, "url": 0, "with": 0,
            "message": 0, "title": 0, "label": 0, "value": 0,
            "directory": "x", "code": "x", "payload": 0
        })
    }

    fn invoke(w: &WebviewWindow<MockRuntime>, cmd: &str, body: Value) -> Result<Value, String> {
        get_ipc_response(
            w,
            InvokeRequest {
                cmd: cmd.into(),
                callback: CallbackFn(0),
                error: CallbackFn(1),
                // The local origin on Linux and macOS (`http://tauri.localhost`
                // on Windows). Anything else is a remote origin.
                url: "tauri://localhost".parse().expect("url"),
                body: InvokeBody::Json(body),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .map(|b| b.deserialize::<Value>().expect("json response"))
        .map_err(|e| {
            e.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| e.to_string())
        })
    }

    /// The rejection text for an ACL denial. Debug builds explain it via
    /// `RuntimeAuthority::resolve_access_message` (tauri 2.11.5
    /// `src/ipc/authority.rs`: "{plugin}.{command} not allowed. ..."); release
    /// builds use "Command {cmd} not allowed by ACL" (`src/webview/mod.rs`).
    fn denial_prefix(cmd: &str) -> String {
        if cfg!(debug_assertions) {
            let (plugin, command) = cmd
                .strip_prefix("plugin:")
                .and_then(|c| c.split_once('|'))
                .expect("plugin command");
            format!("{plugin}.{command} not allowed")
        } else {
            format!("Command {cmd} not allowed by ACL")
        }
    }

    #[test]
    fn granted_plugin_commands_pass_the_acl() {
        let w = window();
        for cmd in [
            "plugin:event|listen",
            "plugin:event|unlisten",
            "plugin:dialog|open",
        ] {
            let err = invoke(&w, cmd, poison())
                .expect_err("the poison body must fail argument deserialisation");
            let name = cmd.rsplit('|').next().expect("command name");
            // Reaching argument parsing proves the ACL let the call through.
            assert!(
                !err.contains("not allowed") && err.contains("invalid args `"),
                "{cmd}: expected an argument error after the ACL passed, got: {err}"
            );
            assert!(
                err.contains(&format!("for command `{name}`")),
                "{cmd}: {err}"
            );
        }
    }

    #[test]
    fn ungranted_plugin_commands_are_denied_by_the_acl() {
        let w = window();
        for cmd in [
            "plugin:fs|read_file",
            "plugin:fs|read_text_file",
            "plugin:fs|read_dir",
            "plugin:dialog|save",
            "plugin:dialog|message",
            "plugin:notification|notify",
            "plugin:opener|open_url",
            "plugin:opener|reveal_item_in_dir",
            "plugin:os|platform",
            "plugin:process|exit",
            "plugin:process|restart",
            "plugin:path|resolve_directory",
            "plugin:app|version",
            "plugin:window|close",
            "plugin:window|set_title",
            "plugin:event|emit",
        ] {
            let err = invoke(&w, cmd, poison())
                .expect_err("an ungranted plugin command must be rejected");
            assert!(
                err.starts_with(&denial_prefix(cmd)),
                "{cmd}: expected an ACL denial, got: {err}"
            );
        }
    }

    #[test]
    fn app_commands_are_not_acl_gated() {
        // No app manifest in build.rs: typed app commands bypass the ACL.
        let w = window();
        assert_eq!(
            invoke(&w, "greet", json!({})).expect("greet"),
            json!("Hello, Locast")
        );
    }
}
