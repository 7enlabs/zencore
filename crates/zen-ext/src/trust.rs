use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const TRUST_ENV: &str = "ZEN_TRUST_FOLDER";

pub fn trust_env_set() -> bool {
    std::env::var(TRUST_ENV)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

fn home_dir(home: Option<&Path>) -> Option<PathBuf> {
    home.map(Path::to_path_buf).or_else(dirs::home_dir)
}

pub fn trust_store_path(home: Option<&Path>) -> Option<PathBuf> {
    home_dir(home).map(|h| h.join(".zen").join("trusted-folders.json"))
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| normalize_lexical(p))
}

fn normalize_lexical(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(comp.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

fn read_store(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("trusted").and_then(|t| t.as_array()).cloned())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

pub fn is_folder_trusted(cwd: &Path, home: Option<&Path>) -> bool {
    let cwd = canon(cwd);
    if let Some(h) = home_dir(home)
        && canon(&h) == cwd
    {
        return true;
    }
    let Some(store) = trust_store_path(home) else {
        return false;
    };
    read_store(&store)
        .iter()
        .any(|t| cwd.starts_with(canon(Path::new(t))))
}

pub fn trust_folder(cwd: &Path, home: Option<&Path>) -> std::io::Result<()> {
    let store = trust_store_path(home)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"))?;
    let entry = canon(cwd).to_string_lossy().into_owned();
    let mut list = read_store(&store);
    if !list.contains(&entry) {
        list.push(entry);
    }
    if let Some(dir) = store.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = store.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&json!({ "trusted": list }))? + "\n",
    )?;
    std::fs::rename(&tmp, &store)
}

pub(crate) fn gated_keys(layer: &Value) -> Vec<&'static str> {
    let mut out = Vec::new();
    let non_empty = |v: Option<&Value>| match v {
        None | Some(Value::Null) => false,
        Some(Value::Object(m)) => !m.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(_) => true,
    };
    if non_empty(layer.get("hooks")) {
        out.push("hooks");
    }
    // Only the "true" value is gated: a project layer may still set
    // disableAllHooks to false, but it must never switch all hooks off
    // by itself (issue #329). Value-conditional, like defaultMode below.
    if layer.get("disableAllHooks").and_then(Value::as_bool) == Some(true) {
        out.push("disableAllHooks");
    }
    if non_empty(layer.pointer("/permissions/allow")) {
        out.push("permissions.allow");
    }
    if non_empty(layer.pointer("/permissions/additionalDirectories")) {
        out.push("permissions.additionalDirectories");
    }
    // Only the bypassPermissions value is gated: a project layer may
    // pick any other default mode, but it must never grant itself
    // permission bypass (issue #282). Value-conditional, like
    // allowPrivateNetwork below.
    if layer
        .pointer("/permissions/defaultMode")
        .and_then(Value::as_str)
        == Some("bypassPermissions")
    {
        out.push("permissions.defaultMode");
    }
    // Only the "off" value is gated: a project layer may pick any other
    // sandbox mode, but it must never turn the sandbox off by itself
    // (issue #328). Value-conditional, like defaultMode above.
    if layer
        .pointer("/permissions/sandboxMode")
        .and_then(Value::as_str)
        == Some("off")
    {
        out.push("permissions.sandboxMode");
    }
    if non_empty(layer.get("env")) {
        out.push("env");
    }
    if non_empty(layer.get("verify")) {
        out.push("verify");
    }

    if non_empty(layer.get("baseUrl")) {
        out.push("baseUrl");
    }
    if non_empty(layer.get("apiKeyEnv")) {
        out.push("apiKeyEnv");
    }

    if layer.get("allowPrivateNetwork").and_then(Value::as_bool) == Some(true) {
        out.push("allowPrivateNetwork");
    }
    out
}

pub(crate) fn strip_gated(layer: &mut Value) -> Vec<&'static str> {
    let found = gated_keys(layer);
    if let Value::Object(m) = layer {
        m.remove("hooks");
        // Only the "true" value is stripped; a false value from a
        // project layer survives (issue #329).
        if m.get("disableAllHooks").and_then(Value::as_bool) == Some(true) {
            m.remove("disableAllHooks");
        }
        m.remove("env");
        m.remove("verify");
        m.remove("baseUrl");
        m.remove("apiKeyEnv");
        m.remove("allowPrivateNetwork");
        if let Some(Value::Object(p)) = m.get_mut("permissions") {
            p.remove("allow");
            p.remove("additionalDirectories");
            // Only the bypass value is stripped; any other default
            // mode from a project layer survives (issue #282).
            if p.get("defaultMode").and_then(Value::as_str) == Some("bypassPermissions") {
                p.remove("defaultMode");
            }
            // Only the "off" value is stripped; any other sandbox mode
            // from a project layer survives (issue #328).
            if p.get("sandboxMode").and_then(Value::as_str) == Some("off") {
                p.remove("sandboxMode");
            }
        }
    }
    found
}

pub fn project_mcp_files(cwd: &Path) -> [PathBuf; 2] {
    [cwd.join(".mcp.json"), cwd.join(".zen").join("mcp.json")]
}

pub fn gated_project_config(cwd: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for rel in [".zen/settings.json", ".zen/settings.local.json"] {
        let v = std::fs::read_to_string(cwd.join(rel))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        if let Some(v) = v {
            let keys = gated_keys(&v);
            if !keys.is_empty() {
                out.push(format!("{}: {}", rel, keys.join(", ")));
            }
        }
    }
    for (rel, path) in [".mcp.json", ".zen/mcp.json"]
        .iter()
        .zip(project_mcp_files(cwd))
    {
        if path.is_file() {
            out.push(rel.to_string());
        }
    }

    for rel in [".zen/hooks.json", ".zen/hooks.local.json"] {
        let p = cwd.join(rel);
        if p.is_file() {
            let is_hooks = std::fs::read_to_string(&p)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .map(|v| v.is_object())
                .unwrap_or(false);
            if is_hooks {
                out.push(format!("{}: hooks", rel));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs;

    #[test]
    fn store_roundtrip_and_subfolders() {
        let t = TempDir::new();
        let home = t.path().join("home");
        let proj = t.path().join("proj");
        fs::create_dir_all(proj.join("sub")).unwrap();
        fs::create_dir_all(&home).unwrap();
        assert!(!is_folder_trusted(&proj, Some(&home)));
        trust_folder(&proj, Some(&home)).unwrap();
        trust_folder(&proj, Some(&home)).unwrap();
        assert!(is_folder_trusted(&proj, Some(&home)));
        assert!(is_folder_trusted(&proj.join("sub"), Some(&home)));
        assert!(!is_folder_trusted(t.path(), Some(&home)));
        let stored = read_store(&trust_store_path(Some(&home)).unwrap());
        assert_eq!(stored.len(), 1);

        assert!(!proj.join(".zen").exists());
    }

    #[test]
    fn home_is_trusted() {
        let t = TempDir::new();
        assert!(is_folder_trusted(t.path(), Some(t.path())));
    }

    #[test]
    fn canon_is_symmetric_for_missing_paths() {
        use std::path::Path;
        assert_eq!(
            canon(Path::new("/nope/a/./b")),
            canon(Path::new("/nope/a/b"))
        );
        assert_eq!(
            canon(Path::new("/nope/a/b/")),
            canon(Path::new("/nope/a/b"))
        );
        assert_eq!(
            canon(Path::new("/nope/a/x/../b")),
            canon(Path::new("/nope/a/b"))
        );
    }

    #[test]
    fn dotdot_spelling_of_trusted_dir_is_trusted() {
        let t = TempDir::new();
        let home = t.path().join("home");
        let proj = t.path().join("proj");
        fs::create_dir_all(proj.join("sub")).unwrap();
        fs::create_dir_all(&home).unwrap();
        trust_folder(&proj, Some(&home)).unwrap();

        assert!(is_folder_trusted(&proj.join("sub").join(".."), Some(&home)));
    }

    #[test]
    fn gated_detection_and_strip() {
        let t = TempDir::new();
        let proj = t.path();
        assert!(gated_project_config(proj).is_empty());
        fs::create_dir_all(proj.join(".zen")).unwrap();
        fs::write(
            proj.join(".zen/settings.json"),
            r#"{"model":"m","permissions":{"deny":["Bash"]}}"#,
        )
        .unwrap();
        assert!(
            gated_project_config(proj).is_empty(),
            "model/deny are not gated"
        );
        fs::write(
            proj.join(".zen/settings.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"x"}]}]},"permissions":{"allow":["Bash"]}}"#,
        )
        .unwrap();
        fs::write(proj.join(".mcp.json"), r#"{"mcpServers":{}}"#).unwrap();
        assert_eq!(
            gated_project_config(proj),
            vec![
                ".zen/settings.json: hooks, permissions.allow".to_string(),
                ".mcp.json".to_string()
            ]
        );
        let mut v = json!({"hooks": {"a": 1}, "env": {"A": "1"}, "verify": {"command": "x"},
            "permissions": {"allow": ["Bash"], "deny": ["Write"], "additionalDirectories": ["/"]}, "model": "m"});
        let removed = strip_gated(&mut v);
        assert_eq!(removed.len(), 5);
        assert_eq!(v, json!({"permissions": {"deny": ["Write"]}, "model": "m"}));
    }

    #[test]
    fn gated_detection_reports_hooks_files() {
        let t = TempDir::new();
        let proj = t.path();
        fs::create_dir_all(proj.join(".zen")).unwrap();
        assert!(gated_project_config(proj).is_empty());
        fs::write(
            proj.join(".zen/hooks.json"),
            r#"{"PreToolUse": [{"matcher": "Bash", "hooks": []}]}"#,
        )
        .unwrap();
        assert_eq!(
            gated_project_config(proj),
            vec![".zen/hooks.json: hooks".to_string()]
        );

        fs::write(proj.join(".zen/hooks.json"), r#"[1,2]"#).unwrap();
        assert!(gated_project_config(proj).is_empty());
    }

    #[test]
    fn gated_default_mode_only_bypass_is_gated() {
        // Issue #282: only the bypassPermissions value is gated - a
        // project layer may pick any other default mode.
        let mut v = json!({"permissions": {"defaultMode": "bypassPermissions", "deny": ["Write"]}});
        let removed = strip_gated(&mut v);
        assert_eq!(removed, vec!["permissions.defaultMode"]);
        assert_eq!(v, json!({"permissions": {"deny": ["Write"]}}));

        let mut v = json!({"permissions": {"defaultMode": "plan"}});
        let removed = strip_gated(&mut v);
        assert!(removed.is_empty(), "removed: {removed:?}");
        assert_eq!(v, json!({"permissions": {"defaultMode": "plan"}}));

        let t = TempDir::new();
        let proj = t.path();
        fs::create_dir_all(proj.join(".zen")).unwrap();
        fs::write(
            proj.join(".zen/settings.json"),
            r#"{"permissions":{"defaultMode":"bypassPermissions"}}"#,
        )
        .unwrap();
        assert_eq!(
            gated_project_config(proj),
            vec![".zen/settings.json: permissions.defaultMode".to_string()]
        );
    }

    #[test]
    fn gated_sandbox_mode_off_is_gated() {
        // Issue #328: only the "off" value is gated - an untrusted
        // project layer may pick any other sandbox mode, but it must
        // never turn the sandbox off by itself.
        let mut v = json!({"permissions": {"sandboxMode": "off", "deny": ["Write"]}});
        let removed = strip_gated(&mut v);
        assert_eq!(removed, vec!["permissions.sandboxMode"]);
        assert_eq!(v, json!({"permissions": {"deny": ["Write"]}}));

        for mode in ["workspace-write", "read-only"] {
            let mut v = json!({"permissions": {"sandboxMode": mode}});
            let removed = strip_gated(&mut v);
            assert!(removed.is_empty(), "removed: {removed:?}");
            assert_eq!(v, json!({"permissions": {"sandboxMode": mode}}));
        }

        let t = TempDir::new();
        let proj = t.path();
        fs::create_dir_all(proj.join(".zen")).unwrap();
        fs::write(
            proj.join(".zen/settings.json"),
            r#"{"permissions":{"sandboxMode":"off"}}"#,
        )
        .unwrap();
        assert_eq!(
            gated_project_config(proj),
            vec![".zen/settings.json: permissions.sandboxMode".to_string()]
        );
    }

    #[test]
    fn gated_disable_all_hooks_is_gated() {
        // Issue #329: only the "true" value is gated - an untrusted
        // project layer must never switch all hooks off by itself.
        let mut v = json!({"disableAllHooks": true, "model": "m"});
        let removed = strip_gated(&mut v);
        assert_eq!(removed, vec!["disableAllHooks"]);
        assert_eq!(v, json!({"model": "m"}));

        let mut v = json!({"disableAllHooks": false});
        let removed = strip_gated(&mut v);
        assert!(removed.is_empty(), "removed: {removed:?}");
        assert_eq!(v, json!({"disableAllHooks": false}));

        let t = TempDir::new();
        let proj = t.path();
        fs::create_dir_all(proj.join(".zen")).unwrap();
        fs::write(
            proj.join(".zen/settings.json"),
            r#"{"disableAllHooks":true}"#,
        )
        .unwrap();
        assert_eq!(
            gated_project_config(proj),
            vec![".zen/settings.json: disableAllHooks".to_string()]
        );
    }
}
