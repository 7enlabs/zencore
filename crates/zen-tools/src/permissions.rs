use crate::builtin::{canonicalize_lenient, resolve_path};
use globset::GlobBuilder;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use zen_core::lock_recovery::lock_or_recover;
use zen_core::{
    GateBucket, GateInfo, PermissionChecker, PermissionDecision, PermissionMode,
    PermissionSettings, ToolContext,
};

pub fn tool_is_read_only(name: &str) -> bool {
    matches!(
        name,
        "Read"
            | "Glob"
            | "Grep"
            | "TodoWrite"
            | "Task"
            | "delegate"
            | "Skill"
            // Browser agent (spec section 4): the four read-only tools.
            | crate::browser::BROWSER_NAVIGATE
            | crate::browser::BROWSER_CONTENT
            | crate::browser::BROWSER_SCREENSHOT
            | crate::browser::BROWSER_WAIT
    )
}

fn split_rule(rule: &str) -> (&str, Option<&str>) {
    let rule = rule.trim();
    match rule.find('(') {
        Some(i) if rule.ends_with(')') => (&rule[..i], Some(&rule[i + 1..rule.len() - 1])),
        _ => (rule, None),
    }
}

pub(crate) fn path_arg(input: &Value) -> Option<&str> {
    input
        .get("file_path")
        .or_else(|| input.get("path"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

fn rule_pattern_path(cwd: &Path, spec: &str) -> (PathBuf, bool) {
    let dir_hint = spec.ends_with('/') || spec.ends_with("/**");
    let spec = spec.trim_end_matches("/**").trim_end_matches('/');
    if let Some(rest) = spec.strip_prefix("//") {
        // The target side is canonicalized (see path_rule_matches),
        // so an absolute pattern must be too: on macOS /etc is a
        // symlink to /private/etc, and //etc/hosts would never meet
        // the canonical target /private/etc/hosts (issue #156).
        (
            canonicalize_lenient(&PathBuf::from(format!("/{}", rest))),
            dir_hint,
        )
    } else if let Some(rest) = spec.strip_prefix("~/") {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
        (home.join(rest), dir_hint)
    } else {
        // The target side of the match is canonicalized (see
        // path_rule_matches), so the pattern side must be built from
        // the canonical cwd too. A raw cwd spelled with an 8.3 short
        // name, a symlink, or `..` segments would never meet the
        // canonical target, and the rule would be silently skipped
        // (PR #38: a settings deny lost to a CLI allow on Windows).
        let cwd = canonicalize_lenient(cwd);
        if let Some(rest) = spec.strip_prefix("./").or_else(|| spec.strip_prefix('/')) {
            (cwd.join(rest), dir_hint)
        } else if !spec.contains('/') {
            (cwd.join("**").join(spec), dir_hint)
        } else {
            (cwd.join(spec), dir_hint)
        }
    }
}

fn drive_prefix_of(slash_path: &str) -> Option<&str> {
    let b = slash_path.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        Some(&slash_path[..2])
    } else {
        None
    }
}

fn has_drive_prefix(p: &Path) -> bool {
    matches!(p.components().next(), Some(std::path::Component::Prefix(_)))
}

fn path_rule_matches(cwd: &Path, spec: &str, candidate: &str) -> bool {
    let (pattern_path, dir_hint) = rule_pattern_path(cwd, spec);

    let target = canonicalize_lenient(&resolve_path(cwd, candidate));
    // Match in a uniform `/`-separated space. On Windows a pattern such as
    // `\work\proj\src/**` is mangled by globset, which treats `\` as an
    // escape, so no Windows pattern would ever match. On other platforms
    // the conversion is a no-op for ordinary paths.
    let slash = |p: &Path| {
        let s = p.to_string_lossy();
        if cfg!(windows) {
            s.replace('\\', "/")
        } else {
            s.into_owned()
        }
    };
    let pattern_slash = slash(&pattern_path);
    let target_slash = slash(&target);
    // A drive-less pattern cannot name a drive, so it must also match a
    // drive-qualified target by its drive-less remainder - but only when
    // the target sits on the same drive as the cwd (or the cwd itself is
    // drive-less), so a rule that names a drive keeps matching per-drive.
    // (canonicalize_lenient attaches the current drive to drive-less
    // targets on Windows, while patterns built from a synthetic cwd stay
    // drive-less.)
    let mut targets = vec![target_slash.clone()];
    if cfg!(windows) && !has_drive_prefix(&pattern_path) && drive_prefix_of(&target_slash).is_some()
    {
        let cwd_slash = slash(cwd);
        let cwd_drive = drive_prefix_of(&cwd_slash);
        let target_drive = drive_prefix_of(&target_slash);
        if cwd_drive.is_none() || cwd_drive == target_drive {
            targets.push(target_slash[2..].to_string());
        }
    }
    let mut globs = vec![pattern_slash.clone()];
    if dir_hint {
        globs.push(format!("{}/**", pattern_slash));
    }
    for pat in &globs {
        if let Ok(g) = GlobBuilder::new(pat).literal_separator(true).build() {
            let matcher = g.compile_matcher();
            for t in &targets {
                if matcher.is_match_candidate(&globset::Candidate::from_bytes(t.as_bytes())) {
                    return true;
                }
            }
        }
    }
    // Component-wise fallback on the same normalized forms.
    let pattern_pb = PathBuf::from(&pattern_slash);
    for t in &targets {
        let target_pb = PathBuf::from(t);
        if target_pb == pattern_pb || (dir_hint && target_pb.starts_with(&pattern_pb)) {
            return true;
        }
    }
    false
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn bash_rule_matches(spec: &str, command: &str) -> bool {
    let command = command.trim();
    let spec = spec.trim();
    let spec = match spec.strip_suffix(":*") {
        Some(p) => format!("{} *", p.trim_end()),
        None => spec.to_string(),
    };
    if spec == "*" {
        return true;
    }
    if let Some(prefix) = spec.strip_suffix(" *") {
        let prefix = prefix.trim_end();
        return wildcard_match(prefix, command)
            || wildcard_match(&format!("{} *", prefix), command);
    }
    wildcard_match(&spec, command)
}

pub fn split_compound(command: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = command.chars().collect();

    let mut segs: Vec<(String, &'static str)> = Vec::new();
    let mut cur = String::new();
    let (mut sq, mut dq) = (false, false);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '\\' && !sq {
            cur.push(c);
            if let Some(n) = next {
                cur.push(n);
            }
            i += 2;
            continue;
        }
        if c == '\'' && !dq {
            sq = !sq;
        } else if c == '"' && !sq {
            dq = !dq;
        } else if !sq && !dq {
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            let redirect = c == '&' && (matches!(prev, Some('>') | Some('<')) || next == Some('>'));
            let op: Option<(&'static str, usize)> = match (c, next) {
                _ if redirect => None,
                ('&', Some('&')) => Some(("&&", 2)),
                ('|', Some('|')) => Some(("||", 2)),
                ('|', Some('&')) => Some(("|&", 2)),
                ('|', _) => Some(("|", 1)),
                ('&', _) => Some(("&", 1)),
                (';', _) => Some((";", 1)),
                ('\n', _) => Some(("\n", 1)),
                _ => None,
            };
            if let Some((op, len)) = op {
                segs.push((std::mem::take(&mut cur), op));
                i += len;
                continue;
            }
        }
        cur.push(c);
        i += 1;
    }
    segs.push((cur, ""));
    let binary = |op: &str| matches!(op, "&&" | "||" | "|" | "|&");
    let mut out = Vec::new();
    for (idx, (seg, op_after)) in segs.iter().enumerate() {
        let seg = seg.trim();
        if seg.is_empty() {
            let op_before = if idx > 0 { segs[idx - 1].1 } else { "" };
            if binary(op_before) || binary(op_after) {
                return None;
            }
            continue;
        }
        out.push(seg.to_string());
    }
    Some(out)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RuleKind {
    Allow,
    Ask,
    Deny,
}

/// Whether the named tool edits files (M4: the ACP prompter maps it
/// to the `edit` tool kind in `session/request_permission`).
pub fn is_edit_tool(name: &str) -> bool {
    matches!(
        name,
        "Edit"
            | "Write"
            | "MultiEdit"
            | "NotebookEdit"
            // Browser agent (spec section 4.6): the two mutating tools,
            // so the ACP prompter maps them to the `edit` tool kind.
            | crate::browser::BROWSER_CLICK
            | crate::browser::BROWSER_TYPE
    )
}

pub fn rule_matches(rule: &str, tool_name: &str, input: &Value, cwd: &Path) -> bool {
    rule_matches_kind(rule, tool_name, input, cwd, RuleKind::Allow)
}

fn rule_matches_kind(
    rule: &str,
    tool_name: &str,
    input: &Value,
    cwd: &Path,
    kind: RuleKind,
) -> bool {
    let rule = rule.trim();
    if rule.is_empty() {
        return false;
    }
    if !rule.contains('(') {
        if rule.contains('*') && (rule.starts_with("mcp__") || kind != RuleKind::Allow) {
            return wildcard_match(rule, tool_name);
        }

        if rule.starts_with("mcp__") {
            return rule == tool_name || tool_name.starts_with(&format!("{}__", rule));
        }
    }

    let (rule_tool, spec) = split_rule(rule);
    let spec = spec.map(str::trim).filter(|s| !s.is_empty() && *s != "*");

    let name_ok = rule_tool.eq_ignore_ascii_case(tool_name)
        || (rule_tool == "Edit" && is_edit_tool(tool_name))
        || (kind == RuleKind::Deny
            && spec.is_some()
            && rule_tool == "Read"
            && (is_edit_tool(tool_name) || matches!(tool_name, "Grep" | "Glob")))
        // Browser agent (spec section 4.5): a rule may name either
        // spelling, so `BrowserClick` matches `browser_click`; and the
        // `BrowserLogin` class matches a login-gated click/type call.
        || (crate::browser::is_browser_tool(tool_name)
            && crate::browser::norm_tool_name(rule_tool)
                == crate::browser::norm_tool_name(tool_name))
        || (crate::browser::norm_tool_name(rule_tool) == "browserlogin"
            && browser_login_call(tool_name, input));
    if !name_ok {
        return false;
    }
    let Some(spec) = spec else { return true };
    match rule_tool {
        "Bash" | "bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            bash_rule_matches(spec, cmd)
        }
        t if rule_tool_is_browser(t) => {
            // Host-scoped browser rules (spec section 4.7): the host is
            // the input URL for navigate, the active session's page host
            // for the other tools.
            let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let host = if url.is_empty() {
                crate::browser::current_browser_host().unwrap_or_default()
            } else {
                host_of(url)
            };
            match spec.strip_prefix("domain:") {
                Some(domain) => host_matches_domain(domain, &host),
                None => {
                    if url.is_empty() {
                        host.contains(spec)
                    } else {
                        url.contains(spec)
                    }
                }
            }
        }
        t if t == "Read" || is_edit_tool(t) => match path_arg(input) {
            Some(fp) => path_rule_matches(cwd, spec, fp),
            None => false,
        },
        "WebFetch" => {
            let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
            match spec.strip_prefix("domain:") {
                Some(domain) => domain_matches(domain, url),
                None => url.contains(spec),
            }
        }
        _ => false,
    }
}

fn domain_matches(domain: &str, url: &str) -> bool {
    let domain = domain.trim().to_lowercase();
    let raw = host_raw(url);
    if raw.is_empty() {
        return false;
    }
    if domain == "*" {
        return true;
    }
    if let Some(base) = domain.strip_prefix("*.") {
        return raw.ends_with(&format!(".{}", base));
    }
    host_of(url) == domain.trim_start_matches("www.")
}

pub fn rule_removes_tool(rule: &str, tool_name: &str) -> bool {
    let rule = rule.trim();
    let (rule_tool, spec) = split_rule(rule);
    let tool_level = spec
        .map(|s| s.trim().is_empty() || s.trim() == "*")
        .unwrap_or(true);
    tool_level
        && rule_matches_kind(
            rule_tool,
            tool_name,
            &Value::Null,
            Path::new("/"),
            RuleKind::Deny,
        )
}

fn host_raw(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host_and_path = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = host_and_path
        .split('@')
        .next_back()
        .unwrap_or(host_and_path);
    let host = host.split(':').next().unwrap_or(host);
    host.to_lowercase()
}

fn host_of(url: &str) -> String {
    host_raw(url).trim_start_matches("www.").to_string()
}

fn is_simple_fs_command(command: &str) -> bool {
    match split_compound(command) {
        Some(parts) if !parts.is_empty() => parts.iter().all(|p| {
            matches!(
                p.split_whitespace().next().unwrap_or(""),
                "mkdir" | "touch" | "mv" | "cp"
            )
        }),
        _ => false,
    }
}

fn any_rule<'a>(
    rules: &'a [String],
    tool_name: &str,
    input: &Value,
    cwd: &Path,
    kind: RuleKind,
) -> Option<&'a String> {
    let subs: Vec<Value> = match bash_parts(tool_name, input) {
        Some(Some(parts)) if parts.len() > 1 => parts
            .into_iter()
            .map(|p| serde_json::json!({ "command": p }))
            .collect(),
        _ => Vec::new(),
    };
    rules.iter().find(|r| {
        rule_matches_kind(r, tool_name, input, cwd, kind)
            || subs
                .iter()
                .any(|sub| rule_matches_kind(r, tool_name, sub, cwd, kind))
    })
}

fn allow_covers(rules: &[String], tool_name: &str, input: &Value, cwd: &Path) -> bool {
    let tool_level = |r: &String| {
        let (_, spec) = split_rule(r.trim());
        spec.map(|s| s.trim().is_empty() || s.trim() == "*")
            .unwrap_or(true)
    };
    match bash_parts(tool_name, input) {
        None => rules
            .iter()
            .any(|r| rule_matches_kind(r, tool_name, input, cwd, RuleKind::Allow)),
        Some(None) => rules
            .iter()
            .any(|r| tool_level(r) && rule_matches_kind(r, tool_name, input, cwd, RuleKind::Allow)),
        Some(Some(parts)) if parts.len() <= 1 => rules
            .iter()
            .any(|r| rule_matches_kind(r, tool_name, input, cwd, RuleKind::Allow)),
        Some(Some(parts)) => parts.iter().all(|p| {
            let sub = serde_json::json!({ "command": p });
            rules
                .iter()
                .any(|r| rule_matches_kind(r, tool_name, &sub, cwd, RuleKind::Allow))
        }),
    }
}

fn substitution_bodies(command: &str) -> Vec<String> {
    fn scan(chars: &[char], out: &mut Vec<String>) {
        let (mut sq, mut dq) = (false, false);
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' && !sq {
                i += 2;
                continue;
            }
            if c == '\'' && !dq {
                sq = !sq;
            } else if c == '"' && !sq {
                dq = !dq;
            } else if !sq {
                if c == '$' && chars.get(i + 1) == Some(&'(') {
                    let mut depth = 1usize;
                    let mut j = i + 2;
                    let (mut jsq, mut jdq) = (false, false);
                    while j < chars.len() && depth > 0 {
                        let d = chars[j];
                        if d == '\\' && !jsq {
                            j += 2;
                            continue;
                        }
                        if d == '\'' && !jdq {
                            jsq = !jsq;
                        } else if d == '"' && !jsq {
                            jdq = !jdq;
                        } else if !jsq && !jdq {
                            if d == '(' {
                                depth += 1;
                            } else if d == ')' {
                                depth -= 1;
                            }
                        }
                        j += 1;
                    }
                    if depth == 0 {
                        let inner: String = chars[i + 2..j - 1].iter().collect();
                        out.push(inner.clone());
                        scan(&inner.chars().collect::<Vec<_>>(), out);
                        i = j;
                        continue;
                    }
                } else if c == '`' {
                    let mut j = i + 1;
                    while j < chars.len() {
                        if chars[j] == '\\' {
                            j += 2;
                            continue;
                        }
                        if chars[j] == '`' {
                            break;
                        }
                        j += 1;
                    }
                    if j < chars.len() {
                        let inner: String = chars[i + 1..j].iter().collect();
                        out.push(inner.clone());
                        scan(&inner.chars().collect::<Vec<_>>(), out);
                        i = j + 1;
                        continue;
                    }
                } else if c == '$' && chars.get(i + 1) == Some(&'{') {
                    let mut depth = 1usize;
                    let mut j = i + 2;
                    while j < chars.len() && depth > 0 {
                        match chars[j] {
                            '{' => depth += 1,
                            '}' => depth -= 1,
                            _ => {}
                        }
                        j += 1;
                    }
                    if depth == 0 {
                        let inner: String = chars[i + 2..j - 1].iter().collect();
                        scan(&inner.chars().collect::<Vec<_>>(), out);
                        i = j;
                        continue;
                    }
                }
            }
            i += 1;
        }
    }
    let mut out = Vec::new();
    scan(&command.chars().collect::<Vec<_>>(), &mut out);
    out
}

pub(crate) fn bash_parts(tool_name: &str, input: &Value) -> Option<Option<Vec<String>>> {
    if !tool_name.eq_ignore_ascii_case("Bash") {
        return None;
    }
    let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
    Some(match split_compound(cmd) {
        None => None,
        Some(parts) => {
            let mut seen: HashSet<String> = parts.iter().cloned().collect();
            let mut all = parts;

            for part in all.clone() {
                for body in substitution_bodies(&part) {
                    match split_compound(&body) {
                        Some(inner) if !inner.is_empty() => {
                            for frag in inner {
                                if seen.insert(frag.clone()) {
                                    all.push(frag);
                                }
                            }
                        }
                        _ => {
                            if seen.insert(body.clone()) {
                                all.push(body);
                            }
                        }
                    }
                }
            }
            Some(all)
        }
    })
}

pub struct RulePermissions {
    /// The mode decisions are evaluated under. A Mutex (not a plain
    /// field) so `session/set_mode` can change it through the shared
    /// `Arc<dyn PermissionChecker>` after construction (M4, Phase 2 #9).
    mode: Mutex<PermissionMode>,
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
    pub local_settings_path: Option<PathBuf>,
    /// Permissions classifier settings (Flightdeck Phase 2).
    pub classifier: zen_core::ClassifierSettings,

    dynamic_allow: Mutex<Vec<String>>,
}

impl RulePermissions {
    pub fn new(
        mode: PermissionMode,
        settings: &PermissionSettings,
        cli_allow: Vec<String>,
        cli_deny: Vec<String>,
        local_settings_path: Option<PathBuf>,
    ) -> Self {
        let mut allow = settings.allow.clone();
        allow.extend(cli_allow);
        let mut deny = settings.deny.clone();
        deny.extend(cli_deny);
        Self {
            mode: Mutex::new(mode),
            allow,
            ask: settings.ask.clone(),
            deny,
            local_settings_path,
            classifier: settings.classifier.clone(),
            dynamic_allow: Mutex::new(Vec::new()),
        }
    }

    /// The mode decisions are currently evaluated under (M4).
    pub fn current_mode(&self) -> PermissionMode {
        *lock_or_recover(&self.mode, "mode")
    }

    pub fn effective_allow(&self) -> Vec<String> {
        let mut v = self.allow.clone();
        v.extend(
            lock_or_recover(&self.dynamic_allow, "dynamic_allow")
                .iter()
                .cloned(),
        );
        v
    }

    fn working_dirs(&self, ctx: &ToolContext) -> Vec<PathBuf> {
        let mut dirs = vec![canonicalize_lenient(&ctx.cwd())];
        for d in &ctx.extra_dirs {
            dirs.push(canonicalize_lenient(d));
        }
        dirs
    }

    fn path_inside_workdir(&self, input: &Value, ctx: &ToolContext) -> bool {
        let Some(fp) = path_arg(input) else {
            return false;
        };
        let target = canonicalize_lenient(&resolve_path(&ctx.cwd(), fp));
        self.working_dirs(ctx).iter().any(|d| target.starts_with(d))
    }

    fn fs_args_inside_workdir(&self, command: &str, ctx: &ToolContext) -> bool {
        if command.contains(['`', '>', '<', '$']) {
            return false;
        }
        let cwd = ctx.cwd();
        let dirs = self.working_dirs(ctx);
        split_compound(command)
            .unwrap_or_default()
            .iter()
            .all(|part| {
                part.split_whitespace()
                    .skip(1)
                    // A `--opt=value` argument hides a path after the
                    // `=` (issue #322): `cp --target-directory=/etc`
                    // must not slip past as a plain flag. The value
                    // after the first `=` is checked as a path; a bare
                    // flag (`--verbose`, `-p`) carries no path.
                    .filter_map(|a| {
                        if let Some(rest) = a.strip_prefix('-')
                            && let Some(eq) = rest.find('=')
                        {
                            Some(&rest[eq + 1..])
                        } else if a.starts_with('-') {
                            None
                        } else {
                            Some(a)
                        }
                    })
                    .filter(|a| !a.is_empty())
                    .all(|arg| {
                        let arg = arg.trim_matches(|c| c == '"' || c == '\'');

                        let target = canonicalize_lenient(&resolve_path(&cwd, arg));
                        dirs.iter().any(|d| target.starts_with(d))
                    })
            })
    }

    fn evaluate(
        &self,
        tool_name: &str,
        input: &Value,
        read_only: bool,
        ctx: &ToolContext,
    ) -> PermissionDecision {
        self.evaluate_full(tool_name, input, read_only, ctx).0
    }

    /// Full gate evaluation with the browser audit log (spec section
    /// 4.8): every browser tool decision is logged with tool, host,
    /// decision, and mode. The log never includes typed text or
    /// cookies.
    fn evaluate_full(
        &self,
        tool_name: &str,
        input: &Value,
        read_only: bool,
        ctx: &ToolContext,
    ) -> (PermissionDecision, GateInfo) {
        let result = self.evaluate_full_inner(tool_name, input, read_only, ctx);
        if crate::browser::is_browser_tool(tool_name) {
            let decision = match &result.0 {
                PermissionDecision::Allow => "allow",
                PermissionDecision::Ask => "ask",
                PermissionDecision::Deny(_) => "deny",
            };
            let host = crate::browser::current_browser_host().unwrap_or_default();
            eprintln!(
                "browser permission: tool={tool_name} host={host} decision={decision} mode={} reason={}",
                self.current_mode().as_str(),
                result.1.reason_code
            );
        }
        result
    }

    /// The full gate evaluation (Flightdeck Phase 2): the decision
    /// plus the GATE bucket and reason code that produced it. The
    /// classifier verdict is computed once here and reused by both
    /// the dangerous hard-deny and the safe auto-allow below.
    fn evaluate_full_inner(
        &self,
        tool_name: &str,
        input: &Value,
        read_only: bool,
        ctx: &ToolContext,
    ) -> (PermissionDecision, GateInfo) {
        let cwd = ctx.cwd();
        let mode = self.current_mode();
        let allowed = allow_covers(&self.effective_allow(), tool_name, input, &cwd);
        let classification = if self.classifier.enabled {
            Some(crate::classifier::classify(
                tool_name,
                input,
                &cwd,
                &self.working_dirs(ctx),
            ))
        } else {
            None
        };
        // Reason code for an Ask that reaches the mode fallback: a
        // labelled mutation (warn.*, sudo escalation) keeps its label
        // so the approver sees why the call is flagged.
        let ask_code = match &classification {
            Some(c)
                if c.class == crate::classifier::RiskClass::Mutating
                    && c.reason_code != "mutating.default" =>
            {
                c.reason_code.to_string()
            }
            _ => format!("mode.{}", mode.as_str()),
        };

        // 1. Deny rules: the highest layer, unchanged.
        if let Some(rule) = any_rule(&self.deny, tool_name, input, &cwd, RuleKind::Deny) {
            return (
                PermissionDecision::Deny(format!("Denied by rule '{}'", rule)),
                GateInfo {
                    bucket: GateBucket::Denied,
                    reason_code: format!("rule:{rule}"),
                },
            );
        }

        // 2. Classifier hard-deny of dangerous calls (D2: an exact
        // allow rule still wins; bypass mode does NOT override this).
        if let Some(c) = &classification
            && c.class == crate::classifier::RiskClass::Dangerous
            && !allowed
        {
            return (
                PermissionDecision::Deny(format!(
                    "Blocked by classifier [{}]: {}",
                    c.reason_code, c.reason
                )),
                GateInfo {
                    bucket: GateBucket::Denied,
                    reason_code: c.reason_code.to_string(),
                },
            );
        }

        // 2b. Browser tools (spec section 4): the permission gate for
        // the six browser tools. Classification comes from the selector
        // text, the `submit` flag, and the cached page state; the mark
        // is recorded for the runtime re-check in `browser.rs`.
        // Login, submit, and destructive actions always need their own
        // approval: Ask directly, never via `resolve_ask` (its
        // BypassPermissions arm would turn the Ask into Allow).
        if crate::browser::is_browser_tool(tool_name) {
            // Rule 1: Plan mode blocks every browser tool, read-only
            // included.
            if mode == PermissionMode::Plan {
                return (
                    PermissionDecision::Deny("plan mode: browser tools are not available".into()),
                    GateInfo {
                        bucket: GateBucket::Denied,
                        reason_code: "mode.plan.browser".into(),
                    },
                );
            }
            if crate::browser::is_browser_mutating(tool_name) {
                let session_key = if ctx.session_id.is_empty() {
                    "default".to_string()
                } else {
                    ctx.session_id.clone()
                };
                let (mark, _) =
                    crate::browser::classify_browser_call(tool_name, input, &session_key);
                // Rule 9 + section 5: login, submit, and destructive
                // actions always need a separate approval, in every
                // mode, and no allow rule overrides this.
                if mark.login {
                    return (
                        PermissionDecision::Ask,
                        GateInfo {
                            bucket: GateBucket::Classifier,
                            reason_code: "browser.login".into(),
                        },
                    );
                }
                if mark.submit || mark.destructive {
                    return (
                        PermissionDecision::Ask,
                        GateInfo {
                            bucket: GateBucket::Classifier,
                            reason_code: "browser.submit".into(),
                        },
                    );
                }
                // Rule 4: BypassPermissions never auto-allows
                // click/type, even though it skips asking for other
                // tools. Read-only browser tools follow the general
                // gate below.
                if mode == PermissionMode::BypassPermissions {
                    return (
                        PermissionDecision::Ask,
                        GateInfo {
                            bucket: GateBucket::Classifier,
                            reason_code: "browser.mutating".into(),
                        },
                    );
                }
            }
        }

        // 3. Plan mode: only read-only tools run.
        if mode == PermissionMode::Plan && !read_only {
            return (
                PermissionDecision::Deny("plan mode: only read-only tools are available".into()),
                GateInfo {
                    bucket: GateBucket::Denied,
                    reason_code: "mode.plan".into(),
                },
            );
        }

        // 4. Ask rules.
        if let Some(rule) = any_rule(&self.ask, tool_name, input, &cwd, RuleKind::Ask) {
            let decision = self.resolve_ask(format!("Ask rule '{}' matches", rule));
            let info = match &decision {
                PermissionDecision::Allow => GateInfo {
                    bucket: GateBucket::Allowed,
                    reason_code: "mode.bypassPermissions".into(),
                },
                PermissionDecision::Deny(_) => GateInfo {
                    bucket: GateBucket::Denied,
                    reason_code: format!("mode.{}", mode.as_str()),
                },
                PermissionDecision::Ask => GateInfo {
                    bucket: GateBucket::Classifier,
                    reason_code: format!("rule:{rule}"),
                },
            };
            return (decision, info);
        }

        // 5+6. Allow rules, then bypass mode.
        if mode == PermissionMode::BypassPermissions {
            return (
                PermissionDecision::Allow,
                GateInfo {
                    bucket: GateBucket::Allowed,
                    reason_code: "mode.bypassPermissions".into(),
                },
            );
        }

        // askAll: an allow rule never self-allows, and a read-only tool
        // never auto-allows. Everything past the deny/ask rules above
        // falls through to the mode match, which asks.
        if allowed
            && mode != PermissionMode::AskAll
            && !(mode == PermissionMode::DontAsk && crate::browser::is_browser_mutating(tool_name))
        {
            return (
                PermissionDecision::Allow,
                GateInfo {
                    bucket: GateBucket::Allowed,
                    reason_code: "rule.allow".into(),
                },
            );
        }

        // 7. Read-only tools, then classifier-Safe auto-allow (D3:
        // only when safeAutoAllow is switched on in settings).
        if (read_only || tool_is_read_only(tool_name)) && mode != PermissionMode::AskAll {
            return (
                PermissionDecision::Allow,
                GateInfo {
                    bucket: GateBucket::Allowed,
                    reason_code: "safe.read-only".into(),
                },
            );
        }
        if let Some(c) = &classification
            && c.class == crate::classifier::RiskClass::Safe
            && self.classifier.safe_auto_allow
            && mode != PermissionMode::AskAll
        {
            return (
                PermissionDecision::Allow,
                GateInfo {
                    bucket: GateBucket::Allowed,
                    reason_code: c.reason_code.to_string(),
                },
            );
        }

        // 8. Mode fallback.
        match mode {
            PermissionMode::AcceptEdits => {
                // Browser agent (spec section 4, rule 3): AcceptEdits
                // only waives asking for file edits, never for browser
                // click/type, which still ask.
                let is_edit =
                    is_edit_tool(tool_name) && !crate::browser::is_browser_mutating(tool_name);
                if is_edit && self.path_inside_workdir(input, ctx) {
                    return (
                        PermissionDecision::Allow,
                        GateInfo {
                            bucket: GateBucket::Allowed,
                            reason_code: "mode.acceptEdits".into(),
                        },
                    );
                }
                if tool_name == "Bash" {
                    let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
                    if is_simple_fs_command(cmd) && self.fs_args_inside_workdir(cmd, ctx) {
                        return (
                            PermissionDecision::Allow,
                            GateInfo {
                                bucket: GateBucket::Allowed,
                                reason_code: "mode.acceptEdits".into(),
                            },
                        );
                    }
                }
                (
                    PermissionDecision::Ask,
                    GateInfo {
                        bucket: GateBucket::Classifier,
                        reason_code: ask_code,
                    },
                )
            }
            PermissionMode::DontAsk => (
                PermissionDecision::Deny("dontAsk mode: permission request suppressed".into()),
                GateInfo {
                    bucket: GateBucket::Denied,
                    reason_code: "mode.dontAsk".into(),
                },
            ),
            PermissionMode::AskAll => (
                PermissionDecision::Ask,
                GateInfo {
                    bucket: GateBucket::Classifier,
                    reason_code: ask_code,
                },
            ),
            PermissionMode::Default | PermissionMode::Plan | PermissionMode::BypassPermissions => (
                PermissionDecision::Ask,
                GateInfo {
                    bucket: GateBucket::Classifier,
                    reason_code: ask_code,
                },
            ),
        }
    }

    fn resolve_ask(&self, reason: String) -> PermissionDecision {
        match self.current_mode() {
            PermissionMode::DontAsk => {
                PermissionDecision::Deny(format!("{} (dontAsk mode denies prompts)", reason))
            }

            PermissionMode::Plan => PermissionDecision::Deny("plan mode".into()),
            PermissionMode::BypassPermissions => PermissionDecision::Allow,
            _ => PermissionDecision::Ask,
        }
    }
}

impl PermissionChecker for RulePermissions {
    fn check(
        &self,
        tool_name: &str,
        input: &Value,
        read_only: bool,
        ctx: &ToolContext,
    ) -> PermissionDecision {
        self.evaluate(tool_name, input, read_only, ctx)
    }

    fn gate_decision(
        &self,
        tool_name: &str,
        input: &Value,
        read_only: bool,
        ctx: &ToolContext,
    ) -> (PermissionDecision, GateInfo) {
        self.evaluate_full(tool_name, input, read_only, ctx)
    }

    fn ask_rule_matches(&self, tool_name: &str, input: &Value, ctx: &ToolContext) -> bool {
        any_rule(&self.ask, tool_name, input, &ctx.cwd(), RuleKind::Ask).is_some()
    }

    fn set_mode(&self, mode: PermissionMode) {
        *lock_or_recover(&self.mode, "mode") = mode;
    }

    fn add_allow_rule(&self, rule: &str) {
        let rule = rule.trim();
        if rule.is_empty() {
            return;
        }
        {
            let mut dyn_rules = lock_or_recover(&self.dynamic_allow, "dynamic_allow");
            if !dyn_rules.iter().any(|r| r == rule) {
                dyn_rules.push(rule.to_string());
            }
        }
        if let Some(path) = &self.local_settings_path
            && let Err(e) = persist_allow_rule(path, rule)
        {
            eprintln!(
                "warning: could not save rule '{}' to {}: {}",
                rule,
                path.display(),
                e
            );
        }
    }
}

pub fn persist_allow_rule(path: &Path, rule: &str) -> anyhow::Result<()> {
    let mut root: Value = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => {
            serde_json::from_str(&text).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        _ => Value::Object(Default::default()),
    };
    if !root.is_object() {
        root = Value::Object(Default::default());
    }
    let obj = root
        .as_object_mut()
        .expect("root is an object: coerced to one above");
    let perms = obj
        .entry("permissions")
        .or_insert_with(|| Value::Object(Default::default()));
    if !perms.is_object() {
        *perms = Value::Object(Default::default());
    }
    let perms = perms
        .as_object_mut()
        .expect("permissions entry is an object: coerced to one above");
    let allow = perms.entry("allow").or_insert_with(|| Value::Array(vec![]));
    if !allow.is_array() {
        *allow = Value::Array(vec![]);
    }
    let arr = allow
        .as_array_mut()
        .expect("allow entry is an array: coerced to one above");
    let already = arr.iter().any(|v| v.as_str() == Some(rule));
    if !already {
        arr.push(Value::String(rule.to_string()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&root)?)?;
    Ok(())
}

/// Whether this browser call is a login call (spec section 5): typing
/// into a password field, or clicking submit on a form that holds a
/// password field. Used for `BrowserLogin` rules. Mirrors the gate
/// classification in `browser.rs`; with no input and no active session
/// it is conservatively false.
/// Whether a rule's tool name refers to a browser tool (either
/// spelling) or to the `BrowserLogin` class (spec section 4.5).
/// `norm_tool_name` drops `_`, so the comparison normalises both sides
/// (via the tool constants, so a rename stays correct) instead of
/// matching the canonical spelling only.
fn rule_tool_is_browser(rule_tool: &str) -> bool {
    let norm = crate::browser::norm_tool_name(rule_tool);
    [
        crate::browser::BROWSER_NAVIGATE,
        crate::browser::BROWSER_CLICK,
        crate::browser::BROWSER_TYPE,
        crate::browser::BROWSER_CONTENT,
        crate::browser::BROWSER_SCREENSHOT,
        crate::browser::BROWSER_WAIT,
    ]
    .iter()
    .any(|c| crate::browser::norm_tool_name(c) == norm)
        || norm == "browserlogin"
}

fn browser_login_call(tool_name: &str, input: &Value) -> bool {
    if !crate::browser::is_browser_mutating(tool_name) {
        return false;
    }
    crate::browser::classify_current_browser_call(tool_name, input).1
}

/// Host-vs-domain match, shared by browser rules (spec section 4.7).
/// `*.example.com` matches subdomains; a bare domain matches exactly
/// (ignoring a `www.` prefix); `*` matches any non-empty host.
fn host_matches_domain(domain: &str, host: &str) -> bool {
    let domain = domain.trim().to_lowercase();
    let host = host.trim().to_lowercase();
    if host.is_empty() {
        return false;
    }
    if domain == "*" {
        return true;
    }
    if let Some(base) = domain.strip_prefix("*.") {
        return host.ends_with(&format!(".{base}"));
    }
    host.trim_start_matches("www.") == domain.trim_start_matches("www.")
}

pub fn suggested_rule(tool_name: &str, input: &Value) -> Option<String> {
    match tool_name {
        "Bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| format!("Bash({})", c)),
        "Read" | "Edit" | "MultiEdit" | "Write" => {
            path_arg(input).map(|p| format!("{}({})", tool_name, p))
        }
        "WebFetch" => input.get("url").and_then(|v| v.as_str()).and_then(|u| {
            let h = host_of(u);
            if h.is_empty() {
                None
            } else {
                Some(format!("WebFetch(domain:{})", h))
            }
        }),
        // Browser agent (spec section 4.5): suggest the rule class the
        // user can write. Login calls suggest `BrowserLogin`; other
        // click/type calls suggest a host-scoped rule when the active
        // session's host is known.
        "browser_navigate" => input.get("url").and_then(|v| v.as_str()).map(|u| {
            let h = host_of(u);
            if h.is_empty() {
                "browser_navigate".to_string()
            } else {
                format!("browser_navigate(domain:{h})")
            }
        }),
        "browser_click" | "browser_type" => {
            if browser_login_call(tool_name, input) {
                Some("BrowserLogin".to_string())
            } else if let Some(h) = crate::browser::current_browser_host().filter(|h| !h.is_empty())
            {
                Some(format!("{tool_name}(domain:{h})"))
            } else {
                Some(tool_name.to_string())
            }
        }
        other => Some(other.to_string()),
    }
}

pub fn suggested_rules(tool_name: &str, input: &Value) -> Vec<String> {
    if tool_name == "Bash" {
        let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(parts) = split_compound(cmd)
            && parts.len() > 1
        {
            return dedup(parts.into_iter().map(|p| format!("Bash({})", p)).collect())
                .into_iter()
                .take(5)
                .collect();
        }
    }
    suggested_rule(tool_name, input).into_iter().collect()
}

pub fn dedup(mut rules: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    rules.retain(|r| seen.insert(r.clone()));
    rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use zen_core::PermissionMode;

    fn test_perms(deny: &[&str], allow: &[&str]) -> RulePermissions {
        RulePermissions::new(
            PermissionMode::Default,
            &PermissionSettings {
                deny: deny.iter().map(|s| s.to_string()).collect(),
                allow: allow.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            vec![],
            vec![],
            None,
        )
    }

    fn check_bash(perms: &RulePermissions, cmd: &str) -> PermissionDecision {
        let ctx = ToolContext::new(std::path::PathBuf::from("/tmp"), "test");
        perms.check("Bash", &serde_json::json!({ "command": cmd }), false, &ctx)
    }

    fn is_deny(d: &PermissionDecision) -> bool {
        matches!(d, PermissionDecision::Deny(_))
    }

    #[test]
    fn substitution_deny_bypass_blocked() {
        let perms = test_perms(&["Bash(rm *)"], &["Bash(echo *)"]);
        assert!(
            is_deny(&check_bash(&perms, "echo $(rm -rf ~)")),
            "$() bypass not blocked"
        );
        assert!(
            is_deny(&check_bash(&perms, "echo `rm -rf ~`")),
            "backtick bypass not blocked"
        );
        assert!(
            is_deny(&check_bash(&perms, "echo $(echo $(rm -rf ~))")),
            "nested bypass not blocked"
        );
        assert!(
            is_deny(&check_bash(&perms, "echo ${X:-$(rm -rf ~)}")),
            "${{}} bypass not blocked"
        );
        assert!(is_deny(&check_bash(&perms, "rm -rf /tmp/x")));
    }

    #[test]
    fn substitution_allow_fails_closed() {
        let perms = test_perms(&[], &["Bash(echo *)", "Bash(date *)"]);
        assert!(matches!(
            check_bash(&perms, "echo $(date)"),
            PermissionDecision::Allow
        ));
        let perms2 = test_perms(&[], &["Bash(echo *)"]);
        assert!(matches!(
            check_bash(&perms2, "echo $(date)"),
            PermissionDecision::Ask
        ));
    }

    #[test]
    fn substitution_inside_double_quotes_blocked() {
        let perms = test_perms(&["Bash(rm *)"], &["Bash(echo *)"]);
        assert!(
            is_deny(&check_bash(&perms, "echo \"$(rm -rf ~)\"")),
            "dq $() bypass not blocked"
        );
        assert!(
            is_deny(&check_bash(&perms, "echo \"x `rm -rf ~` y\"")),
            "dq backtick bypass not blocked"
        );
        assert!(
            is_deny(&check_bash(&perms, "echo \"${X:-$(rm -rf ~)}\"")),
            "dq ${{}} bypass not blocked"
        );
    }

    #[test]
    fn substitution_single_quoted_not_extracted() {
        let perms = test_perms(&["Bash(rm *)"], &[]);
        assert!(!is_deny(&check_bash(&perms, "echo '$(rm -rf ~)'")));
        assert!(substitution_bodies("echo '$(rm)'").is_empty());
    }

    #[test]
    fn substitution_bodies_extraction() {
        assert_eq!(substitution_bodies("echo $(rm -rf ~)"), vec!["rm -rf ~"]);
        assert_eq!(substitution_bodies("echo `rm -rf ~`"), vec!["rm -rf ~"]);
        assert_eq!(substitution_bodies("echo hi"), Vec::<String>::new());
        let nested = substitution_bodies("a $(b $(c)) d");
        assert!(nested.contains(&"b $(c)".to_string()));
        assert!(nested.contains(&"c".to_string()));
        assert!(substitution_bodies("echo $(oops").is_empty());
    }

    #[test]
    fn compound_without_substitution_unchanged() {
        let perms = test_perms(&[], &["Bash(echo *)", "Bash(ls *)"]);
        assert!(matches!(
            check_bash(&perms, "echo hi && ls /tmp"),
            PermissionDecision::Allow
        ));
        // Phase 2: the classifier hard-denies the dangerous part of
        // a compound even when the other parts are allow-ruled.
        assert!(matches!(
            check_bash(&perms, "echo hi && rm -rf /"),
            PermissionDecision::Deny(_)
        ));
    }

    #[test]
    fn symlink_inside_workdir_blocked() {
        let dir = std::env::temp_dir().join(format!("zen-perm-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work")).unwrap();
        #[cfg(unix)]
        {
            std::fs::create_dir_all(dir.join("outside")).unwrap();
            std::os::unix::fs::symlink(dir.join("outside"), dir.join("work").join("link")).unwrap();
        }
        let perms = test_perms(&[], &[]);
        let ctx = ToolContext::new(dir.join("work"), "test");
        let ok = serde_json::json!({ "file_path": "sub/file.txt" });
        assert!(perms.path_inside_workdir(&ok, &ctx));
        #[cfg(unix)]
        {
            let evil = serde_json::json!({ "file_path": "link/secret.txt" });
            assert!(
                !perms.path_inside_workdir(&evil, &ctx),
                "symlink escape passed the workdir check"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_parsing() {
        assert_eq!(host_of("https://www.example.com/a/b?x=1"), "example.com");
        assert_eq!(
            host_of("http://user:pw@api.example.com:8443/x"),
            "api.example.com"
        );
        assert_eq!(host_of("example.com/x"), "example.com");
    }

    #[test]
    fn simple_fs_commands() {
        assert!(is_simple_fs_command("mkdir -p a/b"));
        assert!(is_simple_fs_command("touch x.txt"));
        assert!(!is_simple_fs_command("rm -rf /"));
        assert!(!is_simple_fs_command("cargo test"));
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;
    use serde_json::json;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zen-perm-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx_for(dir: &Path) -> ToolContext {
        ToolContext::new(dir.to_path_buf(), "perm-session")
    }

    fn perms(
        mode: PermissionMode,
        allow: &[&str],
        ask: &[&str],
        deny: &[&str],
        local: Option<PathBuf>,
    ) -> RulePermissions {
        let settings = PermissionSettings {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            ask: ask.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            default_mode: None,
            additional_directories: vec![],
            sandbox_mode: None,
            classifier: Default::default(),
        };
        RulePermissions::new(mode, &settings, vec![], vec![], local)
    }

    fn is_allow(d: &PermissionDecision) -> bool {
        matches!(d, PermissionDecision::Allow)
    }
    fn is_ask(d: &PermissionDecision) -> bool {
        matches!(d, PermissionDecision::Ask)
    }
    fn deny_reason(d: &PermissionDecision) -> String {
        match d {
            PermissionDecision::Deny(r) => r.clone(),
            other => panic!("expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn rule_bare_tool_names() {
        let cwd = PathBuf::from("/tmp/work");
        assert!(rule_matches(
            "Bash",
            "Bash",
            &json!({"command": "ls"}),
            &cwd
        ));
        assert!(rule_matches(
            "Read",
            "Read",
            &json!({"file_path": "x"}),
            &cwd
        ));
        assert!(!rule_matches(
            "Read",
            "Write",
            &json!({"file_path": "x"}),
            &cwd
        ));
        assert!(rule_matches(
            "Bash(*)",
            "Bash",
            &json!({"command": "anything"}),
            &cwd
        ));
        assert!(rule_matches(
            "mcp__files",
            "mcp__files__read",
            &json!({}),
            &cwd
        ));
        assert!(rule_matches("mcp__files", "mcp__files", &json!({}), &cwd));
        assert!(!rule_matches(
            "mcp__files",
            "mcp__other__read",
            &json!({}),
            &cwd
        ));
        assert!(rule_matches(
            "mcp__files__read",
            "mcp__files__read",
            &json!({}),
            &cwd
        ));
        assert!(!rule_matches(
            "mcp__files__read",
            "mcp__files__write",
            &json!({}),
            &cwd
        ));
    }

    #[test]
    fn rule_bash_exact_prefix_and_space_prefix() {
        let cwd = PathBuf::from("/tmp/work");
        let cmd = |c: &str| json!({"command": c});

        assert!(rule_matches(
            "Bash(npm run test)",
            "Bash",
            &cmd("npm run test"),
            &cwd
        ));
        assert!(!rule_matches(
            "Bash(npm run test)",
            "Bash",
            &cmd("npm run test --watch"),
            &cwd
        ));

        assert!(rule_matches(
            "Bash(git diff *)",
            "Bash",
            &cmd("git diff HEAD"),
            &cwd
        ));
        assert!(rule_matches(
            "Bash(git diff *)",
            "Bash",
            &cmd("git diff"),
            &cwd
        ));
        assert!(!rule_matches(
            "Bash(git diff *)",
            "Bash",
            &cmd("git diff-index"),
            &cwd
        ));

        assert!(rule_matches(
            "Bash(git diff*)",
            "Bash",
            &cmd("git diff-index --cached"),
            &cwd
        ));
        assert!(rule_matches(
            "Bash(git diff*)",
            "Bash",
            &cmd("git diff"),
            &cwd
        ));
        assert!(!rule_matches(
            "Bash(git diff*)",
            "Bash",
            &cmd("git status"),
            &cwd
        ));
    }

    #[test]
    fn rule_path_globs_relative_absolute_and_home() {
        let cwd = PathBuf::from("/work/proj");
        let f = |p: &str| json!({"file_path": p});
        assert!(rule_matches("Read(./.env)", "Read", &f(".env"), &cwd));
        assert!(rule_matches("Read(./.env)", "Read", &f("./.env"), &cwd));
        assert!(!rule_matches("Read(./.env)", "Read", &f("src/.env"), &cwd));
        assert!(rule_matches(
            "Edit(src/**)",
            "Edit",
            &f("src/main.rs"),
            &cwd
        ));
        assert!(rule_matches(
            "Edit(src/**)",
            "Edit",
            &f("src/deep/a/b.rs"),
            &cwd
        ));
        assert!(!rule_matches(
            "Edit(src/**)",
            "Edit",
            &f("tests/a.rs"),
            &cwd
        ));
        assert!(rule_matches(
            "Write(//etc/hosts)",
            "Write",
            &f("/etc/hosts"),
            &cwd
        ));

        assert!(!rule_matches(
            "Write(//etc/hosts)",
            "Write",
            &f("etc/hosts"),
            &cwd
        ));
        assert!(rule_matches(
            "Read(~/notes.txt)",
            "Read",
            &f("~/notes.txt"),
            &cwd
        ));
        assert!(!rule_matches(
            "Read(src/**, tests/**)",
            "Read",
            &f("src/x"),
            &cwd
        ));
    }

    #[test]
    #[cfg(unix)]
    fn absolute_rule_matches_through_a_symlinked_parent() {
        // Issue #156: on macOS /etc is a symlink to /private/etc, so
        // a //etc/hosts rule pattern must be canonicalized like the
        // target side. Reproduce the mechanism with a symlinked temp
        // dir (Linux has no /etc symlink).
        let root = tmpdir("link");
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("hosts"), "x").unwrap();
        std::os::unix::fs::symlink(&real, root.join("link")).unwrap();
        let cwd = PathBuf::from("/work/proj");
        let f = |p: &str| json!({"file_path": p});
        let rule = format!("Write(//{}/hosts)", root.join("link").display());
        let candidate = format!("/{}/hosts", root.join("link").display());
        assert!(rule_matches(&rule, "Write", &f(&candidate), &cwd));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn path_rules_match_when_cwd_spelling_is_not_canonical() {
        // Regression (PR #38 windows-check): the pattern side was built
        // from the raw cwd while the target side is canonicalized, so
        // when the cwd spelling was not the canonical one (a `..`
        // segment, a symlink, or an 8.3 short name on Windows) the glob
        // never met the target and a path deny rule was silently
        // skipped - settings deny lost to a CLI allow.
        let dir = tmpdir("noncanon");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("test.txt"), "x").unwrap();
        let alias = dir.join("sub").join("..");
        assert!(
            path_rule_matches(&alias, "test.txt", "test.txt"),
            "deny pattern built from a non-canonical cwd must still match"
        );
        assert!(path_rule_matches(&alias, "./test.txt", "test.txt"));
        // A different file must still not match.
        assert!(!path_rule_matches(&alias, "other.txt", "test.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn path_rules_match_with_windows_short_path_cwd() {
        // Regression (PR #38): on the GitHub Windows runner the process
        // cwd is spelled with an 8.3 short name (e.g. RUNNER~1) while
        // targets canonicalize to the long name (runneradmin), so deny
        // rules built from the raw cwd never matched.
        use std::os::windows::ffi::OsStrExt;
        unsafe extern "system" {
            fn GetShortPathNameW(long: *const u16, short: *mut u16, len: u32) -> u32;
        }
        let dir = tmpdir("short83");
        let long_dir = dir.join("longdirectoryname");
        std::fs::create_dir_all(&long_dir).unwrap();
        std::fs::write(long_dir.join("test.txt"), "x").unwrap();
        let wide: Vec<u16> = long_dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut buf = vec![0u16; 1024];
        let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        assert!(n > 0, "GetShortPathNameW failed");
        let short = String::from_utf16_lossy(&buf[..n as usize]);
        if short == long_dir.to_string_lossy() {
            // 8.3 name generation is disabled on this machine; the
            // non-canonical-spelling test above covers the mechanism.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert!(
            path_rule_matches(Path::new(&short), "test.txt", "test.txt"),
            "deny pattern built from the 8.3 cwd {short} must match the canonical target"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rule_webfetch_domain() {
        let cwd = PathBuf::from("/work");
        let u = |s: &str| json!({"url": s});
        assert!(rule_matches(
            "WebFetch(domain:example.com)",
            "WebFetch",
            &u("https://www.example.com/page"),
            &cwd
        ));

        assert!(!rule_matches(
            "WebFetch(domain:example.com)",
            "WebFetch",
            &u("https://api.example.com/v1"),
            &cwd
        ));
        assert!(!rule_matches(
            "WebFetch(domain:example.com)",
            "WebFetch",
            &u("https://notexample.com/"),
            &cwd
        ));
        assert!(!rule_matches(
            "WebFetch(domain:example.com)",
            "WebFetch",
            &u("https://example.org/"),
            &cwd
        ));
    }

    #[test]
    fn rule_empty_and_unknown_forms_do_not_match() {
        let cwd = PathBuf::from("/work");
        assert!(!rule_matches("", "Bash", &json!({"command": "ls"}), &cwd));
        assert!(!rule_matches(
            "Bash",
            "Read",
            &json!({"file_path": "x"}),
            &cwd
        ));
        assert!(!rule_matches("Bash(ls)", "Bash", &json!({}), &cwd));
    }

    #[test]
    fn default_mode_readonly_allowed_mutating_asks() {
        let dir = tmpdir("default");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::Default, &[], &[], &[], None);
        assert!(is_allow(&p.check(
            "Read",
            &json!({"file_path": "a"}),
            true,
            &ctx
        )));
        assert!(is_allow(&p.check(
            "Grep",
            &json!({"pattern": "x"}),
            true,
            &ctx
        )));
        assert!(is_ask(&p.check(
            "Write",
            &json!({"file_path": "a"}),
            false,
            &ctx
        )));
        assert!(is_ask(&p.check(
            "Bash",
            &json!({"command": "ls"}),
            false,
            &ctx
        )));
        assert!(is_ask(&p.check(
            "WebFetch",
            &json!({"url": "https://x.test"}),
            false,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn accept_edits_mode() {
        let dir = tmpdir("accept");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::AcceptEdits, &[], &[], &[], None);
        assert!(is_allow(&p.check(
            "Write",
            &json!({"file_path": "src/a.rs"}),
            false,
            &ctx
        )));
        assert!(is_allow(&p.check(
            "Edit",
            &json!({"file_path": "./a.rs"}),
            false,
            &ctx
        )));
        assert!(is_allow(&p.check(
            "Bash",
            &json!({"command": "mkdir -p a/b"}),
            false,
            &ctx
        )));
        assert!(is_allow(&p.check(
            "Bash",
            &json!({"command": "cp a b"}),
            false,
            &ctx
        )));
        // Phase 2: dangerous commands are hard-denied by the
        // classifier in every mode, acceptEdits included.
        assert!(matches!(
            p.check("Bash", &json!({"command": "rm -rf /"}), false, &ctx),
            PermissionDecision::Deny(_)
        ));
        assert!(is_ask(&p.check(
            "Bash",
            &json!({"command": "cargo test"}),
            false,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #322: `--opt=value` must not smuggle a path outside
    /// the workdir past the AcceptEdits auto-allow.
    #[test]
    fn accept_edits_rejects_target_directory_equals_escape() {
        let dir = tmpdir("accept-target-dir");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::AcceptEdits, &[], &[], &[], None);
        let bash = |cmd: &str| p.check("Bash", &json!({"command": cmd}), false, &ctx);
        // The `=` form escapes the old flag filter: /etc/cron.d is
        // outside the workdir, so this must ask, not auto-allow.
        assert!(is_ask(&bash("cp --target-directory=/etc/cron.d payload")));
        assert!(is_ask(&bash("mv --target-directory=/etc/cron.d payload")));
        // The space-separated form was already caught.
        assert!(is_ask(&bash("cp -t /etc/cron.d payload")));
        // An in-workdir target still auto-allows.
        assert!(is_allow(&bash("cp --target-directory=dest payload")));
        assert!(is_allow(&bash("cp --target-directory=sub/dir payload")));
        // A non-path `--opt=value` flag stays auto-allowed.
        assert!(is_allow(&bash("cp --color=always a b")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M4: `set_mode` moves the checker between modes, so a session
    /// that switches mode mid-conversation gets the new mode's
    /// decisions from the next tool call on.
    #[test]
    fn set_mode_changes_permission_decisions() {
        let dir = tmpdir("m4-set-mode");
        let ctx = ctx_for(&dir);
        let perms = perms(PermissionMode::Default, &[], &[], &[], None);
        let edit = serde_json::json!({"file_path": "x.rs"});
        // Default mode asks for a non-read-only edit tool.
        assert!(is_ask(&perms.check("Edit", &edit, false, &ctx)));
        // Plan mode denies it outright (read-only tools only).
        perms.set_mode(PermissionMode::Plan);
        assert_eq!(perms.current_mode(), PermissionMode::Plan);
        assert!(matches!(
            perms.check("Edit", &edit, false, &ctx),
            PermissionDecision::Deny(_)
        ));
        // AcceptEdits allows an edit inside the workdir...
        perms.set_mode(PermissionMode::AcceptEdits);
        assert!(is_allow(&perms.check("Edit", &edit, false, &ctx)));
        // ...and still asks for a shell command that is not a simple
        // filesystem command.
        let bash = serde_json::json!({"command": "cargo test"});
        assert!(is_ask(&perms.check("Bash", &bash, false, &ctx)));
        // AskAll asks even for a read-only tool.
        perms.set_mode(PermissionMode::AskAll);
        assert_eq!(perms.current_mode(), PermissionMode::AskAll);
        assert!(is_ask(&perms.check(
            "Read",
            &serde_json::json!({"file_path": "x.rs"}),
            true,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn accept_edits_stays_inside_working_dirs() {
        let dir = tmpdir("accept-outside");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::AcceptEdits, &[], &[], &[], None);
        assert!(is_ask(&p.check(
            "Write",
            &json!({"file_path": "/definitely/outside/file.txt"}),
            false,
            &ctx
        )));

        let mut ctx2 = ctx_for(&dir);
        let extra = tmpdir("accept-extra");
        ctx2.extra_dirs = vec![extra.clone()];
        let target = extra.join("ok.txt");
        assert!(is_allow(&p.check(
            "Write",
            &json!({"file_path": target.to_string_lossy()}),
            false,
            &ctx2
        )));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&extra);
    }

    #[test]
    fn plan_mode_denies_all_mutating_tools() {
        let dir = tmpdir("plan");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::Plan, &[], &[], &[], None);
        assert!(is_allow(&p.check(
            "Read",
            &json!({"file_path": "a"}),
            true,
            &ctx
        )));
        for (tool, input) in [
            ("Write", json!({"file_path": "a", "content": "x"})),
            ("Edit", json!({"file_path": "a"})),
            ("Bash", json!({"command": "ls"})),
            ("WebFetch", json!({"url": "https://x.test"})),
        ] {
            let d = p.check(tool, &input, false, &ctx);
            assert_eq!(
                deny_reason(&d),
                "plan mode: only read-only tools are available"
            );
        }

        let p2 = perms(PermissionMode::Plan, &["Write"], &[], &[], None);
        assert!(matches!(
            p2.check("Write", &json!({"file_path": "a"}), false, &ctx),
            PermissionDecision::Deny(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bypass_permissions_allows_unless_denied() {
        let dir = tmpdir("bypass");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::BypassPermissions, &[], &[], &[], None);
        // Phase 2 (D2): bypass does NOT override the classifier's
        // hard deny of dangerous commands.
        assert!(matches!(
            p.check("Bash", &json!({"command": "rm -rf /"}), false, &ctx),
            PermissionDecision::Deny(_)
        ));
        assert!(is_allow(&p.check(
            "Write",
            &json!({"file_path": "/etc/x"}),
            false,
            &ctx
        )));

        let p2 = perms(PermissionMode::BypassPermissions, &[], &["Bash"], &[], None);
        assert!(is_allow(&p2.check(
            "Bash",
            &json!({"command": "ls"}),
            false,
            &ctx
        )));

        let p3 = perms(
            PermissionMode::BypassPermissions,
            &[],
            &[],
            &["Bash(rm *)"],
            None,
        );
        assert!(matches!(
            p3.check("Bash", &json!({"command": "rm -rf /"}), false, &ctx),
            PermissionDecision::Deny(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn perms_with_classifier(
        mode: PermissionMode,
        allow: &[&str],
        ask: &[&str],
        deny: &[&str],
        classifier: zen_core::ClassifierSettings,
    ) -> RulePermissions {
        let settings = PermissionSettings {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            ask: ask.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            default_mode: None,
            additional_directories: vec![],
            sandbox_mode: None,
            classifier,
        };
        RulePermissions::new(mode, &settings, vec![], vec![], None)
    }

    #[test]
    fn classifier_gate_order_and_codes() {
        use zen_core::{GateBucket, PermissionChecker as _};
        let dir = tmpdir("gateorder");
        let ctx = ctx_for(&dir);

        // Deny rule beats the classifier: the reason names the rule.
        let p = perms(PermissionMode::Default, &[], &[], &["Bash(rm *)"], None);
        let (d, info) = p.gate_decision("Bash", &bash("rm -rf /"), false, &ctx);
        assert!(matches!(d, PermissionDecision::Deny(_)));
        assert_eq!(info.bucket, GateBucket::Denied);
        assert!(
            info.reason_code.starts_with("rule:"),
            "{}",
            info.reason_code
        );

        // An exact allow rule beats the classifier's dangerous verdict
        // (D2) — but only for the exact command it covers.
        let p = perms(PermissionMode::Default, &["Bash(rm -rf /)"], &[], &[], None);
        let (d, info) = p.gate_decision("Bash", &bash("rm -rf /"), false, &ctx);
        assert!(matches!(d, PermissionDecision::Allow));
        assert_eq!(info.bucket, GateBucket::Allowed);
        let (d, info) = p.gate_decision("Bash", &bash("echo hi && rm -rf /"), false, &ctx);
        assert!(matches!(d, PermissionDecision::Deny(_)));
        assert_eq!(info.reason_code, "danger.rm-recursive-root");

        // No rule: the classifier denies with its own code.
        let p = perms(PermissionMode::Default, &[], &[], &[], None);
        let (d, info) = p.gate_decision("Bash", &bash("rm -rf /"), false, &ctx);
        assert!(matches!(d, PermissionDecision::Deny(_)));
        assert_eq!(info.bucket, GateBucket::Denied);
        assert_eq!(info.reason_code, "danger.rm-recursive-root");

        // A warn-labelled mutation lands in the Classifier bucket
        // carrying its label for the approver.
        let (d, info) = p.gate_decision("Bash", &bash("git reset --hard"), false, &ctx);
        assert!(matches!(d, PermissionDecision::Ask));
        assert_eq!(info.bucket, GateBucket::Classifier);
        assert_eq!(info.reason_code, "warn.git-destructive");

        // DontAsk turns the Classifier bucket into a denial.
        let p = perms(PermissionMode::DontAsk, &[], &[], &[], None);
        let (d, info) = p.gate_decision("Bash", &bash("cargo test"), false, &ctx);
        assert!(matches!(d, PermissionDecision::Deny(_)));
        assert_eq!(info.bucket, GateBucket::Denied);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classifier_safe_auto_allow_is_opt_in() {
        use zen_core::{ClassifierSettings, PermissionChecker as _};
        let dir = tmpdir("gateauto");
        let ctx = ctx_for(&dir);
        // Default settings: safeAutoAllow off — a safe command still asks.
        let p = perms(PermissionMode::Default, &[], &[], &[], None);
        let (d, _) = p.gate_decision("Bash", &bash("ls -la"), false, &ctx);
        assert!(matches!(d, PermissionDecision::Ask));
        // Opted in: the same command is allowed with the safe code.
        let p = perms_with_classifier(
            PermissionMode::Default,
            &[],
            &[],
            &[],
            ClassifierSettings {
                enabled: true,
                safe_auto_allow: true,
            },
        );
        let (d, info) = p.gate_decision("Bash", &bash("ls -la"), false, &ctx);
        assert!(matches!(d, PermissionDecision::Allow));
        assert_eq!(info.reason_code, "safe.bash-readonly");
        // Disabled classifier: the dangerous hard-deny goes away too.
        let p = perms_with_classifier(
            PermissionMode::Default,
            &[],
            &[],
            &[],
            ClassifierSettings {
                enabled: false,
                safe_auto_allow: false,
            },
        );
        let (d, _) = p.gate_decision("Bash", &bash("rm -rf /"), false, &ctx);
        assert!(matches!(d, PermissionDecision::Ask));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dont_ask_turns_prompts_into_denials_but_keeps_allow_rules() {
        let dir = tmpdir("dontask");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::DontAsk, &[], &[], &[], None);
        assert!(is_allow(&p.check(
            "Read",
            &json!({"file_path": "a"}),
            true,
            &ctx
        )));
        assert!(matches!(
            p.check("Write", &json!({"file_path": "a"}), false, &ctx),
            PermissionDecision::Deny(_)
        ));
        assert!(matches!(
            p.check("Bash", &json!({"command": "ls"}), false, &ctx),
            PermissionDecision::Deny(_)
        ));

        let p2 = perms(PermissionMode::DontAsk, &[], &["WebFetch"], &[], None);
        assert!(matches!(
            p2.check("WebFetch", &json!({"url": "https://x.test"}), false, &ctx),
            PermissionDecision::Deny(_)
        ));

        let p3 = perms(
            PermissionMode::DontAsk,
            &["Bash(cargo test)"],
            &[],
            &[],
            None,
        );
        assert!(is_allow(&p3.check(
            "Bash",
            &json!({"command": "cargo test"}),
            false,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ask_all_asks_everything_including_read_only() {
        let dir = tmpdir("ask-all");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::AskAll, &[], &[], &[], None);
        // Read-only tools ask too: that is the one empty cell askAll fills.
        assert!(is_ask(&p.check(
            "Read",
            &json!({"file_path": "a"}),
            true,
            &ctx
        )));
        assert!(is_ask(&p.check(
            "Glob",
            &json!({"pattern": "**/*.rs"}),
            true,
            &ctx
        )));
        // Mutating tools ask as well, inside the workdir or outside it.
        assert!(is_ask(&p.check(
            "Write",
            &json!({"file_path": "a", "content": "x"}),
            false,
            &ctx
        )));
        assert!(is_ask(&p.check(
            "Edit",
            &json!({"file_path": "a"}),
            false,
            &ctx
        )));
        assert!(is_ask(&p.check(
            "Bash",
            &json!({"command": "ls"}),
            false,
            &ctx
        )));
        assert!(is_ask(&p.check(
            "mcp__server__tool",
            &json!({}),
            false,
            &ctx
        )));
        // The same read-only call is allowed in default mode, so the
        // Ask above comes from the mode, not from the tool.
        let p2 = perms(PermissionMode::Default, &[], &[], &[], None);
        assert!(is_allow(&p2.check(
            "Read",
            &json!({"file_path": "a"}),
            true,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ask_all_ignores_allow_rules_but_keeps_deny_rules() {
        let dir = tmpdir("ask-all-rules");
        let ctx = ctx_for(&dir);
        // An allow rule never self-allows in askAll.
        let p = perms(PermissionMode::AskAll, &["Read", "Bash"], &[], &[], None);
        assert!(is_ask(&p.check(
            "Read",
            &json!({"file_path": "a"}),
            true,
            &ctx
        )));
        assert!(is_ask(&p.check(
            "Bash",
            &json!({"command": "ls"}),
            false,
            &ctx
        )));
        // A deny rule still wins first, and an ask rule still asks.
        let p2 = perms(
            PermissionMode::AskAll,
            &["Bash"],
            &["Bash(git push *)"],
            &["Bash(rm *)"],
            None,
        );
        assert!(matches!(
            p2.check("Bash", &json!({"command": "rm -rf /"}), false, &ctx),
            PermissionDecision::Deny(_)
        ));
        assert!(is_ask(&p2.check(
            "Bash",
            &json!({"command": "git push origin main"}),
            false,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_only_allow_needs_either_flag_or_tool_list() {
        let dir = tmpdir("read-only-or");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::Default, &[], &[], &[], None);
        // Either side of the `||` allows on its own: the caller flag
        // with a tool that is not on the read-only list, and a listed
        // tool with the flag off. If `||` became `&&`, both would ask.
        assert!(is_allow(&p.check(
            "Bash",
            &json!({"command": "ls"}),
            true,
            &ctx
        )));
        assert!(is_allow(&p.check(
            "Read",
            &json!({"file_path": "a"}),
            false,
            &ctx
        )));
        // In askAll neither side allows: the mode guard still asks.
        let p2 = perms(PermissionMode::AskAll, &[], &[], &[], None);
        assert!(is_ask(&p2.check(
            "Bash",
            &json!({"command": "ls"}),
            true,
            &ctx
        )));
        assert!(is_ask(&p2.check(
            "Read",
            &json!({"file_path": "a"}),
            false,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deny_beats_allow_and_ask() {
        let dir = tmpdir("deny-wins");
        let ctx = ctx_for(&dir);
        let p = perms(
            PermissionMode::Default,
            &["Bash"],
            &[],
            &["Bash(rm *)"],
            None,
        );
        assert!(is_allow(&p.check(
            "Bash",
            &json!({"command": "ls"}),
            false,
            &ctx
        )));
        assert!(matches!(
            p.check("Bash", &json!({"command": "rm -rf /"}), false, &ctx),
            PermissionDecision::Deny(_)
        ));

        let p2 = perms(
            PermissionMode::Default,
            &["Bash"],
            &["Bash(rm *)"],
            &["Bash(rm *)"],
            None,
        );
        assert!(matches!(
            p2.check("Bash", &json!({"command": "rm x"}), false, &ctx),
            PermissionDecision::Deny(_)
        ));

        let p3 = perms(
            PermissionMode::Default,
            &["Bash"],
            &["Bash(git push *)"],
            &[],
            None,
        );
        assert!(is_ask(&p3.check(
            "Bash",
            &json!({"command": "git push origin main"}),
            false,
            &ctx
        )));
        assert!(is_allow(&p3.check(
            "Bash",
            &json!({"command": "git status"}),
            false,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_only_tools_allowed_in_default_and_accept_edits() {
        let dir = tmpdir("readonly-tools");
        let ctx = ctx_for(&dir);
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::DontAsk,
        ] {
            let p = perms(mode, &[], &[], &[], None);
            for tool in [
                "Read",
                "Glob",
                "Grep",
                "TodoWrite",
                "Task",
                "delegate",
                "Skill",
            ] {
                assert!(
                    is_allow(&p.check(tool, &json!({}), true, &ctx)),
                    "{} should be allowed in {}",
                    tool,
                    mode.as_str()
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_allow_rule_updates_state_and_file() {
        let dir = tmpdir("always");
        let settings_path = dir.join(".zen/settings.local.json");
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(
            &settings_path,
            r#"{"model":"keep-me","permissions":{"allow":["Read"],"deny":["Bash(rm *)"]}}"#,
        )
        .unwrap();

        let ctx = ctx_for(&dir);
        let p = perms(
            PermissionMode::Default,
            &["Read"],
            &[],
            &[],
            Some(settings_path.clone()),
        );
        assert!(is_ask(&p.check(
            "Write",
            &json!({"file_path": "a"}),
            false,
            &ctx
        )));
        p.add_allow_rule("Write");
        assert!(is_allow(&p.check(
            "Write",
            &json!({"file_path": "a"}),
            false,
            &ctx
        )));

        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(saved["model"], json!("keep-me"));
        assert_eq!(saved["permissions"]["deny"], json!(["Bash(rm *)"]));
        let allow = saved["permissions"]["allow"].as_array().unwrap();
        assert!(allow.contains(&json!("Read")));
        assert!(allow.contains(&json!("Write")));

        p.add_allow_rule("Write");
        let saved2: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(saved2["permissions"]["allow"].as_array().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_allow_rule_creates_missing_file() {
        let dir = tmpdir("always-new");
        let settings_path = dir.join("nested/deeper/settings.local.json");
        let ctx = ctx_for(&dir);
        let p = perms(
            PermissionMode::Default,
            &[],
            &[],
            &[],
            Some(settings_path.clone()),
        );
        p.add_allow_rule("Bash(cargo test)");
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(saved["permissions"]["allow"], json!(["Bash(cargo test)"]));
        assert!(is_allow(&p.check(
            "Bash",
            &json!({"command": "cargo test"}),
            false,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_allow_rule_without_path_only_updates_memory() {
        let dir = tmpdir("always-mem");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::Default, &[], &[], &[], None);
        p.add_allow_rule("Edit");
        assert!(is_allow(&p.check(
            "Edit",
            &json!({"file_path": "a"}),
            false,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suggested_rules_are_sane() {
        assert_eq!(
            suggested_rule("Bash", &json!({"command": "ls -la"})).unwrap(),
            "Bash(ls -la)"
        );
        assert_eq!(
            suggested_rule("Read", &json!({"file_path": "a.txt"})).unwrap(),
            "Read(a.txt)"
        );
        assert_eq!(
            suggested_rule("WebFetch", &json!({"url": "https://www.example.com/x"})).unwrap(),
            "WebFetch(domain:example.com)"
        );
        assert_eq!(
            suggested_rule("TodoWrite", &json!({})).unwrap(),
            "TodoWrite"
        );
    }

    fn bash(c: &str) -> Value {
        json!({ "command": c })
    }

    #[test]
    fn c2_1_first_match_wins_specificity_ignored() {
        let dir = tmpdir("c21");
        let ctx = ctx_for(&dir);

        let p = perms(
            PermissionMode::Default,
            &["Bash(git push origin main)"],
            &["Bash(git *)"],
            &[],
            None,
        );
        assert!(is_ask(&p.check(
            "Bash",
            &bash("git push origin main"),
            false,
            &ctx
        )));
        let p2 = perms(
            PermissionMode::Default,
            &["Bash(*)"],
            &[],
            &["Bash(curl *)"],
            None,
        );
        assert!(matches!(
            p2.check("Bash", &bash("curl x"), false, &ctx),
            PermissionDecision::Deny(_)
        ));
        assert!(is_allow(&p2.check("Bash", &bash("ls"), false, &ctx)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn c2_2_only_tool_level_deny_removes_tool() {
        assert!(rule_removes_tool("Bash", "Bash"));
        assert!(rule_removes_tool("Bash(*)", "Bash"));
        assert!(rule_removes_tool("WebFetch", "WebFetch"));
        assert!(!rule_removes_tool("Bash(rm *)", "Bash"));
        assert!(!rule_removes_tool("Read(./.env)", "Read"));
        assert!(rule_removes_tool("mcp__srv", "mcp__srv__q"));
        assert!(rule_removes_tool("mcp__*", "mcp__srv__q"));
        assert!(!rule_removes_tool("Bash", "Read"));
    }

    #[test]
    fn c2_3_bash_wildcards() {
        let cwd = PathBuf::from("/w");
        let m = |r: &str, c: &str| rule_matches(r, "Bash", &bash(c), &cwd);
        assert!(m("Bash(ls *)", "ls"));
        assert!(m("Bash(ls *)", "ls -la"));
        assert!(!m("Bash(ls *)", "lsof"));
        assert!(m("Bash(ls*)", "lsof"));
        assert!(m("Bash(ls:*)", "ls -la"));
        assert!(m("Bash(ls:*)", "ls"));
        assert!(!m("Bash(ls:*)", "lsof"));

        assert!(m("Bash(git * main)", "git push origin main"));
        assert!(!m("Bash(git * main)", "git push origin dev"));
        assert!(m("Bash(* --version)", "node --version"));
        assert!(m("Bash(npm run *:fast)", "npm run test:fast"));
    }

    #[test]
    fn c2_4_compound_commands() {
        assert_eq!(
            split_compound("a && b || c; d | e |& f & g\nh").unwrap(),
            vec!["a", "b", "c", "d", "e", "f", "g", "h"]
        );
        assert_eq!(split_compound("echo 'a && b' \"c;d\"").unwrap().len(), 1);
        assert_eq!(
            split_compound("cargo test 2>&1 | tail").unwrap(),
            vec!["cargo test 2>&1", "tail"]
        );
        assert_eq!(split_compound("ls;").unwrap(), vec!["ls"]);
        assert!(split_compound("ls &&").is_none());

        let dir = tmpdir("c24");
        let ctx = ctx_for(&dir);
        let p = perms(
            PermissionMode::Default,
            &["Bash(git status)", "Bash(ls *)"],
            &[],
            &["Bash(rm *)"],
            None,
        );

        assert!(is_allow(&p.check(
            "Bash",
            &bash("git status && ls -la"),
            false,
            &ctx
        )));
        assert!(is_ask(&p.check(
            "Bash",
            &bash("git status && curl x"),
            false,
            &ctx
        )));

        assert!(matches!(
            p.check("Bash", &bash("ls && rm -rf x"), false, &ctx),
            PermissionDecision::Deny(_)
        ));

        let p2 = perms(
            PermissionMode::Default,
            &["Bash"],
            &["Bash(git push *)"],
            &[],
            None,
        );
        assert!(is_ask(&p2.check(
            "Bash",
            &bash("git add . && git push origin"),
            false,
            &ctx
        )));

        assert!(is_ask(&p.check(
            "Bash",
            &bash("git status &&"),
            false,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn c2_7_path_rule_forms() {
        let cwd = PathBuf::from("/work/proj");
        let f = |p: &str| json!({ "file_path": p });

        assert!(rule_matches("Read(/src/**)", "Read", &f("src/a.rs"), &cwd));
        assert!(!rule_matches(
            "Read(/src/**)",
            "Read",
            &f("/src/a.rs"),
            &cwd
        ));
        assert!(rule_matches(
            "Read(//src/**)",
            "Read",
            &f("/src/a.rs"),
            &cwd
        ));

        assert!(rule_matches("Read(.env)", "Read", &f(".env"), &cwd));
        assert!(rule_matches("Read(.env)", "Read", &f("a/b/.env"), &cwd));
        assert!(rule_matches("Read(*.log)", "Read", &f("logs/x.log"), &cwd));
        assert!(!rule_matches("Read(.env)", "Read", &f("a/.envrc"), &cwd));

        assert!(rule_matches(
            "Edit(src/**)",
            "Write",
            &f("src/new.rs"),
            &cwd
        ));
        assert!(rule_matches("Edit", "Write", &f("x"), &cwd));
    }

    #[test]
    fn c2_7_read_deny_guards_edit_write_grep_glob() {
        let dir = tmpdir("c27");
        let ctx = ctx_for(&dir);
        let p = perms(
            PermissionMode::BypassPermissions,
            &[],
            &[],
            &["Read(./secrets/**)"],
            None,
        );
        for (tool, input) in [
            ("Read", json!({"file_path": "secrets/k"})),
            ("Edit", json!({"file_path": "secrets/k"})),
            ("Write", json!({"file_path": "secrets/k", "content": ""})),
            ("Grep", json!({"pattern": "x", "path": "secrets"})),
            ("Glob", json!({"pattern": "*", "path": "secrets/sub"})),
        ] {
            assert!(
                matches!(
                    p.check(tool, &input, false, &ctx),
                    PermissionDecision::Deny(_)
                ),
                "{} should be denied",
                tool
            );
        }
        assert!(is_allow(&p.check(
            "Write",
            &json!({"file_path": "src/k"}),
            false,
            &ctx
        )));

        let p2 = perms(PermissionMode::Default, &["Read(./src/**)"], &[], &[], None);
        assert!(is_ask(&p2.check(
            "Write",
            &json!({"file_path": "src/k"}),
            false,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn c2_8_webfetch_domain_forms() {
        let cwd = PathBuf::from("/w");
        let m = |r: &str, u: &str| rule_matches(r, "WebFetch", &json!({ "url": u }), &cwd);
        assert!(m("WebFetch(domain:Example.COM)", "https://EXAMPLE.com/x"));
        assert!(m(
            "WebFetch(domain:*.example.com)",
            "https://api.example.com/x"
        ));
        assert!(m(
            "WebFetch(domain:*.example.com)",
            "https://a.b.example.com/x"
        ));
        assert!(!m(
            "WebFetch(domain:*.example.com)",
            "https://example.com/x"
        ));
        assert!(!m(
            "WebFetch(domain:*.example.com)",
            "https://badexample.com/x"
        ));
        assert!(m("WebFetch(domain:*)", "https://anything.test/"));
        assert!(!m("WebFetch(domain:*)", ""));
    }

    #[test]
    fn c2_9_mcp_rules_and_globs() {
        let dir = tmpdir("c29");
        let ctx = ctx_for(&dir);
        let cwd = dir.clone();
        let v = json!({});
        assert!(rule_matches("mcp__srv__*", "mcp__srv__query", &v, &cwd));
        assert!(!rule_matches("mcp__srv__*", "mcp__other__query", &v, &cwd));

        let p = perms(PermissionMode::Default, &["*"], &["mcp__*"], &[], None);
        assert!(is_ask(&p.check("mcp__srv__q", &v, false, &ctx)));
        assert!(
            is_ask(&p.check("Bash", &bash("ls"), false, &ctx)),
            "allow `*` must not match"
        );
        let p2 = perms(PermissionMode::BypassPermissions, &[], &[], &["*"], None);
        assert!(matches!(
            p2.check("Bash", &bash("ls"), false, &ctx),
            PermissionDecision::Deny(_)
        ));
        let p3 = perms(PermissionMode::Default, &["mcp__srv__*"], &[], &[], None);
        assert!(is_allow(&p3.check("mcp__srv__q", &v, false, &ctx)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn c2_11_mode_aliases_and_accept_edits_fs_scope() {
        // 'manual' and 'auto' are not modes and never silently mean default.
        assert_eq!(PermissionMode::parse("manual"), None);
        assert_eq!(PermissionMode::parse("auto"), None);
        // Kebab-case spellings parse the same as camelCase.
        assert_eq!(
            PermissionMode::parse("accept-edits"),
            Some(PermissionMode::AcceptEdits)
        );
        assert_eq!(
            PermissionMode::parse("dont-ask"),
            Some(PermissionMode::DontAsk)
        );
        let dir = tmpdir("c211");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::AcceptEdits, &[], &[], &[], None);
        assert!(is_allow(&p.check(
            "Bash",
            &bash("mkdir -p a/b && touch a/b/c"),
            false,
            &ctx
        )));
        // Phase 2: copying into a system path is classifier-dangerous.
        assert!(matches!(
            p.check("Bash", &bash("cp a /etc/passwd"), false, &ctx),
            PermissionDecision::Deny(_)
        ));
        assert!(is_ask(&p.check(
            "Bash",
            &bash("mv a ../outside"),
            false,
            &ctx
        )));
        assert!(is_ask(&p.check(
            "Bash",
            &bash("mkdir a && rm -rf b"),
            false,
            &ctx
        )));
        assert!(is_ask(&p.check(
            "Bash",
            &bash("touch $(whoami)"),
            false,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn c2_12_always_allow_one_rule_per_subcommand() {
        assert_eq!(
            suggested_rules("Bash", &bash("git add . && git commit -m x")),
            vec!["Bash(git add .)", "Bash(git commit -m x)"]
        );
        assert_eq!(suggested_rules("Bash", &bash("a;b;c;d;e;f;g")).len(), 5);
        assert_eq!(suggested_rules("Bash", &bash("ls")), vec!["Bash(ls)"]);
        assert_eq!(
            suggested_rules("Write", &json!({"file_path": "a"})),
            vec!["Write(a)"]
        );

        let dir = tmpdir("c212");
        let path = dir.join(".zen/settings.local.json");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::Default, &[], &[], &[], Some(path.clone()));
        for r in suggested_rules("Bash", &bash("git add . && git commit -m x")) {
            p.add_allow_rule(&r);
        }
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["permissions"]["allow"].as_array().unwrap().len(), 2);
        assert!(is_allow(&p.check(
            "Bash",
            &bash("git add . && git commit -m x"),
            false,
            &ctx
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn c2_13_ask_rule_detection() {
        let dir = tmpdir("c213");
        let ctx = ctx_for(&dir);
        let p = perms(
            PermissionMode::Default,
            &[],
            &["Bash(git push *)"],
            &[],
            None,
        );
        assert!(p.ask_rule_matches("Bash", &bash("git push"), &ctx));
        assert!(p.ask_rule_matches("Bash", &bash("ls && git push x"), &ctx));
        assert!(!p.ask_rule_matches("Bash", &bash("ls"), &ctx));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_mutating_ask_keeps_mode_reason_code() {
        // Kills the guard -> true mutant on the ask_code match: a
        // Mutating classification with the default reason code must
        // fall through to "mode.default", not keep "mutating.default".
        use zen_core::PermissionChecker as _;
        let dir = tmpdir("plainask");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::Default, &[], &[], &[], None);
        let (d, info) = p.gate_decision("Bash", &bash("cargo test"), false, &ctx);
        assert!(matches!(d, PermissionDecision::Ask));
        assert_eq!(info.bucket, GateBucket::Classifier);
        assert_eq!(info.reason_code, "mode.default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Browser agent (spec section 4): the permission gate for the six
    // browser tools. The test session key is "perm-session" (ctx_for).
    fn browser_ctx() -> (PathBuf, ToolContext) {
        let dir = tmpdir("browser-gate");
        let ctx = ctx_for(&dir);
        (dir, ctx)
    }

    fn is_deny(d: &PermissionDecision) -> bool {
        matches!(d, PermissionDecision::Deny(_))
    }

    fn browser_perms(
        mode: PermissionMode,
        allow: &[&str],
        ask: &[&str],
        deny: &[&str],
    ) -> RulePermissions {
        perms(mode, allow, ask, deny, None)
    }

    fn browser_input(tool: &str) -> Value {
        match tool {
            "browser_navigate" => serde_json::json!({"url": "https://example.com/page"}),
            "browser_click" => serde_json::json!({"selector": "#ok"}),
            "browser_type" => serde_json::json!({"selector": "#q", "text": "hello"}),
            _ => serde_json::json!({}),
        }
    }

    #[test]
    fn browser_tool_classification() {
        assert!(tool_is_read_only("browser_navigate"));
        assert!(tool_is_read_only("browser_content"));
        assert!(tool_is_read_only("browser_screenshot"));
        assert!(tool_is_read_only("browser_wait"));
        assert!(!tool_is_read_only("browser_click"));
        assert!(!tool_is_read_only("browser_type"));
        assert!(is_edit_tool("browser_click"));
        assert!(is_edit_tool("browser_type"));
    }

    #[test]
    fn browser_read_only_tools_allow_in_default() {
        let (dir, ctx) = browser_ctx();
        let p = browser_perms(PermissionMode::Default, &[], &[], &[]);
        for tool in [
            "browser_navigate",
            "browser_content",
            "browser_screenshot",
            "browser_wait",
        ] {
            let d = p.check(tool, &browser_input(tool), true, &ctx);
            assert!(is_allow(&d), "{tool}: expected Allow, got {d:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_click_and_type_ask_in_default() {
        let (dir, ctx) = browser_ctx();
        let p = browser_perms(PermissionMode::Default, &[], &[], &[]);
        for tool in ["browser_click", "browser_type"] {
            let d = p.check(tool, &browser_input(tool), false, &ctx);
            assert!(is_ask(&d), "{tool}: expected Ask, got {d:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_plan_mode_denies_all_browser_tools() {
        let (dir, ctx) = browser_ctx();
        let p = browser_perms(PermissionMode::Plan, &[], &[], &[]);
        for tool in [
            "browser_navigate",
            "browser_click",
            "browser_type",
            "browser_content",
            "browser_screenshot",
            "browser_wait",
        ] {
            let d = p.check(tool, &browser_input(tool), tool_is_read_only(tool), &ctx);
            assert!(is_deny(&d), "{tool}: expected Deny, got {d:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_ask_all_asks_every_browser_tool() {
        let (dir, ctx) = browser_ctx();
        let p = browser_perms(PermissionMode::AskAll, &[], &[], &[]);
        for tool in [
            "browser_navigate",
            "browser_click",
            "browser_type",
            "browser_content",
            "browser_screenshot",
            "browser_wait",
        ] {
            let d = p.check(tool, &browser_input(tool), tool_is_read_only(tool), &ctx);
            assert!(is_ask(&d), "{tool}: expected Ask, got {d:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_accept_edits_does_not_auto_allow_click_type() {
        let (dir, ctx) = browser_ctx();
        let p = browser_perms(PermissionMode::AcceptEdits, &[], &[], &[]);
        for tool in ["browser_click", "browser_type"] {
            let d = p.check(tool, &browser_input(tool), false, &ctx);
            assert!(is_ask(&d), "{tool}: expected Ask, got {d:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_bypass_never_auto_allows_click_type() {
        let (dir, ctx) = browser_ctx();
        let p = browser_perms(PermissionMode::BypassPermissions, &[], &[], &[]);
        for tool in ["browser_click", "browser_type"] {
            let d = p.check(tool, &browser_input(tool), false, &ctx);
            assert!(is_ask(&d), "{tool}: expected Ask, got {d:?}");
        }
        // Read-only browser tools follow the general gate in Bypass.
        let d = p.check(
            "browser_navigate",
            &browser_input("browser_navigate"),
            true,
            &ctx,
        );
        assert!(is_allow(&d), "navigate: expected Allow, got {d:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_submit_always_asks_despite_allow_rule() {
        let (dir, ctx) = browser_ctx();
        let p = browser_perms(
            PermissionMode::Default,
            &["BrowserType", "BrowserClick"],
            &[],
            &[],
        );
        let d = p.check(
            "browser_type",
            &serde_json::json!({"selector": "#q", "text": "x", "submit": true}),
            false,
            &ctx,
        );
        assert!(is_ask(&d), "submit type: expected Ask, got {d:?}");
        let d = p.check(
            "browser_click",
            &serde_json::json!({"selector": "button[type=submit]"}),
            false,
            &ctx,
        );
        assert!(is_ask(&d), "submit click: expected Ask, got {d:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_login_always_asks_and_browser_login_deny_blocks() {
        let (dir, ctx) = browser_ctx();
        let login = serde_json::json!({"selector": "#password", "text": "s3cret"});
        // Even with an explicit allow rule, a login call asks.
        let p = browser_perms(PermissionMode::Default, &["BrowserType"], &[], &[]);
        let d = p.check("browser_type", &login, false, &ctx);
        assert!(is_ask(&d), "login type: expected Ask, got {d:?}");
        // In BypassPermissions, a login call still asks.
        let pb = browser_perms(
            PermissionMode::BypassPermissions,
            &["BrowserType"],
            &[],
            &[],
        );
        let d = pb.check("browser_type", &login, false, &ctx);
        assert!(is_ask(&d), "login type in Bypass: expected Ask, got {d:?}");
        // And a BrowserLogin deny rule blocks it outright.
        let pd = browser_perms(PermissionMode::Default, &[], &[], &["BrowserLogin"]);
        let d = pd.check("browser_type", &login, false, &ctx);
        assert!(
            is_deny(&d),
            "login type with deny BrowserLogin: expected Deny, got {d:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_allow_rule_allows_plain_click_in_default() {
        let (dir, ctx) = browser_ctx();
        let p = browser_perms(PermissionMode::Default, &["BrowserClick"], &[], &[]);
        let d = p.check(
            "browser_click",
            &browser_input("browser_click"),
            false,
            &ctx,
        );
        assert!(
            is_allow(&d),
            "plain click with allow rule: expected Allow, got {d:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_deny_rule_by_host() {
        let (dir, ctx) = browser_ctx();
        let p = browser_perms(
            PermissionMode::Default,
            &[],
            &[],
            &["browser_navigate(domain:example.com)"],
        );
        let d = p.check(
            "browser_navigate",
            &serde_json::json!({"url": "https://example.com/page"}),
            true,
            &ctx,
        );
        assert!(
            is_deny(&d),
            "navigate to example.com: expected Deny, got {d:?}"
        );
        let d = p.check(
            "browser_navigate",
            &serde_json::json!({"url": "https://other.test/"}),
            true,
            &ctx,
        );
        assert!(
            is_allow(&d),
            "navigate to other.test: expected Allow, got {d:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_destructive_click_always_asks() {
        let (dir, ctx) = browser_ctx();
        let p = browser_perms(PermissionMode::Default, &["BrowserClick"], &[], &[]);
        let d = p.check(
            "browser_click",
            &serde_json::json!({"selector": "#delete-account"}),
            false,
            &ctx,
        );
        assert!(is_ask(&d), "destructive click: expected Ask, got {d:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_suggested_rule() {
        assert_eq!(
            suggested_rule(
                "browser_type",
                &serde_json::json!({"selector": "#password"})
            ),
            Some("BrowserLogin".to_string())
        );
        assert_eq!(
            suggested_rule(
                "browser_navigate",
                &serde_json::json!({"url": "https://example.com/page"})
            ),
            Some("browser_navigate(domain:example.com)".to_string())
        );
    }

    // ==================== PR #580 mutant kills ====================

    #[test]
    fn rule_browser_name_matching_needs_same_tool() {
        let cwd = PathBuf::from("/tmp/work");
        // A rule naming browser_click must not match browser_navigate.
        assert!(!rule_matches(
            "BrowserClick",
            "browser_navigate",
            &json!({}),
            &cwd
        ));
        // ... and BrowserLogin must not match a read-only browser tool.
        assert!(!rule_matches(
            "BrowserLogin",
            "browser_navigate",
            &json!({}),
            &cwd
        ));
        // Positive control: the same tool matches (BrowserClick spelling).
        assert!(rule_matches(
            "BrowserClick",
            "browser_click",
            &json!({}),
            &cwd
        ));
    }

    #[test]
    fn browser_login_call_needs_a_mutating_login_call() {
        // Read-only browser tools are never login calls.
        assert!(!browser_login_call("browser_navigate", &json!({})));
        assert!(!browser_login_call("browser_content", &json!({})));
        // A mutating call on a password selector is a login call.
        assert!(browser_login_call(
            "browser_click",
            &json!({"selector": "#password"})
        ));
        assert!(browser_login_call(
            "browser_type",
            &json!({"selector": "#password"})
        ));
        // ... but the same tools on a plain selector are not.
        assert!(!browser_login_call(
            "browser_click",
            &json!({"selector": "#ok"})
        ));
    }

    #[test]
    fn accept_edits_does_not_allow_unlisted_tools_with_paths() {
        let dir = tmpdir("accept-edits-2");
        let ctx = ctx_for(&dir);
        let p = perms(PermissionMode::AcceptEdits, &[], &[], &[], None);
        // A tool that is neither an edit tool nor a browser tool must still ask,
        // even when its file path is inside the workdir.
        let d = p.evaluate(
            "mcp__files__write",
            &json!({"file_path": "x.txt"}),
            false,
            &ctx,
        );
        assert!(
            is_ask(&d),
            "AcceptEdits must ask for unlisted tools, got {d:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_suggested_rule_prefers_the_host_scoped_rule() {
        // With a known host, click/type suggest the host-scoped rule.
        crate::browser::update_cached_page(
            "suggest-key",
            "https://example.com/page".to_string(),
            false,
        );
        assert_eq!(
            suggested_rule("browser_click", &json!({"selector": "#ok"})),
            Some("browser_click(domain:example.com)".to_string())
        );
        assert_eq!(
            suggested_rule("browser_type", &json!({"selector": "#ok"})),
            Some("browser_type(domain:example.com)".to_string())
        );
    }
}
