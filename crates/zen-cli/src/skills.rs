use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zen_agent::sha256_hex;
use zen_core::lock_recovery::lock_or_recover;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustTier {
    Verified,

    Community,

    Unverified,
}

impl TrustTier {
    pub fn badge(&self) -> &'static str {
        match self {
            TrustTier::Verified => "[✓ verified]",
            TrustTier::Community => "[community]",
            TrustTier::Unverified => "[⚠ unverified]",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            TrustTier::Verified => "verified",
            TrustTier::Community => "community",
            TrustTier::Unverified => "unverified",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "verified" => Some(TrustTier::Verified),
            "community" => Some(TrustTier::Community),
            "unverified" => Some(TrustTier::Unverified),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillPermissions {
    #[serde(default)]
    pub allowed_tools: Vec<String>,

    #[serde(default)]
    pub network: bool,

    #[serde(default)]
    pub file_write: bool,

    #[serde(default)]
    pub exec: bool,

    #[serde(default = "default_file_scope")]
    pub file_scope: String,
}

fn default_file_scope() -> String {
    "project".to_string()
}

impl Default for SkillPermissions {
    fn default() -> Self {
        SkillPermissions::advisory()
    }
}

impl SkillPermissions {
    pub fn sandboxed() -> Self {
        SkillPermissions {
            allowed_tools: vec!["read".to_string(), "glob".to_string(), "grep".to_string()],
            network: false,
            file_write: false,
            exec: false,
            file_scope: "project".to_string(),
        }
    }

    /// Advisory declares no tools: with an empty allow-list there is no
    /// skill-level filter, so tool calls fall through to the normal
    /// permission checker instead of being denied by the skill policy.
    pub fn advisory() -> Self {
        SkillPermissions {
            allowed_tools: vec![],
            network: false,
            file_write: false,
            exec: false,
            file_scope: "project".to_string(),
        }
    }

    pub fn describe(&self) -> Vec<String> {
        let mut lines = vec![];
        if self.allowed_tools.is_empty() {
            lines.push("tools: none (advisory only)".to_string());
        } else {
            lines.push(format!("tools: {}", self.allowed_tools.join(", ")));
        }
        lines.push(format!("network: {}", yn(self.network)));
        lines.push(format!("file_write: {}", yn(self.file_write)));
        lines.push(format!("exec: {}", yn(self.exec)));
        lines.push(format!("file_scope: {}", self.file_scope));
        lines
    }
}

fn yn(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub permissions: SkillPermissions,

    #[serde(default = "default_skills_list")]
    pub skills: Vec<String>,

    #[serde(default)]
    pub signature: Option<String>,
}

fn default_skills_list() -> Vec<String> {
    vec![".".to_string()]
}

impl PluginManifest {
    pub fn validate(&self) -> Vec<String> {
        let mut problems = vec![];
        if self.name.trim().is_empty() {
            problems.push("name is empty".to_string());
        } else if !self
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            problems.push(format!("name {:?} must be [a-zA-Z0-9_-]+", self.name));
        }
        if self.version.trim().is_empty() {
            problems.push("version is empty".to_string());
        }
        if self.description.trim().is_empty() {
            problems.push("description is empty".to_string());
        }
        if !["project", "user"].contains(&self.permissions.file_scope.as_str()) {
            problems.push(format!(
                "permissions.file_scope {:?} must be \"project\" or \"user\"",
                self.permissions.file_scope
            ));
        }

        if self.permissions.exec
            && !self
                .permissions
                .allowed_tools
                .iter()
                .any(|t| t.eq_ignore_ascii_case("bash"))
        {
            problems
                .push("permissions.exec is true but \"bash\" is not in allowed_tools".to_string());
        }
        if self.permissions.file_write
            && !self.permissions.allowed_tools.iter().any(|t| {
                matches!(
                    t.to_ascii_lowercase().as_str(),
                    "write" | "edit" | "multiedit" | "bash"
                )
            })
        {
            problems.push(
                "permissions.file_write is true but no write-capable tool in allowed_tools"
                    .to_string(),
            );
        }
        problems
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut tools = self.permissions.allowed_tools.clone();
        tools.sort();
        tools.dedup();
        let mut skills = self.skills.clone();
        skills.sort();
        skills.dedup();

        let canonical = serde_json::json!({
            "author": self.author,
            "description": self.description,
            "name": self.name,
            "permissions": {
                "allowed_tools": tools,
                "exec": self.permissions.exec,
                "file_scope": self.permissions.file_scope,
                "file_write": self.permissions.file_write,
                "network": self.permissions.network,
            },
            "skills": skills,
            "version": self.version,
        });
        serde_json::to_vec(&canonical).expect("canonical JSON serialization cannot fail")
    }
}

pub fn load_manifest(dir: &Path) -> Result<PluginManifest> {
    let path = dir.join("zen-plugin.json");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: PluginManifest =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let problems = manifest.validate();
    if !problems.is_empty() {
        return Err(anyhow!(
            "invalid manifest {}: {}",
            path.display(),
            problems.join("; ")
        ));
    }
    Ok(manifest)
}

pub const ZENCORE_SIGNING_PUBKEY_HEX: &str =
    "622c8cfc675dc43143a47fe69d6046e0411ba659aa647a9bfe1bea905db41bd5";

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    // Fail closed: hex is ASCII-only, and slicing `s` by bytes below is only
    // sound on a char boundary. Reject non-ASCII input up front instead of
    // panicking when a multi-byte character lands on an odd byte offset.
    if !s.is_ascii() {
        return Err(anyhow!("non-ASCII hex string"));
    }
    if !s.len().is_multiple_of(2) {
        return Err(anyhow!("odd-length hex string"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|e| anyhow!("invalid hex at offset {}: {}", i, e))
        })
        .collect()
}

/// Fail closed: any malformed input (bad hex, wrong key/signature length,
/// unparseable key) returns false instead of panicking, so a corrupt
/// registry entry can never verify as trusted.
pub fn verify_signature(message: &[u8], signature_hex: &str) -> bool {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let (Ok(pubkey_bytes), Ok(sig_bytes)) = (
        hex_decode(ZENCORE_SIGNING_PUBKEY_HEX),
        hex_decode(signature_hex),
    ) else {
        return false;
    };
    if pubkey_bytes.len() != 32 || sig_bytes.len() != 64 {
        return false;
    }
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&pubkey_bytes);
    let mut sb = [0u8; 64];
    sb.copy_from_slice(&sig_bytes);
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else {
        return false;
    };

    let sig = Signature::from_bytes(&sb);
    vk.verify(message, &sig).is_ok()
}

/// Trust ladder: builtins are verified by construction; a fetched skill is
/// verified only with a valid Zen signature over its canonical manifest
/// bytes. An unsigned manifest is community, and no manifest at all is
/// unverified - a missing signature is never treated as verified.
pub fn assign_tier(source: &InstallSource, manifest: Option<&PluginManifest>) -> TrustTier {
    match source {
        InstallSource::Builtin => TrustTier::Verified,
        InstallSource::Git(_) | InstallSource::Local(_) => match manifest {
            Some(m) => match &m.signature {
                Some(sig) if verify_signature(&m.canonical_bytes(), sig) => TrustTier::Verified,
                _ => TrustTier::Community,
            },
            None => TrustTier::Unverified,
        },
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    pub version: String,
    pub tier: TrustTier,
    pub source: String,
    #[serde(default)]
    pub commit: Option<String>,
    pub installed_at: String,

    #[serde(default = "default_pinned_hash")]
    pub pinned_hash: String,
}

fn default_pinned_hash() -> String {
    "none".to_string()
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub skills: BTreeMap<String, RegistryEntry>,
}

pub fn skills_root(home: &Path) -> PathBuf {
    home.join(".zen").join("skills")
}

pub fn registry_path(home: &Path) -> PathBuf {
    skills_root(home).join("registry.json")
}

/// Load the skill registry. A missing file means a fresh home and
/// loads as empty; a present-but-unreadable or unparsable file is an
/// error, never an empty registry - the old code silently returned
/// `Registry::default()` on any failure, so a registry truncated by a
/// crash mid-write made the next install unregister every skill
/// (issue #465).
pub fn load_registry(home: &Path) -> Result<Registry> {
    let path = registry_path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Registry::default());
        }
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", path.display()));
        }
    };
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn save_registry(home: &Path, registry: &Registry) -> Result<()> {
    let root = skills_root(home);
    std::fs::create_dir_all(&root)?;
    let path = registry_path(home);
    let text = serde_json::to_string_pretty(registry)?;
    // Write a sibling temp file and rename it over the live path - the
    // atomic pattern save_local_settings in repl.rs uses. A crash
    // mid-write can no longer leave registry.json torn, which the old
    // truncating fs::write did (issue #465). The temp name is unique
    // per write (pid + sequence) so concurrent writers never share one.
    // A failed write or rename removes the temp file and leaves the
    // previous registry untouched.
    static SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = root.join(format!("registry.json.{}-{}.tmp", std::process::id(), seq));
    if let Err(e) =
        std::fs::write(&tmp, text.as_bytes()).and_then(|()| std::fs::rename(&tmp, &path))
    {
        std::fs::remove_file(&tmp).ok();
        return Err(e).with_context(|| format!("writing {}", path.display()));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum InstallSource {
    Builtin,
    Git(String),
    Local(PathBuf),
}

impl InstallSource {
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(name) = s.strip_prefix("builtin:") {
            if name.trim().is_empty() {
                return Err(anyhow!("builtin: requires a skill name"));
            }
            return Ok(InstallSource::Builtin);
        }
        if s.starts_with("http://") || s.starts_with("https://") || s.starts_with("git@") {
            return Ok(InstallSource::Git(s.to_string()));
        }
        let p = PathBuf::from(s);
        if p.exists() {
            return Ok(InstallSource::Local(p));
        }
        Err(anyhow!(
            "cannot resolve skill source {:?}: not a builtin:, git URL, or existing path",
            s
        ))
    }

    pub fn describe(&self) -> String {
        match self {
            InstallSource::Builtin => "builtin".to_string(),
            InstallSource::Git(u) => format!("git:{}", u),
            InstallSource::Local(p) => format!("local:{}", p.display()),
        }
    }
}

pub fn permission_diff(old: Option<&PluginManifest>, new: &PluginManifest) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Skill: {} v{} by {}\n",
        new.name,
        new.version,
        if new.author.is_empty() {
            "(unknown author)"
        } else {
            &new.author
        }
    ));
    out.push_str(&format!("  {}\n", new.description));
    let new_lines = new.permissions.describe();
    match old {
        None => {
            out.push_str("Permissions requested:\n");
            for l in &new_lines {
                out.push_str(&format!("  + {}\n", l));
            }
        }
        Some(o) => {
            out.push_str(&format!(
                "Permission changes ({} → {}):\n",
                o.version, new.version
            ));
            let old_lines = o.permissions.describe();
            let mut changed = false;
            for (a, b) in old_lines.iter().zip(new_lines.iter()) {
                if a != b {
                    out.push_str(&format!("  - {}\n  + {}\n", a, b));
                    changed = true;
                }
            }
            if !changed {
                out.push_str("  (no permission changes)\n");
            }
        }
    }
    out
}

pub fn install_skill(home: &Path, source: &InstallSource, yes: bool) -> Result<RegistryEntry> {
    if !yes {
        return Err(crate::coded_error(
            crate::exit_code::FORBIDDEN,
            "refusing to install without explicit approval: re-run with --yes after reviewing permissions",
        ));
    }
    if matches!(source, InstallSource::Builtin) {
        return Err(anyhow!(
            "internal: use install_builtin(name) for builtin sources"
        ));
    }
    // One staging dir per call, not one per process: install tests (and
    // two concurrent CLI installs) share a process, so a pid-only name
    // lets one install's manifest/SKILL.md leak into another's staging
    // (wrong trust tier, missing-SKILL.md installs that do not fail) and
    // makes concurrent remove/copy races fail, reliably on Windows.
    let staging = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = STAGING_COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "zen-skill-install-{}-{}-{}",
            std::process::id(),
            nanos,
            n
        ))
    };
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    let mut name: String;
    let commit: Option<String>;
    let src_desc = source.describe();

    match source {
        InstallSource::Builtin => {
            return Err(anyhow!(
                "internal: use install_builtin(name) for builtin sources"
            ));
        }
        InstallSource::Git(url) => {
            let (n, c) = clone_skill_repo(url, &staging)?;
            name = n;
            commit = c;
        }
        InstallSource::Local(p) => {
            let (n, c) = copy_skill_dir(p, &staging)?;
            name = n;
            commit = c;
        }
    }

    let manifest = load_manifest(&staging).ok();
    if let Some(m) = &manifest {
        if m.name.trim().is_empty() {
            std::fs::remove_dir_all(&staging).ok();
            return Err(anyhow!("manifest name is empty"));
        }
        name = m.name.clone();
    } else if let Ok(skill_md) = std::fs::read_to_string(staging.join("SKILL.md"))
        && let (Some(fm), _) = zen_ext::split_frontmatter(&skill_md)
        && let Some(n) = fm.name.filter(|n| !n.trim().is_empty())
    {
        name = n;
    }

    // The resolved name becomes a path component below
    // (skills_root(name)), so it must be a single safe segment. A
    // manifest name is already validated by load_manifest, but the
    // SKILL.md frontmatter name and the source-derived fallback are not:
    // a name such as "../../evil" would escape ~/.zen/skills and the
    // remove_dir_all/copy_dir_all below would delete and overwrite an
    // arbitrary directory tree (issue #371). Reject here, whatever the
    // name's source, before the name is used as a path anywhere.
    if let Err(e) = validate_skill_name(&name) {
        std::fs::remove_dir_all(&staging).ok();
        return Err(e);
    }

    if !staging.join("SKILL.md").is_file() {
        std::fs::remove_dir_all(&staging).ok();
        return Err(anyhow!("skill {:?} has no SKILL.md", name));
    }

    let tier = assign_tier(source, manifest.as_ref());
    let mut registry = load_registry(home)?;
    let old_manifest = registry.skills.get(&name).and_then(|_| manifest.clone());

    if manifest.is_none() {
        eprintln!(
            "warning: skill {:?} ships no zen-plugin.json → installing as UNVERIFIED (sandboxed: read-only tools, no network)",
            name
        );
    }

    let dest = skills_root(home).join(&name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::create_dir_all(skills_root(home))?;
    copy_dir_all(&staging, &dest)?;
    std::fs::remove_dir_all(&staging).ok();

    let pinned_hash = manifest
        .as_ref()
        .map(|_| {
            let bytes = std::fs::read(dest.join("zen-plugin.json")).unwrap_or_default();
            sha256_hex(&bytes)
        })
        .unwrap_or_else(|| "none".to_string());
    let _ = old_manifest;

    let entry = RegistryEntry {
        name: name.clone(),
        version: manifest
            .as_ref()
            .map(|m| m.version.clone())
            .unwrap_or_else(|| "0.0.0".to_string()),
        tier,
        source: src_desc,
        commit,
        installed_at: chrono::Utc::now().to_rfc3339(),
        pinned_hash,
    };
    registry.skills.insert(name.clone(), entry.clone());
    save_registry(home, &registry)?;
    Ok(entry)
}

pub const BUILTIN_SKILLS: &[(&str, &str, &str, &str)] = &[
    (
        "tdd",
        include_str!("../../../skills/tdd/SKILL.md"),
        include_str!("../../../skills/tdd/zen-plugin.json"),
        include_str!("../../../skills/tdd/test.md"),
    ),
    (
        "token-diet",
        include_str!("../../../skills/token-diet/SKILL.md"),
        include_str!("../../../skills/token-diet/zen-plugin.json"),
        include_str!("../../../skills/token-diet/test.md"),
    ),
    (
        "security-review",
        include_str!("../../../skills/security-review/SKILL.md"),
        include_str!("../../../skills/security-review/zen-plugin.json"),
        include_str!("../../../skills/security-review/test.md"),
    ),
];

pub fn builtin_skill_names() -> Vec<&'static str> {
    BUILTIN_SKILLS.iter().map(|(n, _, _, _)| *n).collect()
}

/// Parse, validate and verify the embedded manifest of a builtin skill
/// without writing anything. Shared by install_builtin and the --dry-run
/// preview in cmd_install (issue #281).
fn builtin_manifest(name: &str) -> Result<PluginManifest> {
    let (_, _, manifest_json, _) = BUILTIN_SKILLS
        .iter()
        .find(|(n, _, _, _)| *n == name)
        .ok_or_else(|| {
            anyhow!(
                "no builtin skill named {:?} (available: {})",
                name,
                builtin_skill_names().join(", ")
            )
        })?;

    let manifest: PluginManifest = serde_json::from_str(manifest_json)
        .with_context(|| format!("parsing embedded manifest for builtin skill {:?}", name))?;
    let problems = manifest.validate();
    if !problems.is_empty() {
        return Err(anyhow!(
            "embedded manifest for {:?} is invalid: {}",
            name,
            problems.join("; ")
        ));
    }

    match &manifest.signature {
        Some(sig) if verify_signature(&manifest.canonical_bytes(), sig) => {}
        _ => {
            return Err(anyhow!(
                "embedded manifest for {:?} has no valid Zen signature",
                name
            ));
        }
    }
    Ok(manifest)
}

pub fn install_builtin(home: &Path, name: &str) -> Result<RegistryEntry> {
    let (_, skill_md, manifest_json, test_md) = BUILTIN_SKILLS
        .iter()
        .find(|(n, _, _, _)| *n == name)
        .ok_or_else(|| {
            anyhow!(
                "no builtin skill named {:?} (available: {})",
                name,
                builtin_skill_names().join(", ")
            )
        })?;

    let manifest = builtin_manifest(name)?;

    let dest = skills_root(home).join(name);
    if dest.exists() {
        return Err(crate::coded_error(
            crate::exit_code::CONFLICT,
            format!(
                "skill {:?} is already installed at {}",
                name,
                dest.display()
            ),
        ));
    }
    std::fs::create_dir_all(&dest)?;
    std::fs::write(dest.join("SKILL.md"), skill_md)?;
    std::fs::write(dest.join("zen-plugin.json"), manifest_json)?;
    std::fs::write(dest.join("test.md"), test_md)?;

    let mut registry = load_registry(home)?;
    let entry = RegistryEntry {
        name: name.to_string(),
        version: manifest.version.clone(),
        tier: TrustTier::Verified,
        source: "builtin".to_string(),
        commit: None,
        installed_at: chrono::Utc::now().to_rfc3339(),
        pinned_hash: "builtin".to_string(),
    };
    registry.skills.insert(name.to_string(), entry.clone());
    save_registry(home, &registry)?;
    Ok(entry)
}

pub fn remove_skill(home: &Path, name: &str) -> Result<()> {
    let mut registry = load_registry(home)?;
    if !registry.skills.contains_key(name) {
        return Err(anyhow!("no installed skill named {:?}", name));
    }
    let dest = skills_root(home).join(name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    registry.skills.remove(name);
    save_registry(home, &registry)?;
    Ok(())
}

/// Remove an installed skill, honouring `--yes` like `sessions delete`:
/// without it, a non-interactive stdin refuses with FORBIDDEN (5) and an
/// interactive terminal is asked [y/N]; declining cancels the removal.
/// Returns true when the skill was removed.
fn cmd_remove(home: &Path, name: &str, yes: bool) -> Result<bool> {
    if !yes && !confirm_remove_prompt(name)? {
        return Ok(false);
    }
    remove_skill(home, name)?;
    Ok(true)
}

/// Confirmation prompt for `skill remove`, mirroring
/// `session_mgmt::confirm_delete_prompt`.
fn confirm_remove_prompt(name: &str) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return Err(crate::coded_error(
            crate::exit_code::FORBIDDEN,
            "refusing to remove skill without --yes on non-interactive stdin",
        ));
    }
    eprint!(
        "Permanently remove skill {:?}? This cannot be undone. [y/N] ",
        name
    );
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(crate::session_mgmt::parse_confirmation_answer(&answer))
}

fn clone_skill_repo(url: &str, dest: &Path) -> Result<(String, Option<String>)> {
    let out = std::process::Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--quiet",
            url,
            &dest.to_string_lossy(),
        ])
        .output()
        .context("running git clone")?;
    if !out.status.success() {
        return Err(anyhow!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let commit = std::process::Command::new("git")
        .args(["-C", &dest.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    if dest.join("SKILL.md").is_file() {
        let name = repo_name_from_url(url)?;
        return Ok((name, commit));
    }
    let mut candidates = vec![];
    if let Ok(entries) = std::fs::read_dir(dest) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir()
                && !p.file_name().map(|n| n == ".git").unwrap_or(false)
                && p.join("SKILL.md").is_file()
            {
                candidates.push(p);
            }
        }
    }
    if candidates.len() == 1 {
        // Guarded by the length check above.
        let skill_dir = candidates
            .pop()
            .expect("candidates holds exactly one entry");
        let name = skill_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "skill".to_string());

        let tmp = dest.with_extension("tmp-skill");
        std::fs::rename(&skill_dir, &tmp)?;
        std::fs::remove_dir_all(dest)?;
        std::fs::rename(&tmp, dest)?;
        return Ok((name, commit));
    }
    Err(anyhow!(
        "repo does not look like a skill: expected SKILL.md at root or in exactly one top-level dir"
    ))
}

fn repo_name_from_url(url: &str) -> Result<String> {
    let tail = url.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let tail = tail.strip_suffix(".git").unwrap_or(tail);
    let tail = tail.split('@').next_back().unwrap_or(tail);
    if tail.is_empty() {
        return Err(anyhow!("cannot derive skill name from URL {:?}", url));
    }
    Ok(tail.to_string())
}

fn copy_skill_dir(src: &Path, dest: &Path) -> Result<(String, Option<String>)> {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow!("cannot derive skill name from path {}", src.display()))?;

    copy_dir_all(src, dest)?;
    Ok((name, None))
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            copy_dir_all(&entry.path(), &dst_path)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckResult {
    pub check: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CheckOp {
    Eq,
    Ne,
    Contains,
    NotContains,
}

struct ParsedCheck {
    target: String,
    op: CheckOp,
    value: String,
}

fn parse_check_line(line: &str) -> Option<ParsedCheck> {
    let line = line.trim().strip_prefix("- CHECK")?.trim();

    let (target, op, value) = if let Some(i) = line.find("not contains") {
        (
            &line[..i],
            CheckOp::NotContains,
            &line[i + "not contains".len()..],
        )
    } else if let Some(i) = line.find("contains") {
        (&line[..i], CheckOp::Contains, &line[i + "contains".len()..])
    } else if let Some(i) = line.find("==") {
        (&line[..i], CheckOp::Eq, &line[i + 2..])
    } else {
        let i = line.find("!=")?;
        (&line[..i], CheckOp::Ne, &line[i + 2..])
    };
    let target = target.trim().to_string();
    let value = unquote_check(value.trim());
    if target.is_empty() || value.is_empty() {
        return None;
    }
    Some(ParsedCheck { target, op, value })
}

fn unquote_check(s: &str) -> String {
    let t = s.trim();
    for (open, close) in [('"', '"'), ('\'', '\''), ('`', '`')] {
        if t.starts_with(open) && t.ends_with(close) && t.len() >= 2 {
            return t[1..t.len() - 1].to_string();
        }
    }
    t.to_string()
}

fn resolve_target(
    target: &str,
    frontmatter: &BTreeMap<String, String>,
    manifest: Option<&PluginManifest>,
    instructions: &str,
) -> Option<(String, bool)> {
    if let Some(rest) = target.strip_prefix("frontmatter.") {
        return frontmatter.get(rest).map(|v| (v.clone(), false));
    }
    if target == "instructions" {
        return Some((instructions.to_string(), false));
    }
    let m = manifest?;
    match target {
        "manifest.name" => Some((m.name.clone(), false)),
        "manifest.version" => Some((m.version.clone(), false)),
        "manifest.description" => Some((m.description.clone(), false)),
        "manifest.author" => Some((m.author.clone(), false)),
        "manifest.permissions.network" => Some((m.permissions.network.to_string(), false)),
        "manifest.permissions.file_write" => Some((m.permissions.file_write.to_string(), false)),
        "manifest.permissions.exec" => Some((m.permissions.exec.to_string(), false)),
        "manifest.permissions.file_scope" => Some((m.permissions.file_scope.clone(), false)),
        "manifest.permissions.allowed_tools" => {
            Some((m.permissions.allowed_tools.join(", "), true))
        }
        _ => None,
    }
}

fn eval_check(
    pc: &ParsedCheck,
    frontmatter: &BTreeMap<String, String>,
    manifest: Option<&PluginManifest>,
    instructions: &str,
) -> CheckResult {
    let label = format!("CHECK {} {} {}", pc.target, op_str(pc.op), pc.value);
    let Some((actual, is_list)) = resolve_target(&pc.target, frontmatter, manifest, instructions)
    else {
        return CheckResult {
            check: label,
            passed: false,
            detail: format!("unknown target {:?}", pc.target),
        };
    };
    if is_list && matches!(pc.op, CheckOp::Eq | CheckOp::Ne) {
        return CheckResult {
            check: label,
            passed: false,
            detail: "== / != not supported on list targets; use contains".to_string(),
        };
    }
    let passed = match pc.op {
        CheckOp::Eq => actual == pc.value,
        CheckOp::Ne => actual != pc.value,
        CheckOp::Contains => {
            if is_list {
                actual.split(", ").any(|t| t == pc.value)
            } else {
                actual.contains(&pc.value)
            }
        }
        CheckOp::NotContains => {
            if is_list {
                !actual.split(", ").any(|t| t == pc.value)
            } else {
                !actual.contains(&pc.value)
            }
        }
    };
    CheckResult {
        check: label,
        passed,
        detail: if passed {
            "ok".to_string()
        } else {
            format!("actual: {:?}", truncate(&actual, 120))
        },
    }
}

fn op_str(op: CheckOp) -> &'static str {
    match op {
        CheckOp::Eq => "==",
        CheckOp::Ne => "!=",
        CheckOp::Contains => "contains",
        CheckOp::NotContains => "not contains",
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

pub fn run_skill_tests(skill_dir: &Path) -> Result<Vec<CheckResult>> {
    let skill_md = std::fs::read_to_string(skill_dir.join("SKILL.md"))
        .with_context(|| format!("reading {}", skill_dir.join("SKILL.md").display()))?;
    let (fm_opt, instructions) = zen_ext::split_frontmatter(&skill_md);
    let mut frontmatter = BTreeMap::new();
    if let Some(fm) = fm_opt {
        if let Some(n) = fm.name {
            frontmatter.insert("name".to_string(), n);
        }
        if let Some(d) = fm.description {
            frontmatter.insert("description".to_string(), d);
        }
        for (k, v) in fm.extra {
            frontmatter.insert(k, v);
        }
        if !fm.allowed_tools.is_empty() {
            frontmatter.insert("allowed_tools".to_string(), fm.allowed_tools.join(", "));
        }
    }
    let manifest = load_manifest(skill_dir).ok();

    let test_path = skill_dir.join("test.md");
    if !test_path.is_file() {
        return Err(anyhow!(
            "skill at {} ships no test.md - cannot evaluate",
            skill_dir.display()
        ));
    }
    let test_md = std::fs::read_to_string(&test_path)?;
    let mut results = vec![];
    for line in test_md.lines() {
        if let Some(pc) = parse_check_line(line) {
            results.push(eval_check(
                &pc,
                &frontmatter,
                manifest.as_ref(),
                &instructions,
            ));
        }
    }
    if results.is_empty() {
        return Err(anyhow!("test.md contains no CHECK lines"));
    }
    Ok(results)
}

const SCAFFOLD_SKILL_MD: &str = r#"---
name: {name}
description: TODO - one line describing what this skill does
allowed-tools: read
---

# {Name}

TODO - write the skill instructions here. Keep them short and specific:
state when the skill applies, the exact steps to follow, and what done looks like.
"#;

const SCAFFOLD_MANIFEST: &str = r#"{
  "name": "{name}",
  "version": "0.1.0",
  "description": "TODO - one line describing what this skill does",
  "author": "",
  "permissions": {
    "allowed_tools": ["read"],
    "network": false,
    "file_write": false,
    "exec": false,
    "file_scope": "project"
  },
  "skills": ["."]
}
"#;

const SCAFFOLD_TEST_MD: &str = r#"# Tests for {name}

## checks
- CHECK frontmatter.name == "{name}"
- CHECK manifest.permissions.network == false
- CHECK instructions contains "TODO"
"#;

/// Validate a skill name without writing anything. Shared by
/// create_skill and the --dry-run preview in skill_command (issue #281).
fn validate_skill_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(crate::coded_error(
            crate::exit_code::VALIDATION,
            "skill name must be [a-zA-Z0-9_-]+",
        ));
    }
    Ok(())
}

pub fn create_skill(home: &Path, name: &str) -> Result<PathBuf> {
    validate_skill_name(name)?;
    let dir = skills_root(home).join(name);
    if dir.exists() {
        return Err(crate::coded_error(
            crate::exit_code::CONFLICT,
            format!("skill {:?} already exists at {}", name, dir.display()),
        ));
    }
    std::fs::create_dir_all(&dir)?;
    let title = {
        let mut c = name.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => String::new(),
        }
    };
    std::fs::write(
        dir.join("SKILL.md"),
        SCAFFOLD_SKILL_MD
            .replace("{name}", name)
            .replace("{Name}", &title),
    )?;
    std::fs::write(
        dir.join("zen-plugin.json"),
        SCAFFOLD_MANIFEST.replace("{name}", name),
    )?;
    std::fs::write(
        dir.join("test.md"),
        SCAFFOLD_TEST_MD.replace("{name}", name),
    )?;

    let mut registry = load_registry(home)?;
    registry.skills.insert(
        name.to_string(),
        RegistryEntry {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            tier: TrustTier::Unverified,
            source: "local".to_string(),
            commit: None,
            installed_at: chrono::Utc::now().to_rfc3339(),
            pinned_hash: "none".to_string(),
        },
    );
    save_registry(home, &registry)?;
    Ok(dir)
}

#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub tier: TrustTier,
    pub permissions: SkillPermissions,
}

impl LoadedSkill {
    /// Unverified skills are forced into the sandbox regardless of what
    /// their manifest declares: an untrusted skill cannot grant itself
    /// tools, network, or file writes by claiming them.
    pub fn effective_permissions(&self) -> SkillPermissions {
        match self.tier {
            TrustTier::Unverified => SkillPermissions::sandboxed(),
            TrustTier::Verified | TrustTier::Community => self.permissions.clone(),
        }
    }
}

pub fn load_enabled_skills(home: &Path) -> Vec<LoadedSkill> {
    // Read-only hot path (agent run): it cannot return the load error,
    // so a corrupt registry warns loudly and loads no skills instead
    // of silently running skill-less. Nothing is written here, so the
    // corrupt file is left intact for repair.
    let registry = match load_registry(home) {
        Ok(registry) => registry,
        Err(e) => {
            eprintln!("warning: cannot read skill registry, no skills loaded: {e}");
            return vec![];
        }
    };
    let mut out = vec![];
    for (name, entry) in &registry.skills {
        let dir = skills_root(home).join(name);
        let skill_md = match std::fs::read_to_string(dir.join("SKILL.md")) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let (fm_opt, body) = zen_ext::split_frontmatter(&skill_md);
        let description = fm_opt
            .as_ref()
            .and_then(|f| f.description.clone())
            .unwrap_or_default();
        let permissions = load_manifest(&dir)
            .map(|m| m.permissions)
            // A skill whose manifest cannot be read gets sandboxed
            // permissions, not its declared ones: fail closed on load errors.
            .unwrap_or_else(|_| SkillPermissions::sandboxed());
        out.push(LoadedSkill {
            name: name.clone(),
            description,
            body,
            tier: entry.tier,
            permissions,
        });
    }
    out
}

pub fn skills_system_prompt(skills: &[LoadedSkill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from("Installed skills (load with the Skill tool):\n");
    for s in skills {
        out.push_str(&format!(
            "- {} {}: {}\n",
            s.name,
            s.tier.badge(),
            s.description
        ));
    }
    out.trim_end().to_string()
}

pub fn allowed_tool_names(skill: &LoadedSkill) -> Option<Vec<String>> {
    let perms = skill.effective_permissions();
    if perms.allowed_tools.is_empty() {
        return None;
    }
    let mut names: Vec<String> = perms
        .allowed_tools
        .iter()
        .map(|t| canonical_tool_name(t))
        .collect();

    names.push("Skill".to_string());
    names.sort();
    names.dedup();
    Some(names)
}

fn canonical_tool_name(t: &str) -> String {
    match t.to_ascii_lowercase().as_str() {
        "read" => "Read".to_string(),
        "write" => "Write".to_string(),
        "edit" => "Edit".to_string(),
        "multiedit" => "MultiEdit".to_string(),
        "bash" => "Bash".to_string(),
        "glob" => "Glob".to_string(),
        "grep" => "Grep".to_string(),
        "webfetch" | "web_fetch" => "WebFetch".to_string(),
        "websearch" | "web_search" => "WebSearch".to_string(),
        "todowrite" | "todo_write" => "TodoWrite".to_string(),
        "skill" => "Skill".to_string(),
        other => {
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}

/// Print a JSON payload the way List does: compact with --raw,
/// pretty otherwise.
fn print_json_value(cli: &crate::cli::Cli, v: &serde_json::Value) -> Result<()> {
    if cli.raw {
        println!("{}", serde_json::to_string(v)?);
    } else {
        println!("{}", serde_json::to_string_pretty(v)?);
    }
    Ok(())
}

pub async fn skill_command(
    cli: &crate::cli::Cli,
    cwd: &Path,
    home: Option<&Path>,
    cmd: &crate::cli::SkillSubcommand,
) -> Result<i32> {
    let home = home
        .map(|p| p.to_path_buf())
        .or_else(dirs::home_dir)
        .ok_or_else(|| anyhow!("cannot determine home directory"))?;
    match cmd {
        crate::cli::SkillSubcommand::List => {
            if cli.json_output || cli.raw {
                if !cli.quiet {
                    let registry = load_registry(&home)?;
                    if cli.raw {
                        println!("{}", serde_json::to_string(&registry)?);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&registry)?);
                    }
                }
            } else if !cli.quiet {
                cmd_list(cli, &home, cwd)?;
            }
            Ok(0)
        }
        crate::cli::SkillSubcommand::Install {
            source,
            yes,
            registry,
            dry_run,
        } => {
            cmd_install(
                &home,
                source,
                *yes,
                registry.as_deref(),
                *dry_run,
                cli.quiet,
            )?;
            Ok(0)
        }
        crate::cli::SkillSubcommand::Search {
            query,
            registry,
            curated,
        } => {
            // --quiet suppresses stdout only (M1, same family as
            // D1): the search still runs and its errors still
            // propagate - a missing --registry file or an invalid
            // curated entry must fail identically with --quiet.
            if *curated {
                if cli.json_output || cli.raw {
                    let hits = crate::curation::search_hits(query, true, Some(&home))?;
                    if cli.quiet {
                        return Ok(0);
                    }
                    let results: Vec<serde_json::Value> = hits
                        .iter()
                        .map(|h| {
                            serde_json::json!({
                                "name": h.name,
                                "description": h.description,
                                "version": h.version,
                                "author": h.author,
                                "tier": h.tier,
                                "review_summary": h.review_summary,
                                "score": h.score,
                            })
                        })
                        .collect();
                    print_json_value(
                        cli,
                        &serde_json::json!({ "query": query, "results": results }),
                    )?;
                    Ok(0)
                } else if cli.quiet {
                    crate::curation::search_hits(query, true, Some(&home))?;
                    Ok(0)
                } else {
                    crate::curation::run_skill_search(query, true, Some(&home))
                }
            } else if cli.json_output || cli.raw {
                let v = crate::skill_registry::search_json(query, registry.as_deref())?;
                if !cli.quiet {
                    print_json_value(cli, &v)?;
                }
                Ok(0)
            } else if cli.quiet {
                crate::skill_registry::search_json(query, registry.as_deref())?;
                Ok(0)
            } else {
                crate::skill_registry::cmd_search(query, registry.as_deref())?;
                Ok(0)
            }
        }
        crate::cli::SkillSubcommand::Info { name, registry } => {
            // --quiet suppresses stdout only (M1, same family as
            // D1): the lookup still runs, so an unknown skill name
            // errors identically with --quiet. info_json shares
            // cmd_info's lookup and not-found error.
            if cli.json_output || cli.raw {
                let v = crate::skill_registry::info_json(name, registry.as_deref())?;
                if !cli.quiet {
                    print_json_value(cli, &v)?;
                }
            } else if cli.quiet {
                crate::skill_registry::info_json(name, registry.as_deref())?;
            } else {
                crate::skill_registry::cmd_info(name, registry.as_deref())?;
            }
            Ok(0)
        }
        crate::cli::SkillSubcommand::Remove { name, yes, dry_run } => {
            if *dry_run {
                let registry = load_registry(&home)?;
                let entry = registry
                    .skills
                    .get(name)
                    .ok_or_else(|| anyhow!("no installed skill named {:?}", name))?;
                if !cli.quiet {
                    let dest = skills_root(&home).join(name);
                    let files = std::fs::read_dir(&dest).map(|d| d.count()).unwrap_or(0);
                    println!(
                        "would remove skill {} v{} from {}",
                        entry.name,
                        entry.version,
                        dest.display()
                    );
                    println!(
                        "  registry entry: {} v{} (installed {})",
                        entry.name, entry.version, entry.installed_at
                    );
                    println!("  {files} file(s) in {}", dest.display());
                    println!("(no change made; drop --dry-run to apply)");
                }
                return Ok(0);
            }
            if cmd_remove(&home, name, *yes)? {
                if !cli.quiet {
                    println!("removed skill {:?}", name);
                }
            } else if !cli.quiet {
                println!("remove cancelled");
            }
            Ok(0)
        }
        crate::cli::SkillSubcommand::Test { name } => {
            let code = cmd_test(&home, name)?;
            Ok(code)
        }
        crate::cli::SkillSubcommand::Create { name, dry_run } => {
            if *dry_run {
                validate_skill_name(name)?;
                let dir = skills_root(&home).join(name);
                if dir.exists() {
                    return Err(crate::coded_error(
                        crate::exit_code::CONFLICT,
                        format!("skill {:?} already exists at {}", name, dir.display()),
                    ));
                }
                if !cli.quiet {
                    println!("would create skill {name} at {}", dir.display());
                    println!("  + SKILL.md, zen-plugin.json, test.md");
                    println!("  registry entry: {name} v0.1.0 (unverified)");
                    println!("(no change made; drop --dry-run to apply)");
                }
                return Ok(0);
            }
            let dir = create_skill(&home, name)?;
            if !cli.quiet {
                println!("created skill {:?} at {}", name, dir.display());
            }
            if !cli.quiet {
                println!(
                    "next: edit SKILL.md, zen-plugin.json, test.md - then `zen skill test {}`",
                    name
                );
            }
            Ok(0)
        }
    }
}

fn cmd_list(cli: &crate::cli::Cli, home: &Path, cwd: &Path) -> Result<()> {
    let registry = load_registry(home)?;
    // The discovered section mirrors what the model can load in this
    // folder: project skills appear only when the folder is trusted.
    let discovered = zen_ext::discover_skills(
        cwd,
        Some(home),
        crate::settings::folder_trusted(cli, cwd, Some(home)),
    );
    print!("{}", format_skill_list(&registry, &discovered));
    Ok(())
}

/// Pure rendering of `zen skill list`, shared by the text output and
/// unit tests. The installed section is the registry; the discovered
/// section adds plain `.zen/skills/` directories the model can load
/// (same discovery as the system prompt and the Skill tool) that have
/// no registry entry, marked advisory. Skills already in the registry
/// are not repeated in the discovered section.
fn format_skill_list(registry: &Registry, discovered: &[zen_core::SkillInfo]) -> String {
    let mut out = String::new();
    let extra: Vec<&zen_core::SkillInfo> = discovered
        .iter()
        .filter(|s| !registry.skills.contains_key(&s.name))
        .collect();
    if registry.skills.is_empty() && extra.is_empty() {
        out.push_str("no skills installed. Try `zen skill install builtin:tdd`.\n");
        return out;
    }
    if !registry.skills.is_empty() {
        out.push_str("installed skills:\n");
        for (name, e) in &registry.skills {
            out.push_str(&format!(
                "  {} {} v{} - {}\n",
                e.tier.badge(),
                name,
                e.version,
                e.source
            ));
        }
    }
    if !extra.is_empty() {
        out.push_str("discovered skills (not installed, advisory):\n");
        for s in extra {
            out.push_str(&format!("  {} - {}\n", s.name, s.description));
        }
    }
    out
}

/// Note printed after the permission preview when the skill to install
/// carries a valid ed25519 signature. A pure function so the
/// Verified-only branch stays unit-testable.
fn verified_signature_note(tier: &TrustTier) -> Option<&'static str> {
    if *tier == TrustTier::Verified {
        Some("signature: valid (signed by Zen)")
    } else {
        None
    }
}

fn cmd_install(
    home: &Path,
    source: &str,
    yes: bool,
    registry_override: Option<&str>,
    dry_run: bool,
    quiet: bool,
) -> Result<()> {
    if let Some(name) = source.strip_prefix("registry:") {
        let reg = crate::skill_registry::load_community_registry(registry_override)?;
        let entry = crate::skill_registry::find_in_registry(&reg, name).ok_or_else(|| {
            anyhow!("no skill {name:?} in the community registry (try `zen skill search {name}`)")
        })?;
        let resolved = match &entry.source {
            crate::skill_registry::RegistrySource::Builtin => format!("builtin:{}", entry.name),
            crate::skill_registry::RegistrySource::Git { url } => url.clone(),
        };
        println!("registry:{} → {}", entry.name, resolved);
        println!("{}", crate::skill_registry::format_skill_info(entry));
        return cmd_install(home, &resolved, yes, registry_override, dry_run, quiet);
    }

    if let Some(name) = source.strip_prefix("builtin:") {
        if dry_run {
            let manifest = builtin_manifest(name)?;
            if !quiet {
                let dest = skills_root(home).join(name);
                println!(
                    "would install builtin skill {name} v{} to {}",
                    manifest.version,
                    dest.display()
                );
                if let Some(old) = load_registry(home)?.skills.get(name) {
                    println!(
                        "  replaces: {name} v{} (installed {})",
                        old.version, old.installed_at
                    );
                }
                println!("(no change made; drop --dry-run to apply)");
            }
            return Ok(());
        }
        if !yes {
            return Err(crate::coded_error(
                crate::exit_code::FORBIDDEN,
                "refusing to install without explicit approval: re-run with --yes after reviewing permissions",
            ));
        }
        let entry = install_builtin(home, name)?;
        println!(
            "installed builtin skill {} v{} {}",
            entry.name,
            entry.version,
            entry.tier.badge()
        );
        return Ok(());
    }
    let src = InstallSource::parse(source)?;

    let staging = std::env::temp_dir().join(format!("zen-skill-preview-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    // The name install_skill would install under: the clone/copy
    // source name, overridden by the manifest name, then by the SKILL.md
    // frontmatter name - the same order as install_skill.
    let mut preview_name: Option<String> = match &src {
        InstallSource::Local(p) => p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|n| !n.is_empty()),
        _ => None,
    };
    let preview_manifest: Option<PluginManifest> = match &src {
        InstallSource::Git(url) => clone_skill_repo(url, &staging).ok().and_then(|(n, _)| {
            preview_name = Some(n);
            load_manifest(&staging).ok()
        }),
        InstallSource::Local(p) => copy_dir_all(p, &staging)
            .ok()
            .and_then(|_| load_manifest(&staging).ok()),
        InstallSource::Builtin => None,
    };
    if let Some(m) = &preview_manifest {
        if !m.name.trim().is_empty() {
            preview_name = Some(m.name.clone());
        }
    } else if let Ok(skill_md) = std::fs::read_to_string(staging.join("SKILL.md"))
        && let (Some(fm), _) = zen_ext::split_frontmatter(&skill_md)
        && let Some(n) = fm.name.filter(|n| !n.trim().is_empty())
    {
        preview_name = Some(n);
    }
    std::fs::remove_dir_all(&staging).ok();

    match &preview_manifest {
        Some(m) => {
            println!("{}", permission_diff(None, m));
            let tier = assign_tier(&src, Some(m));
            println!(
                "trust tier after install: {} {}",
                tier.badge(),
                tier.as_str()
            );
            if let Some(note) = verified_signature_note(&tier) {
                println!("{note}");
            }
        }
        None => {
            println!("no zen-plugin.json found → will install as [⚠ unverified] (sandboxed).");
        }
    }
    if dry_run {
        let name = preview_name.ok_or_else(|| {
            anyhow!(
                "cannot preview install of {source:?}: no skill found (clone/copy failed or no SKILL.md)"
            )
        })?;
        let version = preview_manifest
            .as_ref()
            .map(|m| m.version.clone())
            .unwrap_or_else(|| "0.0.0".to_string());
        if !quiet {
            let dest = skills_root(home).join(&name);
            println!(
                "would install skill {name} v{version} from {source} to {}",
                dest.display()
            );
            if let Some(old) = load_registry(home)?.skills.get(&name) {
                println!(
                    "  replaces: {name} v{} (installed {})",
                    old.version, old.installed_at
                );
            }
            println!("(no change made; drop --dry-run to apply)");
        }
        return Ok(());
    }
    if !yes {
        println!("re-run with --yes to approve and install.");
        return Ok(());
    }
    let entry = install_skill(home, &src, true)?;
    println!(
        "installed {} v{} {} from {}",
        entry.name,
        entry.version,
        entry.tier.badge(),
        entry.source
    );
    Ok(())
}

fn cmd_test(home: &Path, name: &str) -> Result<i32> {
    let dir = skills_root(home).join(name);
    if !dir.is_dir() {
        return Err(anyhow!("no installed skill named {:?}", name));
    }
    let results = run_skill_tests(&dir)?;
    let passed = results.iter().filter(|r| r.passed).count();
    println!(
        "skill {:?}: {}/{} checks passed",
        name,
        passed,
        results.len()
    );
    for r in &results {
        let mark = if r.passed { "PASS" } else { "FAIL" };
        println!("  [{}] {}", mark, r.check);
        if !r.passed {
            println!("        {}", r.detail);
        }
    }
    Ok(if passed == results.len() { 0 } else { 1 })
}

pub fn tier_for(home: &Path, name: &str) -> TrustTier {
    // Fail closed: a corrupt registry is unreadable, so the skill is
    // treated as unverified (and the warning says why).
    match load_registry(home) {
        Ok(registry) => tier_in_registry(&registry, name),
        Err(e) => {
            eprintln!("warning: cannot read skill registry, treating {name:?} as unverified: {e}");
            TrustTier::Unverified
        }
    }
}

/// Trust tier of a skill within an already-loaded registry. Callers
/// that look up many skills (e.g. the system prompt) load the registry
/// once and use this instead of [`tier_for`], which reads the registry
/// file from disk on every call.
pub fn tier_in_registry(registry: &Registry, name: &str) -> TrustTier {
    registry
        .skills
        .get(name)
        .map(|e| e.tier)
        .unwrap_or(TrustTier::Unverified)
}

pub fn skills_prompt_with_tiers(home: Option<&Path>, skills: &[zen_core::SkillInfo]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let home = home.map(|p| p.to_path_buf()).or_else(dirs::home_dir);
    // A corrupt registry warns and falls back to unverified badges
    // (fail closed); it is a prompt string, not a load path, so the
    // error cannot propagate here. Nothing is written.
    let registry = home.as_deref().and_then(|h| match load_registry(h) {
        Ok(registry) => Some(registry),
        Err(e) => {
            eprintln!("warning: cannot read skill registry: {e}");
            None
        }
    });
    let mut out = String::from("Available skills (load with the Skill tool):\n");
    for s in skills {
        let badge = registry
            .as_ref()
            .map(|r| tier_in_registry(r, &s.name).badge())
            .unwrap_or("[⚠ unverified]");
        out.push_str(&format!("- {} {}: {}\n", s.name, badge, s.description));
    }
    // Default wording for the G3 gap (Leader has not ruled yet): plain
    // skill directories without a registry entry are advisory - loading
    // them adds no permission filtering (see docs/skills.md). Only
    // registry-installed unverified skills are sandboxed. Text only;
    // enforcement is unchanged.
    out.push_str("Trust: [✓ verified] = audited + signed by Zen; [community] = user-approved; [⚠ unverified] = advisory for plain skill directories (no permission filtering); installed unverified skills are sandboxed.");
    out
}

use std::sync::Arc;

pub struct SkillPolicyChecker {
    inner: Arc<dyn zen_core::PermissionChecker>,
    allowed: Option<Vec<String>>,
    skill_name: String,
}

impl SkillPolicyChecker {
    pub fn new(inner: Arc<dyn zen_core::PermissionChecker>, skill: &LoadedSkill) -> Self {
        Self {
            inner,
            allowed: allowed_tool_names(skill),
            skill_name: skill.name.clone(),
        }
    }

    pub fn union(inner: Arc<dyn zen_core::PermissionChecker>, skills: &[LoadedSkill]) -> Self {
        let mut names = vec![];
        for s in skills {
            match allowed_tool_names(s) {
                None => {
                    return Self {
                        inner,
                        allowed: None,
                        skill_name: "advisory".to_string(),
                    };
                }
                Some(n) => names.extend(n),
            }
        }
        names.sort();
        names.dedup();
        Self {
            inner,
            allowed: Some(names),
            skill_name: skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join("+"),
        }
    }
}

impl zen_core::PermissionChecker for SkillPolicyChecker {
    fn check(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        read_only: bool,
        ctx: &zen_core::ToolContext,
    ) -> zen_core::PermissionDecision {
        if let Some(allowed) = &self.allowed
            && !allowed.iter().any(|t| t.eq_ignore_ascii_case(tool_name))
        {
            return zen_core::PermissionDecision::Deny(format!(
                "skill {:?} does not declare tool {:?} in its permissions",
                self.skill_name, tool_name
            ));
        }
        self.inner.check(tool_name, input, read_only, ctx)
    }

    fn add_allow_rule(&self, rule: &str) {
        self.inner.add_allow_rule(rule)
    }

    fn ask_rule_matches(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        ctx: &zen_core::ToolContext,
    ) -> bool {
        self.inner.ask_rule_matches(tool_name, input, ctx)
    }
}

pub struct SkillPermissionGate {
    inner: Arc<dyn zen_core::PermissionChecker>,
    active: Arc<std::sync::Mutex<Vec<LoadedSkill>>>,
}

impl SkillPermissionGate {
    pub fn new(inner: Arc<dyn zen_core::PermissionChecker>) -> Self {
        Self {
            inner,
            active: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    pub fn shared(&self, inner: Arc<dyn zen_core::PermissionChecker>) -> Self {
        Self {
            inner,
            active: Arc::clone(&self.active),
        }
    }

    pub fn load_skill(&self, skill: LoadedSkill) {
        let mut active = lock_or_recover(&self.active, "skill registry");
        if let Some(pos) = active.iter().position(|s| s.name == skill.name) {
            active[pos] = skill;
        } else {
            active.push(skill);
        }
    }

    pub fn active_skills(&self) -> Vec<LoadedSkill> {
        lock_or_recover(&self.active, "skill registry").clone()
    }
}

impl zen_core::PermissionChecker for SkillPermissionGate {
    fn check(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        read_only: bool,
        ctx: &zen_core::ToolContext,
    ) -> zen_core::PermissionDecision {
        let active = lock_or_recover(&self.active, "skill registry").clone();
        if active.is_empty() {
            return self.inner.check(tool_name, input, read_only, ctx);
        }

        SkillPolicyChecker::union(self.inner.clone(), &active)
            .check(tool_name, input, read_only, ctx)
    }

    fn add_allow_rule(&self, rule: &str) {
        self.inner.add_allow_rule(rule)
    }

    fn ask_rule_matches(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        ctx: &zen_core::ToolContext,
    ) -> bool {
        self.inner.ask_rule_matches(tool_name, input, ctx)
    }
}

pub struct GatedSkillTool {
    inner: zen_ext::SkillTool,
    gate: Arc<SkillPermissionGate>,
    by_name: std::collections::HashMap<String, LoadedSkill>,
}

impl GatedSkillTool {
    pub fn new(
        inner: zen_ext::SkillTool,
        gate: Arc<SkillPermissionGate>,
        installed: Vec<LoadedSkill>,
    ) -> Self {
        Self {
            inner,
            gate,
            by_name: installed.into_iter().map(|s| (s.name.clone(), s)).collect(),
        }
    }

    #[cfg(test)]
    fn knows(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }
}

#[async_trait::async_trait]
impl zen_core::Tool for GatedSkillTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    async fn call(
        &self,
        args: serde_json::Value,
        ctx: &zen_core::ToolContext,
    ) -> zen_core::ToolOutput {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let out = self.inner.call(args, ctx).await;

        if !out.is_error
            && let Some(name) = name
            && let Some(skill) = self.by_name.get(&name)
        {
            self.gate.load_skill(skill.clone());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zen_core::PermissionChecker;

    fn tmp_home(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zen-skill-test-{}-{}", name, std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_manifest() -> PluginManifest {
        PluginManifest {
            name: "tdd".to_string(),
            version: "1.0.0".to_string(),
            description: "Test-driven development".to_string(),
            author: "ZenCode".to_string(),
            permissions: SkillPermissions {
                allowed_tools: vec!["read".to_string(), "bash".to_string()],
                network: false,
                file_write: true,
                exec: true,
                file_scope: "project".to_string(),
            },
            skills: vec![".".to_string()],
            signature: None,
        }
    }

    #[test]
    fn sha256_test_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn manifest_validate_ok() {
        assert!(sample_manifest().validate().is_empty());
    }

    #[test]
    fn manifest_validate_problems() {
        let mut m = sample_manifest();
        m.name = "bad name!".to_string();
        m.permissions.file_scope = "everywhere".to_string();
        m.permissions.exec = true;
        m.permissions.allowed_tools = vec!["read".to_string()];
        let problems = m.validate();
        assert!(problems.iter().any(|p| p.contains("name")));
        assert!(problems.iter().any(|p| p.contains("file_scope")));
        assert!(problems.iter().any(|p| p.contains("bash")));
    }

    #[test]
    fn canonical_bytes_deterministic() {
        let m = sample_manifest();
        assert_eq!(m.canonical_bytes(), m.canonical_bytes());

        let mut m2 = m.clone();
        m2.permissions.allowed_tools = vec!["bash".to_string(), "read".to_string()];
        assert_eq!(m.canonical_bytes(), m2.canonical_bytes());

        m2.signature = Some("abcd".to_string());
        assert_eq!(m.canonical_bytes(), m2.canonical_bytes());
    }

    #[test]
    fn signature_roundtrip_with_test_key() {
        assert!(!verify_signature(b"hello", "zz"));
        assert!(!verify_signature(b"hello", "00".repeat(64).as_str()));
        assert!(!verify_signature(b"hello", "ff".repeat(63).as_str()));
    }

    #[test]
    fn hex_decode_non_ascii_returns_err_instead_of_panicking() {
        // "a\u{20ac}" is 4 bytes (even length) but slicing [0..2] would cut
        // the 3-byte euro sign and panic on the unfixed code.
        assert!(hex_decode("a\u{20ac}").is_err());
        assert!(hex_decode("\u{20ac}\u{20ac}").is_err());
        assert_eq!(hex_decode("00ff").unwrap(), vec![0x00, 0xff]);
        assert!(!verify_signature(b"hello", "a\u{20ac}"));
        assert!(!verify_signature(
            b"hello",
            "00\u{20ac}00".repeat(16).as_str()
        ));
    }

    #[test]
    fn signature_verifies_real_sig() {
        let m = sample_manifest();
        let msg = m.canonical_bytes();

        let sig = test_signature_for(&msg);
        assert!(
            verify_signature(&msg, &sig),
            "baked-in pubkey must verify dev signatures"
        );

        let mut tampered = msg.clone();
        tampered.push(b'!');
        assert!(!verify_signature(&tampered, &sig));
    }

    fn test_signature_for(msg: &[u8]) -> String {
        let expected = sample_manifest().canonical_bytes();
        if msg == expected.as_slice() {
            "d5f6ccbbfc66ecbc9b3f1e77c30112427e91dd3675239466a8108a1795b17bccefd7d36efec68a6aa9c481057b178d9ac61cabcefacd4ef36821760fc1585e0c".to_string()
        } else {
            "00".repeat(64)
        }
    }

    #[test]
    fn tier_badges() {
        assert_eq!(TrustTier::Verified.badge(), "[✓ verified]");
        assert_eq!(TrustTier::Community.badge(), "[community]");
        assert_eq!(TrustTier::Unverified.badge(), "[⚠ unverified]");
    }

    #[test]
    fn assign_tier_builtin_is_verified() {
        assert_eq!(
            assign_tier(&InstallSource::Builtin, None),
            TrustTier::Verified
        );
    }

    #[test]
    fn assign_tier_no_manifest_is_unverified() {
        let src = InstallSource::Local(PathBuf::from("/tmp"));
        assert_eq!(assign_tier(&src, None), TrustTier::Unverified);
    }

    #[test]
    fn unverified_always_sandboxed() {
        let s = LoadedSkill {
            name: "evil".to_string(),
            description: "x".to_string(),
            body: "x".to_string(),
            tier: TrustTier::Unverified,
            permissions: SkillPermissions {
                allowed_tools: vec!["bash".to_string()],
                network: true,
                file_write: true,
                exec: true,
                file_scope: "user".to_string(),
            },
        };
        let eff = s.effective_permissions();
        assert!(!eff.network && !eff.file_write && !eff.exec);
        assert_eq!(eff.file_scope, "project");
    }

    #[test]
    fn permission_diff_new_skill() {
        let diff = permission_diff(None, &sample_manifest());
        assert!(diff.contains("tdd"));
        assert!(diff.contains("+ tools: read, bash"));
        assert!(diff.contains("+ network: no"));
    }

    #[test]
    fn parse_check_variants() {
        let c = parse_check_line("- CHECK frontmatter.name == \"tdd\"").unwrap();
        assert_eq!(c.target, "frontmatter.name");
        assert_eq!(c.op, CheckOp::Eq);
        assert_eq!(c.value, "tdd");
        let c = parse_check_line("- CHECK instructions not contains rm -rf").unwrap();
        assert_eq!(c.op, CheckOp::NotContains);
        assert_eq!(c.value, "rm -rf");
        assert!(parse_check_line("- CHECK nonsense").is_none());
        assert!(parse_check_line("just a line").is_none());
    }

    fn write_skill_dir(
        root: &Path,
        manifest: Option<&PluginManifest>,
        skill_md: &str,
        test_md: &str,
    ) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("SKILL.md"), skill_md).unwrap();
        if let Some(m) = manifest {
            std::fs::write(
                root.join("zen-plugin.json"),
                serde_json::to_string_pretty(m).unwrap(),
            )
            .unwrap();
        }
        std::fs::write(root.join("test.md"), test_md).unwrap();
    }

    #[test]
    fn eval_runner_pass_and_fail() {
        let home = tmp_home("eval");
        let dir = home.join("skills").join("demo");
        write_skill_dir(
            &dir,
            Some(&sample_manifest()),
            "---\nname: tdd\ndescription: Test-driven development\n---\n\nFollow RED-GREEN-REFACTOR.\n",
            "# Tests\n- CHECK frontmatter.name == \"tdd\"\n- CHECK manifest.permissions.network == false\n- CHECK instructions contains \"RED-GREEN\"\n- CHECK instructions not contains \"rm -rf /\"\n- CHECK frontmatter.name == \"wrong\"\n",
        );
        let results = run_skill_tests(&dir).unwrap();
        assert_eq!(results.len(), 5);
        assert_eq!(results.iter().filter(|r| r.passed).count(), 4);
        assert!(!results[4].passed);
        assert!(results[4].detail.contains("actual"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn eval_runner_list_contains() {
        let home = tmp_home("eval-list");
        let dir = home.join("skills").join("demo");
        write_skill_dir(
            &dir,
            Some(&sample_manifest()),
            "---\nname: tdd\ndescription: x\n---\nbody\n",
            "- CHECK manifest.permissions.allowed_tools contains bash\n- CHECK manifest.permissions.allowed_tools not contains curl\n",
        );
        let results = run_skill_tests(&dir).unwrap();
        assert!(results.iter().all(|r| r.passed));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn eval_runner_requires_test_md() {
        let home = tmp_home("eval-note");
        let dir = home.join("skills").join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: x\ndescription: y\n---\n").unwrap();
        assert!(run_skill_tests(&dir).is_err());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_from_local_path_roundtrip() {
        let home = tmp_home("install");
        let src = home.join("src-skill");
        write_skill_dir(
            &src,
            Some(&sample_manifest()),
            "---\nname: tdd\ndescription: Test-driven development\n---\nbody\n",
            "- CHECK frontmatter.name == \"tdd\"\n",
        );
        let src_enum = InstallSource::Local(src.clone());

        assert!(install_skill(&home, &src_enum, false).is_err());

        let entry = install_skill(&home, &src_enum, true).unwrap();
        assert_eq!(entry.name, "tdd");
        assert_eq!(entry.tier, TrustTier::Community);
        assert!(skills_root(&home).join("tdd").join("SKILL.md").is_file());

        let reg = load_registry(&home).unwrap();
        assert!(reg.skills.contains_key("tdd"));

        remove_skill(&home, "tdd").unwrap();
        assert!(!load_registry(&home).unwrap().skills.contains_key("tdd"));
        assert!(!skills_root(&home).join("tdd").exists());
        assert!(remove_skill(&home, "tdd").is_err());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_without_manifest_is_unverified() {
        let home = tmp_home("install-unv");
        let src = home.join("src-skill");
        write_skill_dir(
            &src,
            None,
            "---\nname: nosig\ndescription: no manifest here\n---\nbody\n",
            "- CHECK frontmatter.name == \"nosig\"\n",
        );
        let entry = install_skill(&home, &InstallSource::Local(src), true).unwrap();
        assert_eq!(entry.tier, TrustTier::Unverified);

        assert_eq!(entry.name, "nosig");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_rejects_frontmatter_name_traversal() {
        // Issue #371: a SKILL.md frontmatter name was joined into
        // skills_root unchecked, so install deleted and overwrote a
        // directory outside ~/.zen/skills. The install must now fail
        // with VALIDATION and leave the traversal target untouched.
        let home = tmp_home("install-traversal");
        // skills_root is home/.zen/skills, so "../../evil" lands on
        // home/evil.
        let victim = home.join("evil");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep.txt"), "do not delete").unwrap();
        let abs_victim = home.join("abs-victim");
        std::fs::create_dir_all(&abs_victim).unwrap();
        std::fs::write(abs_victim.join("keep.txt"), "do not delete").unwrap();

        let bad_names = vec![
            "../../evil".to_string(),
            "..".to_string(),
            ".".to_string(),
            "a/b".to_string(),
            abs_victim.display().to_string(),
        ];
        for bad in &bad_names {
            let src = home.join("src-skill");
            if src.exists() {
                std::fs::remove_dir_all(&src).ok();
            }
            write_skill_dir(
                &src,
                None,
                &format!("---\nname: {bad}\ndescription: x\n---\nbody\n"),
                "- CHECK frontmatter.name == \"x\"\n",
            );
            let err = install_skill(&home, &InstallSource::Local(src), true).unwrap_err();
            assert_eq!(
                crate::exit_code_for_error(&err),
                crate::exit_code::VALIDATION,
                "frontmatter name {bad:?} should be rejected with VALIDATION, got {err:?}"
            );
        }

        assert_eq!(
            std::fs::read_to_string(victim.join("keep.txt")).unwrap(),
            "do not delete"
        );
        assert_eq!(
            std::fs::read_to_string(abs_victim.join("keep.txt")).unwrap(),
            "do not delete"
        );
        assert!(!skills_root(&home).join("evil").exists());
        assert!(load_registry(&home).unwrap().skills.is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_accepts_valid_frontmatter_name() {
        // The #371 gate must not reject ordinary names: letters, digits,
        // '-' and '_' still install from frontmatter alone.
        let home = tmp_home("install-valid-name");
        let src = home.join("src-skill");
        write_skill_dir(
            &src,
            None,
            "---\nname: my-skill_2\ndescription: x\n---\nbody\n",
            "- CHECK frontmatter.name == \"my-skill_2\"\n",
        );
        let entry = install_skill(&home, &InstallSource::Local(src), true).unwrap();
        assert_eq!(entry.name, "my-skill_2");
        assert!(
            skills_root(&home)
                .join("my-skill_2")
                .join("SKILL.md")
                .is_file()
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_requires_skill_md() {
        let home = tmp_home("install-noskillmd");
        let src = home.join("src-skill");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("zen-plugin.json"), "{}").unwrap();
        assert!(install_skill(&home, &InstallSource::Local(src), true).is_err());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn create_scaffolds_and_self_tests() {
        let home = tmp_home("create");
        let dir = create_skill(&home, "my-skill").unwrap();
        assert!(dir.join("SKILL.md").is_file());
        assert!(dir.join("zen-plugin.json").is_file());
        assert!(dir.join("test.md").is_file());

        let results = run_skill_tests(&dir).unwrap();
        assert!(results.iter().all(|r| r.passed), "{:?}", results);

        assert!(create_skill(&home, "my-skill").is_err());

        assert!(create_skill(&home, "bad name!").is_err());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_source_parse() {
        assert!(matches!(
            InstallSource::parse("builtin:tdd").unwrap(),
            InstallSource::Builtin
        ));
        assert!(matches!(
            InstallSource::parse("https://github.com/x/y.git").unwrap(),
            InstallSource::Git(_)
        ));
        assert!(InstallSource::parse("builtin:").is_err());
        assert!(InstallSource::parse("nope-not-a-path-xyz").is_err());
    }

    #[test]
    fn allowed_tool_names_mapping() {
        let s = LoadedSkill {
            name: "t".to_string(),
            description: "d".to_string(),
            body: "b".to_string(),
            tier: TrustTier::Community,
            permissions: SkillPermissions {
                allowed_tools: vec![
                    "read".to_string(),
                    "bash".to_string(),
                    "webfetch".to_string(),
                ],
                network: true,
                file_write: false,
                exec: true,
                file_scope: "project".to_string(),
            },
        };
        let names = allowed_tool_names(&s).unwrap();
        assert!(names.contains(&"Read".to_string()));
        assert!(names.contains(&"Bash".to_string()));
        assert!(names.contains(&"WebFetch".to_string()));
        assert!(names.contains(&"Skill".to_string()));
    }

    #[test]
    fn advisory_skill_no_filtering() {
        let s = LoadedSkill {
            name: "t".to_string(),
            description: "d".to_string(),
            body: "b".to_string(),
            tier: TrustTier::Verified,
            permissions: SkillPermissions::advisory(),
        };
        assert!(allowed_tool_names(&s).is_none());
    }

    #[test]
    fn skills_system_prompt_badges() {
        let skills = vec![LoadedSkill {
            name: "tdd".to_string(),
            description: "TDD workflow".to_string(),
            body: "b".to_string(),
            tier: TrustTier::Verified,
            permissions: SkillPermissions::advisory(),
        }];
        let p = skills_system_prompt(&skills);
        assert!(p.contains("tdd"));
        assert!(p.contains("[✓ verified]"));
        assert!(skills_system_prompt(&[]).is_empty());
    }

    #[test]
    fn load_enabled_skills_roundtrip() {
        let home = tmp_home("load");
        let dir = create_skill(&home, "demo").unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\nDo things.\n",
        )
        .unwrap();
        let skills = load_enabled_skills(&home);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "demo");
        assert_eq!(skills[0].tier, TrustTier::Unverified);

        assert!(!skills[0].effective_permissions().file_write);
        std::fs::remove_dir_all(&home).ok();
    }

    struct AllowAll;
    impl zen_core::PermissionChecker for AllowAll {
        fn check(
            &self,
            _tool: &str,
            _input: &serde_json::Value,
            _ro: bool,
            _ctx: &zen_core::ToolContext,
        ) -> zen_core::PermissionDecision {
            zen_core::PermissionDecision::Allow
        }
    }

    fn dummy_ctx() -> zen_core::ToolContext {
        zen_core::ToolContext::new(std::path::PathBuf::from("/tmp"), "test-session")
    }

    fn policy_skill() -> LoadedSkill {
        LoadedSkill {
            name: "tdd".to_string(),
            description: "d".to_string(),
            body: "b".to_string(),
            tier: TrustTier::Community,
            permissions: SkillPermissions {
                allowed_tools: vec!["read".to_string(), "bash".to_string()],
                network: false,
                file_write: true,
                exec: true,
                file_scope: "project".to_string(),
            },
        }
    }

    #[test]
    fn policy_checker_denies_undeclared_tools() {
        let checker = SkillPolicyChecker::new(std::sync::Arc::new(AllowAll), &policy_skill());
        let ctx = dummy_ctx();
        let v = serde_json::Value::Null;
        assert!(matches!(
            checker.check("Read", &v, true, &ctx),
            zen_core::PermissionDecision::Allow
        ));
        assert!(matches!(
            checker.check("Bash", &v, false, &ctx),
            zen_core::PermissionDecision::Allow
        ));
        match checker.check("Write", &v, false, &ctx) {
            zen_core::PermissionDecision::Deny(reason) => {
                assert!(reason.contains("tdd") && reason.contains("Write"))
            }
            other => panic!("expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn policy_checker_advisory_delegates() {
        let mut s = policy_skill();
        s.permissions = SkillPermissions::advisory();
        let checker = SkillPolicyChecker::new(std::sync::Arc::new(AllowAll), &s);
        let ctx = dummy_ctx();
        let v = serde_json::Value::Null;
        assert!(matches!(
            checker.check("Anything", &v, false, &ctx),
            zen_core::PermissionDecision::Allow
        ));
    }

    #[test]
    fn policy_checker_union() {
        let s1 = policy_skill();
        let mut s2 = policy_skill();
        s2.name = "other".to_string();
        s2.permissions.allowed_tools = vec!["grep".to_string()];
        let checker = SkillPolicyChecker::union(std::sync::Arc::new(AllowAll), &[s1, s2]);
        let ctx = dummy_ctx();
        let v = serde_json::Value::Null;
        assert!(matches!(
            checker.check("Grep", &v, true, &ctx),
            zen_core::PermissionDecision::Allow
        ));
        assert!(matches!(
            checker.check("Write", &v, false, &ctx),
            zen_core::PermissionDecision::Deny(_)
        ));
    }

    #[test]
    fn skills_prompt_with_tiers_badges() {
        let home = tmp_home("prompt-tiers");
        install_builtin(&home, "tdd").unwrap();
        let skills = vec![zen_core::SkillInfo {
            name: "tdd".to_string(),
            description: "TDD workflow".to_string(),
            path: std::path::PathBuf::from("/x"),
            body: "b".to_string(),
            allowed_tools: vec![],
        }];
        let p = skills_prompt_with_tiers(Some(&home), &skills);
        assert!(p.contains("[✓ verified]"), "{}", p);
        assert!(p.contains("Trust:"), "{}", p);

        let skills2 = vec![zen_core::SkillInfo {
            name: "nope".to_string(),
            description: "d".to_string(),
            path: std::path::PathBuf::from("/x"),
            body: "b".to_string(),
            allowed_tools: vec![],
        }];
        let p2 = skills_prompt_with_tiers(Some(&home), &skills2);
        assert!(p2.contains("[⚠ unverified]"), "{}", p2);
        // G3 default wording: plain (unregistered) skills are advisory,
        // not sandboxed; the old blanket "sandboxed" claim is gone.
        assert!(
            p2.contains("advisory for plain skill directories"),
            "{}",
            p2
        );
        assert!(!p2.contains("[⚠ unverified] = sandboxed"), "{}", p2);
        std::fs::remove_dir_all(&home).ok();
    }

    struct DenyAll;
    impl zen_core::PermissionChecker for DenyAll {
        fn check(
            &self,
            _tool: &str,
            _input: &serde_json::Value,
            _ro: bool,
            _ctx: &zen_core::ToolContext,
        ) -> zen_core::PermissionDecision {
            zen_core::PermissionDecision::Deny("no".to_string())
        }
    }

    fn write_only_skill() -> LoadedSkill {
        let mut s = policy_skill();
        s.name = "writer".to_string();
        s.permissions.allowed_tools = vec!["write".to_string()];
        s
    }

    #[test]
    fn gate_delegates_when_no_skill_loaded() {
        let gate = SkillPermissionGate::new(std::sync::Arc::new(AllowAll));
        let ctx = dummy_ctx();
        let v = serde_json::Value::Null;
        assert!(matches!(
            gate.check("Bash", &v, false, &ctx),
            zen_core::PermissionDecision::Allow
        ));

        let gate2 = SkillPermissionGate::new(std::sync::Arc::new(DenyAll));
        assert!(matches!(
            gate2.check("Read", &v, true, &ctx),
            zen_core::PermissionDecision::Deny(_)
        ));
        assert!(gate.active_skills().is_empty());
    }

    #[test]
    fn gate_enforces_loaded_skill_allowlist() {
        let gate = SkillPermissionGate::new(std::sync::Arc::new(AllowAll));
        gate.load_skill(policy_skill());
        let ctx = dummy_ctx();
        let v = serde_json::Value::Null;
        assert!(matches!(
            gate.check("Read", &v, true, &ctx),
            zen_core::PermissionDecision::Allow
        ));
        assert!(matches!(
            gate.check("Bash", &v, false, &ctx),
            zen_core::PermissionDecision::Allow
        ));
        match gate.check("Write", &v, false, &ctx) {
            zen_core::PermissionDecision::Deny(reason) => {
                assert!(reason.contains("tdd"), "reason names the skill: {reason}")
            }
            other => panic!("expected Deny, got {other:?}"),
        }

        assert!(matches!(
            gate.check("Skill", &v, true, &ctx),
            zen_core::PermissionDecision::Allow
        ));
    }

    #[test]
    fn gate_union_of_loaded_skills() {
        let gate = SkillPermissionGate::new(std::sync::Arc::new(AllowAll));
        gate.load_skill(policy_skill());
        gate.load_skill(write_only_skill());
        let ctx = dummy_ctx();
        let v = serde_json::Value::Null;
        for tool in ["Read", "Bash", "Write"] {
            assert!(
                matches!(
                    gate.check(tool, &v, false, &ctx),
                    zen_core::PermissionDecision::Allow
                ),
                "{tool} should be allowed by the union"
            );
        }
        assert!(matches!(
            gate.check("Edit", &v, false, &ctx),
            zen_core::PermissionDecision::Deny(_)
        ));
    }

    #[test]
    fn gate_advisory_skill_adds_no_filtering() {
        let gate = SkillPermissionGate::new(std::sync::Arc::new(AllowAll));
        let mut s = policy_skill();
        s.permissions = SkillPermissions::advisory();
        gate.load_skill(s);
        let ctx = dummy_ctx();
        let v = serde_json::Value::Null;
        assert!(matches!(
            gate.check("Write", &v, false, &ctx),
            zen_core::PermissionDecision::Allow
        ));
    }

    #[test]
    fn gate_load_replaces_same_name() {
        let gate = SkillPermissionGate::new(std::sync::Arc::new(AllowAll));
        gate.load_skill(policy_skill());
        let mut s2 = policy_skill();
        s2.permissions.allowed_tools = vec!["write".to_string()];
        gate.load_skill(s2);
        let ctx = dummy_ctx();
        let v = serde_json::Value::Null;
        assert_eq!(gate.active_skills().len(), 1);
        assert!(matches!(
            gate.check("Write", &v, false, &ctx),
            zen_core::PermissionDecision::Allow
        ));
        assert!(matches!(
            gate.check("Read", &v, true, &ctx),
            zen_core::PermissionDecision::Deny(_)
        ));
    }

    #[test]
    fn gate_shared_state_across_gates() {
        let g1 = SkillPermissionGate::new(std::sync::Arc::new(AllowAll));
        let g2 = g1.shared(std::sync::Arc::new(AllowAll));
        g1.load_skill(policy_skill());
        let ctx = dummy_ctx();
        let v = serde_json::Value::Null;

        assert!(matches!(
            g2.check("Write", &v, false, &ctx),
            zen_core::PermissionDecision::Deny(_)
        ));
        assert_eq!(g2.active_skills().len(), 1);
    }

    #[tokio::test]
    async fn gated_skill_tool_records_loaded_skill() {
        use zen_core::{Tool, ToolContext};
        let tmp = std::env::temp_dir().join("zen-gate-test");
        let info = zen_core::SkillInfo {
            name: "tdd".to_string(),
            description: "d".to_string(),
            path: tmp.clone(),
            body: "tdd body".to_string(),
            allowed_tools: vec![],
        };
        let gate = std::sync::Arc::new(SkillPermissionGate::new(std::sync::Arc::new(AllowAll)));
        let tool = GatedSkillTool::new(
            zen_ext::SkillTool::new(vec![info]),
            gate.clone(),
            vec![policy_skill()],
        );
        assert!(tool.knows("tdd"));
        assert!(!tool.knows("nope"));
        let ctx = ToolContext::new(tmp, "s1");

        let out = tool.call(serde_json::json!({"name": "nope"}), &ctx).await;
        assert!(out.is_error);
        assert!(gate.active_skills().is_empty());

        let out = tool.call(serde_json::json!({"name": "tdd"}), &ctx).await;
        assert!(!out.is_error);
        assert!(out.content.contains("tdd body"));
        let active = gate.active_skills();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "tdd");

        let v = serde_json::Value::Null;
        assert!(matches!(
            gate.check("Write", &v, false, &ctx),
            zen_core::PermissionDecision::Deny(_)
        ));
    }

    #[test]
    fn builtin_install_verifies_signature() {
        let home = tmp_home("builtin-sig");
        for name in builtin_skill_names() {
            let entry = install_builtin(&home, name).unwrap();
            assert_eq!(entry.tier, TrustTier::Verified, "{}", name);

            let results = run_skill_tests(&skills_root(&home).join(name)).unwrap();
            assert!(
                results.iter().all(|r| r.passed),
                "{:?}",
                results.iter().filter(|r| !r.passed).collect::<Vec<_>>()
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cmd_install_builtin_installs_skill() {
        let home = tmp_home("cmd-install-builtin");
        cmd_install(&home, "builtin:tdd", true, None, false, false).unwrap();
        assert!(skills_root(&home).join("tdd").join("SKILL.md").exists());
        assert!(
            skills_root(&home)
                .join("tdd")
                .join("zen-plugin.json")
                .exists()
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cmd_install_builtin_without_yes_is_forbidden() {
        let home = tmp_home("cmd-install-builtin-no-yes");
        let err = cmd_install(&home, "builtin:tdd", false, None, false, false).unwrap_err();
        assert_eq!(
            crate::exit_code_for_error(&err),
            crate::exit_code::FORBIDDEN
        );
        assert!(
            !skills_root(&home).join("tdd").exists(),
            "a refused install must not write the skill"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cmd_install_builtin_second_install_is_conflict() {
        let home = tmp_home("cmd-install-builtin-twice");
        cmd_install(&home, "builtin:tdd", true, None, false, false).unwrap();
        let marker = "edited by the user - must survive a reinstall";
        std::fs::write(skills_root(&home).join("tdd").join("SKILL.md"), marker).unwrap();
        let err = cmd_install(&home, "builtin:tdd", true, None, false, false).unwrap_err();
        assert_eq!(crate::exit_code_for_error(&err), crate::exit_code::CONFLICT);
        assert_eq!(
            std::fs::read_to_string(skills_root(&home).join("tdd").join("SKILL.md")).unwrap(),
            marker
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cmd_remove_without_yes_is_forbidden_on_non_interactive_stdin() {
        let home = tmp_home("cmd-remove-no-yes");
        cmd_install(&home, "builtin:tdd", true, None, false, false).unwrap();
        let err = cmd_remove(&home, "tdd", false).unwrap_err();
        assert_eq!(
            crate::exit_code_for_error(&err),
            crate::exit_code::FORBIDDEN
        );
        assert!(
            skills_root(&home).join("tdd").join("SKILL.md").exists(),
            "a refused remove must not delete the skill"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cmd_remove_with_yes_removes_skill() {
        let home = tmp_home("cmd-remove-yes");
        cmd_install(&home, "builtin:tdd", true, None, false, false).unwrap();
        assert!(cmd_remove(&home, "tdd", true).unwrap());
        assert!(!skills_root(&home).join("tdd").exists());
        assert!(!load_registry(&home).unwrap().skills.contains_key("tdd"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn verified_signature_note_only_for_verified() {
        assert_eq!(
            verified_signature_note(&TrustTier::Verified),
            Some("signature: valid (signed by Zen)")
        );
        assert_eq!(verified_signature_note(&TrustTier::Community), None);
        assert_eq!(verified_signature_note(&TrustTier::Unverified), None);
    }

    fn test_skill_info(name: &str, description: &str) -> zen_core::SkillInfo {
        zen_core::SkillInfo {
            name: name.to_string(),
            description: description.to_string(),
            path: std::path::PathBuf::from("/x"),
            body: "b".to_string(),
            allowed_tools: vec![],
        }
    }

    #[test]
    fn format_skill_list_empty() {
        let registry = Registry::default();
        let out = format_skill_list(&registry, &[]);
        assert!(out.contains("no skills installed"), "{}", out);
    }

    #[test]
    fn format_skill_list_discovered_only() {
        let registry = Registry::default();
        let discovered = vec![test_skill_info("plain", "A plain skill")];
        let out = format_skill_list(&registry, &discovered);
        assert!(!out.contains("installed skills:"), "{}", out);
        assert!(
            out.contains("discovered skills (not installed, advisory):"),
            "{}",
            out
        );
        assert!(out.contains("plain - A plain skill"), "{}", out);
        assert!(!out.contains("no skills installed"), "{}", out);
    }

    #[test]
    fn format_skill_list_installed_and_discovered() {
        let home = tmp_home("list-unified");
        install_builtin(&home, "tdd").unwrap();
        let registry = load_registry(&home).unwrap();
        let discovered = vec![
            test_skill_info("tdd", "TDD workflow"),
            test_skill_info("plain", "A plain skill"),
        ];
        let out = format_skill_list(&registry, &discovered);
        assert!(out.contains("installed skills:"), "{}", out);
        assert!(out.contains("[✓ verified] tdd v1.0.0"), "{}", out);
        assert!(
            out.contains("discovered skills (not installed, advisory):"),
            "{}",
            out
        );
        assert!(out.contains("plain - A plain skill"), "{}", out);
        // tdd is installed: it must not reappear in the discovered section.
        let discovered_section = out.split("discovered skills").nth(1).unwrap_or("");
        assert!(!discovered_section.contains("tdd"), "{}", out);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn tier_for_matches_registry() {
        let home = tmp_home("tier-for");
        install_builtin(&home, "tdd").unwrap();
        assert_eq!(tier_for(&home, "tdd"), TrustTier::Verified);
        assert_eq!(tier_for(&home, "no-such-skill"), TrustTier::Unverified);
        let registry = load_registry(&home).unwrap();
        assert_eq!(tier_in_registry(&registry, "tdd"), TrustTier::Verified);
        assert_eq!(
            tier_in_registry(&registry, "no-such-skill"),
            TrustTier::Unverified
        );
        std::fs::remove_dir_all(&home).ok();
    }

    fn sample_registry_entry(name: &str) -> RegistryEntry {
        RegistryEntry {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            tier: TrustTier::Verified,
            source: "builtin".to_string(),
            commit: None,
            installed_at: "2026-11-27T00:00:00Z".to_string(),
            pinned_hash: "builtin".to_string(),
        }
    }

    fn sample_registry(name: &str) -> Registry {
        let mut registry = Registry::default();
        registry
            .skills
            .insert(name.to_string(), sample_registry_entry(name));
        registry
    }

    #[test]
    fn corrupt_registry_is_an_error_not_an_empty_registry() {
        // Issue #465: a crash mid-write leaves registry.json truncated,
        // and the old load silently returned an empty registry, so the
        // next install unregistered every skill. Missing file is still
        // fine (fresh home); a present-but-corrupt file must error.
        let home = tmp_home("registry-corrupt");
        assert!(load_registry(&home).unwrap().skills.is_empty());

        let registry = sample_registry("tdd");
        save_registry(&home, &registry).unwrap();
        assert!(load_registry(&home).unwrap().skills.contains_key("tdd"));

        // Truncated mid-object, as a crash during fs::write leaves it.
        let path = registry_path(&home);
        std::fs::write(&path, "{\"skills\": {\"tdd\": ").unwrap();
        assert!(
            load_registry(&home).is_err(),
            "truncated registry.json must error, not load as empty"
        );

        std::fs::write(&path, "not json at all").unwrap();
        assert!(load_registry(&home).is_err());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_refuses_to_overwrite_a_corrupt_registry() {
        // The read-modify-write install path must not clobber a corrupt
        // registry with a fresh one-entry registry (silent unregister).
        let home = tmp_home("registry-corrupt-install");
        install_builtin(&home, "tdd").unwrap();
        let path = registry_path(&home);
        let before = std::fs::read(&path).unwrap();
        std::fs::write(&path, "{\"skills\": {\"tdd\": ").unwrap();

        assert!(install_builtin(&home, "token-diet").is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{\"skills\": {\"tdd\": ",
            "a refused install must leave the corrupt file untouched for repair"
        );
        let _ = before;
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn save_registry_leaves_no_temp_file_and_never_tears() {
        // Atomic save: concurrent writers must never leave a torn
        // registry.json, and no temp file may be left behind.
        let home = tmp_home("registry-atomic");
        let registry = sample_registry("tdd");
        save_registry(&home, &registry).unwrap();
        assert!(load_registry(&home).unwrap().skills.contains_key("tdd"));

        let home = std::sync::Arc::new(home);
        let mut handles = vec![];
        for i in 0..8 {
            let home = std::sync::Arc::clone(&home);
            handles.push(std::thread::spawn(move || {
                let registry = sample_registry(&format!("skill-{i}"));
                save_registry(&home, &registry).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let loaded = load_registry(&home).expect("concurrent saves must not tear registry.json");
        assert_eq!(loaded.skills.len(), 1, "{loaded:?}");
        let leftovers: Vec<_> = std::fs::read_dir(skills_root(&home))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
        std::fs::remove_dir_all(&*home).ok();
    }

    #[test]
    fn load_registry_unreadable_file_is_an_error_not_an_empty_registry() {
        // Mutant-killer for the `NotFound` guard in load_registry
        // (issue #465 follow-up): the guard must only match a missing
        // file. If it were `true`, any read failure - a directory at
        // the registry path, a permission error - would silently load
        // as an empty registry, and the next install would unregister
        // every skill.
        let home = tmp_home("registry-unreadable");
        let path = registry_path(&home);
        std::fs::create_dir_all(&path).unwrap();
        let err = load_registry(&home)
            .expect_err("a directory at registry.json must error, not load as empty");
        assert!(
            err.to_string().contains("reading"),
            "error should name the unreadable file: {err}"
        );
        // A missing file is still a fresh home, not an error.
        std::fs::remove_dir(&path).unwrap();
        assert!(load_registry(&home).unwrap().skills.is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[tokio::test]
    async fn skill_list_json_errors_on_corrupt_registry_unless_quiet() {
        // Mutant-killer for the `if !cli.quiet` guard in skill_command
        // (`skill list --json`): without `--quiet` the registry must be
        // loaded, so a corrupt registry fails the command. If the guard
        // were flipped, the load would be skipped and the command would
        // succeed silently on a corrupt registry.
        use clap::Parser;
        let home = tmp_home("skill-list-json-corrupt");
        let cwd = tmp_home("skill-list-json-corrupt-cwd");
        std::fs::create_dir_all(skills_root(&home)).unwrap();
        std::fs::write(registry_path(&home), "not json at all").unwrap();
        let cli = crate::cli::Cli::parse_from(["zen", "--json", "skill", "list"]);
        let err = skill_command(&cli, &cwd, Some(&home), &crate::cli::SkillSubcommand::List)
            .await
            .expect_err("`skill list --json` must fail on a corrupt registry");
        assert!(
            err.to_string().contains("parsing"),
            "unexpected error: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[tokio::test]
    async fn skill_list_json_quiet_skips_corrupt_registry() {
        // Same guard, other direction: with `--quiet` the JSON list must
        // not load the registry at all, so a corrupt registry still
        // exits 0. If the guard were flipped, the load would run and fail.
        use clap::Parser;
        let home = tmp_home("skill-list-json-quiet-corrupt");
        let cwd = tmp_home("skill-list-json-quiet-corrupt-cwd");
        std::fs::create_dir_all(skills_root(&home)).unwrap();
        std::fs::write(registry_path(&home), "not json at all").unwrap();
        let cli = crate::cli::Cli::parse_from(["zen", "--quiet", "--json", "skill", "list"]);
        let code = skill_command(&cli, &cwd, Some(&home), &crate::cli::SkillSubcommand::List)
            .await
            .expect("`skill list --json --quiet` must succeed without loading the registry");
        assert_eq!(code, 0);
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&cwd).ok();
    }
}
