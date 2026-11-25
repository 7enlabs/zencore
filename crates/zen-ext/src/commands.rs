use crate::HOOK_OUTPUT_LIMIT;
use crate::skills::{Frontmatter, discover_skill_entries, split_frontmatter};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashSource {
    Command,

    Skill,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub allowed_tools: Vec<String>,
    pub model: Option<String>,
    pub disable_model_invocation: bool,

    pub body: String,

    pub path: PathBuf,
    pub source: SlashSource,
}

impl SlashCommand {
    fn from_parts(
        name: String,
        fm: Frontmatter,
        body: String,
        path: PathBuf,
        source: SlashSource,
    ) -> Self {
        let get = |k: &str| fm.extra.get(k).filter(|v| !v.is_empty()).cloned();
        Self {
            name,
            description: fm.description.clone().filter(|d| !d.is_empty()),
            argument_hint: get("argument-hint").or_else(|| get("argument_hint")),
            allowed_tools: fm.allowed_tools.clone(),
            model: get("model").filter(|m| m != "inherit"),
            disable_model_invocation: fm.disable_model_invocation,
            body,
            path,
            source,
        }
    }

    pub fn skill_dir(&self) -> Option<PathBuf> {
        match self.source {
            SlashSource::Skill => self.path.parent().map(Path::to_path_buf),
            SlashSource::Command => None,
        }
    }
}

pub struct ExpandContext<'a> {
    pub project_dir: PathBuf,

    pub session_id: String,

    pub allow_shell: Option<&'a (dyn Fn(&str) -> bool + Send + Sync)>,
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => Some(true),
        "false" | "no" | "0" | "off" => Some(false),
        _ => None,
    }
}

/// Maximum directory depth `collect_md` descends into a commands tree.
/// Bounds session-start discovery against absurdly deep (non-cyclic)
/// directory trees; see issue #388.
const COLLECT_MD_MAX_DEPTH: usize = 16;

fn collect_md(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let mut visited = HashSet::new();
    // Canonical root: the walk must never escape it through a symlink
    // (issue #388 follow-up: `loop -> ..` used to let `.zen/*.md` leak
    // in as commands named `loop/<name>`).
    let root_key = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    collect_md_inner(root, &root_key, dir, out, &mut visited, 0);
}

fn collect_md_inner(
    root: &Path,
    root_key: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf)>,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) {
    if depth > COLLECT_MD_MAX_DEPTH {
        return;
    }
    // A directory reached through different symlinks canonicalizes to the
    // same path, so a symlink cycle revisits an already-seen directory
    // instead of recursing forever (issue #388: `loop -> ..` inside
    // `.zen/commands` used to overflow the stack at session start).
    // Fall back to the raw path when canonicalization fails; `read_dir`
    // below then fails the same way it did before this change.
    let key = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    // Never walk outside the commands tree: a symlink to a parent (or
    // anywhere else) is skipped, so sibling files cannot leak in.
    if !key.starts_with(root_key) {
        return;
    }
    if !visited.insert(key) {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            collect_md_inner(root, root_key, &p, out, visited, depth + 1);
        } else if p.extension().is_some_and(|x| x == "md") {
            let rel = p.strip_prefix(root).unwrap_or(&p).with_extension("");
            let name = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(":");
            if !name.is_empty() {
                out.push((name, p));
            }
        }
    }
}

pub fn discover_custom_commands(
    cwd: &Path,
    home: Option<&Path>,
    trusted: bool,
) -> Vec<SlashCommand> {
    let home_dir = home.map(Path::to_path_buf).or_else(dirs::home_dir);
    let mut roots = Vec::new();
    if let Some(h) = &home_dir {
        roots.push(h.join(".zen").join("commands"));
    }
    // Issue #330: project commands load only in a trusted folder;
    // a cloned repository must not smuggle prompt text in through
    // its own slash commands.
    if trusted {
        roots.push(cwd.join(".zen").join("commands"));
    }
    let mut out: Vec<SlashCommand> = Vec::new();
    for root in roots {
        let mut files = Vec::new();
        collect_md(&root, &root, &mut files);
        for (name, path) in files {
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (fm, body) = split_frontmatter(&raw);
            let cmd = SlashCommand::from_parts(
                name,
                fm.unwrap_or_default(),
                body,
                path,
                SlashSource::Command,
            );
            out.retain(|c| c.name != cmd.name);
            out.push(cmd);
        }
    }
    out
}

pub fn discover_slash_commands(
    cwd: &Path,
    home: Option<&Path>,
    trusted: bool,
) -> Vec<SlashCommand> {
    let mut out = discover_custom_commands(cwd, home, trusted);
    for (info, fm) in discover_skill_entries(cwd, home, trusted) {
        let user_invocable = fm
            .extra
            .get("user-invocable")
            .or_else(|| fm.extra.get("user_invocable"))
            .and_then(|v| parse_bool(v))
            .unwrap_or(true);
        out.retain(|c| c.name != info.name);
        if !user_invocable {
            continue;
        }
        out.push(SlashCommand::from_parts(
            info.name,
            fm,
            info.body,
            info.path,
            SlashSource::Skill,
        ));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn find_slash_command(
    cwd: &Path,
    home: Option<&Path>,
    trusted: bool,
    name: &str,
) -> Option<SlashCommand> {
    discover_slash_commands(cwd, home, trusted)
        .into_iter()
        .find(|c| c.name == name)
}

pub fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut quote: Option<char> = None;
    let mut chars = args.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') | (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    has = true;
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '"') | (None, '\'') => {
                quote = Some(c);
                has = true;
            }
            (None, c) if c.is_whitespace() => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (None, c) => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

fn substitute_args(body: &str, args: &str) -> (String, bool) {
    let parts = split_args(args);
    let nth = |i: usize| parts.get(i).cloned().unwrap_or_default();
    let mut out = String::with_capacity(body.len());
    let mut used = false;
    let mut rest = body;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            if let Some(idx_part) = tail.strip_prefix('[')
                && let Some(close) = idx_part.find(']')
                && let Ok(i) = idx_part[..close].trim().parse::<usize>()
            {
                out.push_str(&nth(i));
                used = true;
                rest = &idx_part[close + 1..];
                continue;
            }
            out.push_str(args);
            used = true;
            rest = tail;
            continue;
        }
        let mut digits = after.chars();
        if let Some(d) = digits.next().filter(char::is_ascii_digit)
            && !after[1..].starts_with(|c: char| c.is_ascii_digit())
        {
            out.push_str(&nth(d as usize - '0' as usize));
            used = true;
            rest = &after[1..];
            continue;
        }
        out.push('$');
        rest = after;
    }
    out.push_str(rest);
    (out, used)
}

fn resolve_ref(raw: &str, base: &Path) -> Option<PathBuf> {
    let p = if let Some(rest) = raw.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else {
        let p = PathBuf::from(raw);
        if p.is_absolute() { p } else { base.join(p) }
    };
    p.is_file().then_some(p)
}

/// Default timeout for `!`...`` shell blocks in slash commands, in seconds.
/// Matches the hook default timeout (`hooks.rs`, issue #392): a shell block
/// must never hang the turn forever.
const SLASH_SHELL_TIMEOUT_SECS: u64 = 60;

/// Grace period for an output-reader thread to deliver what a finished
/// shell block wrote. Bounds the wait when a background grandchild keeps
/// the pipes open after the direct child exits.
const SHELL_READER_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Max bytes read for one output stream of a shell block, or for an inlined
/// `@path` file: enough to hold `HOOK_OUTPUT_LIMIT` chars of any UTF-8 text
/// (4 bytes per char at most). Anything past this is drained and discarded
/// so a streaming command cannot grow memory without bound.
const CAPPED_READ_LIMIT: usize = HOOK_OUTPUT_LIMIT * 4;

/// Truncate `s` to `HOOK_OUTPUT_LIMIT` chars, appending a note when anything
/// was dropped. `exact` says whether `s` holds the complete input: when
/// false the true total is unknown (the input was cut short while reading),
/// so the note omits the total instead of stating a wrong one.
fn cap_text(s: &str, label: &str, exact: bool) -> String {
    let count = s.chars().count();
    if count <= HOOK_OUTPUT_LIMIT && exact {
        return s.to_string();
    }
    let head: String = s.chars().take(HOOK_OUTPUT_LIMIT).collect();
    if exact {
        format!("{head}\n[{label} truncated: {HOOK_OUTPUT_LIMIT} of {count} chars shown]")
    } else {
        format!("{head}\n[{label} truncated: first {HOOK_OUTPUT_LIMIT} chars shown]")
    }
}

/// Read a file for `@path` inlining, capped at `HOOK_OUTPUT_LIMIT` chars
/// (issue #392). Reads at most `CAPPED_READ_LIMIT + 1` bytes, so a huge file
/// never lands fully in memory.
fn read_inline_file(p: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(p).ok()?;
    let mut buf = Vec::new();
    file.by_ref()
        .take(CAPPED_READ_LIMIT as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    let hit_byte_limit = buf.len() > CAPPED_READ_LIMIT;
    if hit_byte_limit {
        buf.truncate(CAPPED_READ_LIMIT);
    }
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if hit_byte_limit && text.ends_with('\u{FFFD}') {
        // The byte cut split a multi-byte char; drop the replacement char so
        // the kept text ends on a real char boundary.
        text.pop();
    }
    Some(cap_text(&text, "file content", !hit_byte_limit))
}

/// Drain one piped stream on a thread, keeping at most `CAPPED_READ_LIMIT`
/// bytes; further bytes are read and discarded so the child never blocks on
/// a full pipe. The kept bytes and whether any bytes were discarded arrive
/// on the returned channel when the stream reaches EOF.
fn spawn_drain<R: std::io::Read + Send + 'static>(
    mut pipe: R,
) -> std::sync::mpsc::Receiver<(Vec<u8>, bool)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 8192];
        let mut cut_short = false;
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let room = CAPPED_READ_LIMIT.saturating_sub(kept.len());
                    if room > 0 {
                        let take = n.min(room);
                        kept.extend_from_slice(&buf[..take]);
                        if take < n {
                            cut_short = true;
                        }
                    } else {
                        cut_short = true;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send((kept, cut_short));
    });
    rx
}

/// Collect one drained stream, waiting at most the grace period. A stream
/// whose reader is still blocked (a background grandchild holding the pipe
/// open) yields empty output instead of hanging the turn; the returned flag
/// is true when the stream was cut short or may be incomplete.
fn collect_drain(rx: std::sync::mpsc::Receiver<(Vec<u8>, bool)>) -> (String, bool) {
    match rx.recv_timeout(SHELL_READER_GRACE) {
        Ok((bytes, cut_short)) => (String::from_utf8_lossy(&bytes).into_owned(), cut_short),
        Err(_) => (String::new(), true),
    }
}

/// Run one `!`...`` shell block with a timeout and a bounded capture (issue
/// #392). Returns the block's replacement text: stdout (plus stderr and the
/// exit code on failure), capped at `HOOK_OUTPUT_LIMIT` chars, or a timeout
/// / spawn-failure note. Killing the child on timeout only kills the direct
/// `sh` process; a compound command's grandchildren may linger until they
/// exit on their own (same as hooks).
fn run_shell_block(cmd: &str, ctx: &ExpandContext<'_>, timeout: std::time::Duration) -> String {
    use std::process::{Command, Stdio};
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(&ctx.project_dir)
        .env("ZEN_PROJECT_DIR", &ctx.project_dir)
        .env("ZEN_SESSION_ID", &ctx.session_id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("(failed to run `{cmd}`: {e})"),
    };
    let stdout_rx = child.stdout.take().map(spawn_drain);
    let stderr_rx = child.stderr.take().map(spawn_drain);
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Reader threads are left detached: they exit when the
                    // pipes close, or stay blocked if a grandchild holds
                    // the pipes open; either way the turn is not stuck.
                    return format!("(timed out after {}s: `{cmd}`)", timeout.as_secs());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return format!("(failed to run `{cmd}`: {e})");
            }
        }
    };
    let (mut s, stdout_cut) = stdout_rx.map(collect_drain).unwrap_or_default();
    let (err, stderr_cut) = stderr_rx.map(collect_drain).unwrap_or_default();
    if !status.success() {
        if !err.trim().is_empty() {
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str(err.trim_end());
        }
        s.push_str(&format!("\n(exit {})", status.code().unwrap_or(-1)));
    }
    let exact = !stdout_cut && !stderr_cut;
    cap_text(&s, "shell output", exact)
        .trim_end_matches('\n')
        .to_string()
}

fn inline_files(text: &str, base: &Path) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_ws = true;
    let mut i = 0;
    while i < text.len() {
        // The loop condition guarantees `text[i..]` is non-empty.
        let Some(c) = text[i..].chars().next() else {
            break;
        };
        if c == '@' && prev_ws {
            let end = text[i + 1..]
                .find(|ch: char| ch.is_whitespace())
                .map(|e| i + 1 + e)
                .unwrap_or(text.len());
            let token = &text[i + 1..end];

            let trimmed = token.trim_end_matches([',', '.', ';', ':', ')', '!', '?']);
            if !trimmed.is_empty()
                && let Some(content) = resolve_ref(trimmed, base).and_then(|p| read_inline_file(&p))
            {
                out.push_str(content.trim_end_matches('\n'));
                out.push_str(&token[trimmed.len()..]);
                i = end;
                prev_ws = false;
                continue;
            }
        }
        out.push(c);
        prev_ws = c.is_whitespace();
        i += c.len_utf8();
    }
    out
}

fn run_shell_blocks(text: &str, ctx: &ExpandContext<'_>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("!`") {
        let after = &rest[pos + 2..];
        let Some(close) = after.find('`') else { break };
        out.push_str(&rest[..pos]);
        let cmd = &after[..close];
        let allowed = ctx.allow_shell.is_some_and(|f| f(cmd));
        if cmd.trim().is_empty() {
            out.push_str(&rest[pos..pos + 2 + close + 1]);
        } else if !allowed {
            out.push_str(&format!("!`{}` (not run: permission denied)", cmd));
        } else {
            let timeout = std::time::Duration::from_secs(SLASH_SHELL_TIMEOUT_SECS);
            out.push_str(&run_shell_block(cmd, ctx, timeout));
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

pub fn expand_command(cmd: &SlashCommand, args: &str, ctx: &ExpandContext<'_>) -> String {
    let mut body = cmd.body.clone();
    if let Some(dir) = cmd.skill_dir() {
        body = body.replace("${ZEN_SKILL_DIR}", &dir.display().to_string());
    }
    body = body
        .replace("${ZEN_PROJECT_DIR}", &ctx.project_dir.display().to_string())
        .replace("${ZEN_SESSION_ID}", &ctx.session_id);
    let (mut body, used) = substitute_args(&body, args);
    if !used && !args.trim().is_empty() {
        let trimmed = body.trim_end().len();
        body.truncate(trimmed);
        body.push_str(&format!("\n\nARGUMENTS: {}", args.trim()));
    }
    let body = inline_files(&body, &ctx.project_dir);
    run_shell_blocks(&body, ctx)
}

pub fn expand_slash_prompt(
    prompt: &str,
    cwd: &Path,
    home: Option<&Path>,
    trusted: bool,
    ctx: &ExpandContext<'_>,
) -> Option<(SlashCommand, String)> {
    let trimmed = prompt.trim_start();
    let rest = trimmed.strip_prefix('/')?;
    let mut it = rest.splitn(2, char::is_whitespace);
    let name = it.next().unwrap_or("");
    if name.is_empty() {
        return None;
    }
    let args = it.next().unwrap_or("").trim();
    let cmd = find_slash_command(cwd, home, trusted, name)?;
    let text = expand_command(&cmd, args, ctx);
    Some((cmd, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs;

    fn ctx(dir: &Path) -> ExpandContext<'static> {
        ExpandContext {
            project_dir: dir.to_path_buf(),
            session_id: "sess-1".into(),
            allow_shell: None,
        }
    }

    fn cmd(body: &str) -> SlashCommand {
        SlashCommand {
            name: "t".into(),
            description: None,
            argument_hint: None,
            allowed_tools: vec![],
            model: None,
            disable_model_invocation: false,
            body: body.into(),
            path: PathBuf::from("/x/t.md"),
            source: SlashSource::Command,
        }
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn c5_2_discovery_subdir_namespace_frontmatter_and_project_override() {
        let tmp = TempDir::new();
        let home = tmp.path().join("home");
        let proj = tmp.path().join("proj");
        write(&home.join(".zen/commands/hello.md"), "USER $ARGUMENTS");
        write(&home.join(".zen/commands/only-user.md"), "u");
        write(&proj.join(".zen/commands/hello.md"), "PROJECT $ARGUMENTS");
        write(
            &proj.join(".zen/commands/git/commit.md"),
            "---\ndescription: Commit\nargument-hint: [msg]\nallowed-tools: Bash(git *), Read\nmodel: m-x\ndisable-model-invocation: true\n---\nCommit: $0\n",
        );
        write(&proj.join(".zen/commands/notes.txt"), "ignored");
        let all = discover_slash_commands(&proj, Some(&home), true);
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["git:commit", "hello", "only-user"]);
        let hello = all.iter().find(|c| c.name == "hello").unwrap();
        assert!(hello.body.starts_with("PROJECT"));
        let gc = all.iter().find(|c| c.name == "git:commit").unwrap();
        assert_eq!(gc.description.as_deref(), Some("Commit"));
        assert_eq!(gc.argument_hint.as_deref(), Some("[msg]"));
        assert_eq!(gc.allowed_tools, vec!["Bash(git *)", "Read"]);
        assert_eq!(gc.model.as_deref(), Some("m-x"));
        assert!(gc.disable_model_invocation);
        assert_eq!(gc.body.trim(), "Commit: $0");
    }

    #[test]
    fn c5_2_argument_placeholders() {
        let t = TempDir::new();
        let c = ctx(t.path());
        let out = expand_command(
            &cmd("a=$0 b=$1 c=$ARGUMENTS[2] all=$ARGUMENTS miss=$5 ten=$10 cost=$x"),
            "one \"two words\" three",
            &c,
        );
        assert_eq!(
            out,
            "a=one b=two words c=three all=one \"two words\" three miss= ten=$10 cost=$x"
        );

        assert_eq!(expand_command(&cmd("[$0]"), "$1", &c), "[$1]");

        assert_eq!(
            expand_command(&cmd("Review this.\n"), "src/a.rs", &c),
            "Review this.\n\nARGUMENTS: src/a.rs"
        );
        assert_eq!(expand_command(&cmd("Plain"), "", &c), "Plain");
    }

    #[test]
    fn c5_2_env_vars_and_file_inlining() {
        let t = TempDir::new();
        write(&t.path().join("notes/a.txt"), "ALPHA\n");
        let c = ctx(t.path());
        let out = expand_command(
            &cmd(
                "dir=${ZEN_PROJECT_DIR} id=${ZEN_SESSION_ID} see @notes/a.txt, mail a@notes/a.txt @missing.txt",
            ),
            "",
            &c,
        );
        assert_eq!(
            out,
            format!(
                "dir={} id=sess-1 see ALPHA, mail a@notes/a.txt @missing.txt",
                t.path().display()
            )
        );

        assert_eq!(
            expand_command(&cmd("Fix $ARGUMENTS"), "@notes/a.txt", &c),
            "Fix ALPHA"
        );
    }

    #[test]
    fn c5_2_shell_pre_execution_is_permission_gated() {
        let t = TempDir::new();
        write(&t.path().join("f.txt"), "x");
        let allow_ls = |c: &str| c.starts_with("ls");
        let c = ExpandContext {
            project_dir: t.path().to_path_buf(),
            session_id: "s9".into(),
            allow_shell: Some(&allow_ls),
        };
        let out = expand_command(
            &cmd("Files: !`ls` sid: !`echo $ZEN_SESSION_ID` arg: $0"),
            "z",
            &c,
        );
        assert!(
            out.starts_with(
                "Files: f.txt sid: !`echo $ZEN_SESSION_ID` (not run: permission denied) arg: z"
            ),
            "{}",
            out
        );
        let all = |_: &str| true;
        let c = ExpandContext {
            project_dir: t.path().to_path_buf(),
            session_id: "s9".into(),
            allow_shell: Some(&all),
        };
        assert_eq!(
            expand_command(&cmd("sid=!`echo $ZEN_SESSION_ID`"), "", &c),
            "sid=s9"
        );
        let fail = expand_command(&cmd("!`echo oops >&2; exit 3`"), "", &c);
        assert!(
            fail.contains("oops") && fail.contains("(exit 3)"),
            "{}",
            fail
        );

        assert!(expand_command(&cmd("!`echo hi`"), "", &ctx(t.path())).contains("not run"));
    }

    #[test]
    fn c5_3_skill_wins_clash_user_invocable_false_hidden_and_skill_dir() {
        let tmp = TempDir::new();
        let home = tmp.path().join("home");
        let proj = tmp.path().join("proj");
        write(&proj.join(".zen/commands/deploy.md"), "COMMAND deploy");
        write(&proj.join(".zen/commands/bg.md"), "COMMAND bg");
        write(
            &proj.join(".zen/skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: Deploy it\nargument-hint: [env]\n---\nSKILL deploy $0 from ${ZEN_SKILL_DIR}\n",
        );
        write(
            &proj.join(".zen/skills/bg/SKILL.md"),
            "---\nname: bg\ndescription: Background knowledge\nuser-invocable: false\n---\nBG\n",
        );
        let all = discover_slash_commands(&proj, Some(&home), true);
        let deploy = all.iter().find(|c| c.name == "deploy").unwrap();
        assert_eq!(deploy.source, SlashSource::Skill);
        assert_eq!(deploy.argument_hint.as_deref(), Some("[env]"));
        assert!(
            all.iter().all(|c| c.name != "bg"),
            "user-invocable:false skill hides /bg (and shadows the command)"
        );
        let c = ctx(&proj);
        let (_, text) = expand_slash_prompt("/deploy prod", &proj, Some(&home), true, &c).unwrap();

        let want = format!(
            "SKILL deploy prod from {}",
            proj.join(".zen/skills/deploy").display()
        );
        assert_eq!(text.trim().replace('\\', "/"), want.replace('\\', "/"));

        write(
            &proj.join(".zen/skills/manual/SKILL.md"),
            "---\nname: manual\ndescription: M\ndisable-model-invocation: true\n---\nMANUAL\n",
        );
        assert!(find_slash_command(&proj, Some(&home), true, "manual").is_some());
    }

    #[test]
    fn c5_4_expand_slash_prompt_only_for_known_names() {
        let tmp = TempDir::new();
        let proj = tmp.path().join("proj");
        let home = tmp.path().join("home");
        write(&proj.join(".zen/commands/greet.md"), "Hello $0 and $1!");
        let c = ctx(&proj);
        let (cmd, text) =
            expand_slash_prompt("  /greet Alice Bob", &proj, Some(&home), true, &c).unwrap();
        assert_eq!(cmd.name, "greet");
        assert_eq!(text, "Hello Alice and Bob!");
        assert!(expand_slash_prompt("/nope x", &proj, Some(&home), true, &c).is_none());
        assert!(expand_slash_prompt("greet x", &proj, Some(&home), true, &c).is_none());
        assert!(expand_slash_prompt("/", &proj, Some(&home), true, &c).is_none());
    }

    // Issue #330: project commands (and skill-derived commands from
    // project skills) load only in a trusted folder; user commands
    // from ~/.zen/commands still load in an untrusted folder.
    #[test]
    fn issue_330_untrusted_folder_skips_project_commands() {
        let tmp = TempDir::new();
        let home = tmp.path().join("home");
        let proj = tmp.path().join("proj");
        write(&home.join(".zen/commands/mine.md"), "USER $ARGUMENTS");
        write(&proj.join(".zen/commands/theirs.md"), "PROJECT $ARGUMENTS");
        write(
            &proj.join(".zen/skills/pskill/SKILL.md"),
            "---\nname: pskill\ndescription: Project skill\n---\nPS\n",
        );
        write(
            &home.join(".zen/skills/uskill/SKILL.md"),
            "---\nname: uskill\ndescription: User skill\n---\nUS\n",
        );

        let untrusted = discover_slash_commands(&proj, Some(&home), false);
        let names: Vec<&str> = untrusted.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["mine", "uskill"],
            "project command and project-skill command must not load untrusted: {names:?}"
        );

        let trusted = discover_slash_commands(&proj, Some(&home), true);
        let names: Vec<&str> = trusted.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["mine", "pskill", "theirs", "uskill"]);
    }

    // Issue #330: a project-only slash command does not expand in an
    // untrusted folder.
    #[test]
    fn issue_330_untrusted_expand_slash_prompt_ignores_project_command() {
        let tmp = TempDir::new();
        let proj = tmp.path().join("proj");
        let home = tmp.path().join("home");
        write(&proj.join(".zen/commands/greet.md"), "Hello $0!");
        write(&home.join(".zen/commands/hi.md"), "Hi $0!");
        let c = ctx(&proj);

        assert!(
            expand_slash_prompt("/greet Bob", &proj, Some(&home), false, &c).is_none(),
            "project command must not expand in an untrusted folder"
        );
        let (_, text) = expand_slash_prompt("/hi Bob", &proj, Some(&home), false, &c).unwrap();
        assert_eq!(text, "Hi Bob!");
        assert!(
            expand_slash_prompt("/greet Bob", &proj, Some(&home), true, &c).is_some(),
            "project command expands once the folder is trusted"
        );
    }

    #[cfg(unix)]
    fn symlink(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    /// Issue #388: `.zen/commands/loop -> ..` used to recurse forever and
    /// overflow the stack at session start. Discovery must terminate and
    /// still find the real commands next to the cycle.
    #[test]
    #[cfg(unix)]
    fn issue_388_symlink_cycle_to_parent_terminates() {
        let tmp = TempDir::new();
        let proj = tmp.path().join("proj");
        write(&proj.join(".zen/commands/real.md"), "real");
        symlink(Path::new(".."), &proj.join(".zen/commands/loop"));
        let all = discover_custom_commands(&proj, Some(&tmp.path().join("home")), true);
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
    }

    /// Issue #388 follow-up (Leader): the same `loop -> ..` cycle must
    /// not let a sibling `.md` under `.zen` leak in as a command
    /// (before the root-escape guard it was collected as `loop/<name>`).
    #[test]
    #[cfg(unix)]
    fn issue_388_symlink_cycle_to_parent_does_not_leak_sibling_md() {
        let tmp = TempDir::new();
        let proj = tmp.path().join("proj");
        write(&proj.join(".zen/commands/real.md"), "real");
        write(&proj.join(".zen/extra.md"), "extra");
        symlink(Path::new(".."), &proj.join(".zen/commands/loop"));
        let all = discover_custom_commands(&proj, Some(&tmp.path().join("home")), true);
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
    }

    /// Issue #388: two directories linked to each other (`a/link -> ../b`,
    /// `b/link -> ../a`) must not recurse forever, and a directory reached
    /// through a symlinked alias must be walked only once.
    #[test]
    #[cfg(unix)]
    fn issue_388_mutual_symlink_cycle_and_alias_visited_once() {
        let tmp = TempDir::new();
        let proj = tmp.path().join("proj");
        let base = proj.join(".zen/commands");
        write(&base.join("cycle-a/one.md"), "one");
        write(&base.join("cycle-b/two.md"), "two");
        write(&base.join("shared/three.md"), "three");
        symlink(Path::new("../cycle-b"), &base.join("cycle-a/link"));
        symlink(Path::new("../cycle-a"), &base.join("cycle-b/link"));
        symlink(Path::new("shared"), &base.join("alias"));
        let all = discover_custom_commands(&proj, Some(&tmp.path().join("home")), true);
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        // Each real file exactly once; the alias walk must not duplicate
        // `shared/three.md`, whichever alias wins the sorted walk order.
        assert_eq!(names.len(), 3);
        assert!(names.iter().any(|n| n.ends_with("one")));
        assert!(names.iter().any(|n| n.ends_with("two")));
        assert_eq!(names.iter().filter(|n| n.ends_with("three")).count(), 1);
    }

    /// Issue #388: the depth cap bounds discovery in a non-cyclic but
    /// absurdly deep tree, so a deep tree cannot blow the stack either.
    #[test]
    fn issue_388_deep_tree_bounded_by_depth_cap() {
        let tmp = TempDir::new();
        let proj = tmp.path().join("proj");
        write(&proj.join(".zen/commands/shallow.md"), "shallow");
        let mut dir = proj.join(".zen/commands");
        for i in 0..20 {
            dir = dir.join(format!("d{i}"));
        }
        write(&dir.join("deep.md"), "deep");
        let all = discover_custom_commands(&proj, Some(&tmp.path().join("home")), true);
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["shallow"]);
    }

    /// Issue #388 follow-up (Leader): the depth cap is `depth > MAX`,
    /// so a file exactly at depth 16 is collected and one at depth 17
    /// is not. The deep-tree test above cannot tell `>` from `>=` or
    /// `==` (both missed mutants at commands.rs:95), because it only
    /// places files at depth 0 and depth 20; this test pins the exact
    /// boundary. Kills: `>` -> `==` and `>` -> `>=` in collect_md_inner.
    #[test]
    fn issue_388_depth_cap_boundary_is_exact() {
        let tmp = TempDir::new();
        let proj = tmp.path().join("proj");
        let mut dir = proj.join(".zen/commands");
        for i in 0..16 {
            dir = dir.join(format!("d{i}"));
        }
        write(&dir.join("at_cap.md"), "at cap");
        write(&dir.join("d16").join("past_cap.md"), "past cap");
        let all = discover_custom_commands(&proj, Some(&tmp.path().join("home")), true);
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        let expected = (0..16)
            .map(|i| format!("d{i}"))
            .chain(std::iter::once("at_cap".to_string()))
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(names, vec![expected.as_str()]);
    }

    /// Issue #392: a shell block that streams a lot of output must not
    /// buffer it unboundedly into the turn; captured output is capped at
    /// HOOK_OUTPUT_LIMIT chars and reports truncation.
    #[test]
    #[cfg(unix)]
    fn issue_392_shell_output_is_capped() {
        let t = TempDir::new();
        let allow = |_: &str| true;
        let c = ExpandContext {
            project_dir: t.path().to_path_buf(),
            session_id: "s392".into(),
            allow_shell: Some(&allow),
        };
        // `seq 1 3000` prints ~13_893 chars, past HOOK_OUTPUT_LIMIT.
        let out = expand_command(&cmd("!`seq 1 3000`"), "", &c);
        assert!(
            out.contains("truncated"),
            "over-limit shell output must report truncation, got {} chars: {:.200}",
            out.chars().count(),
            out
        );
        assert!(
            out.chars().count() <= crate::HOOK_OUTPUT_LIMIT + 120,
            "capped shell output must stay near the limit, got {} chars",
            out.chars().count()
        );
    }

    /// Issue #392: a shell block that never exits must time out instead of
    /// hanging the turn forever.
    #[test]
    #[cfg(unix)]
    fn issue_392_shell_block_times_out() {
        let t = TempDir::new();
        let c = ctx(t.path());
        let start = std::time::Instant::now();
        let out = run_shell_block("sleep 30", &c, std::time::Duration::from_secs(1));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "timed-out shell block must return promptly, took {:?}",
            start.elapsed()
        );
        assert!(
            out.contains("timed out"),
            "timed-out shell block must say so, got: {out}"
        );
    }

    /// Issue #392: `@path` inlining must cap file content at the same
    /// limit instead of reading whole files without bound.
    #[test]
    fn issue_392_inline_file_is_capped() {
        let t = TempDir::new();
        let big = "a".repeat(crate::HOOK_OUTPUT_LIMIT + 5_000);
        write(&t.path().join("big.txt"), &big);
        let c = ctx(t.path());
        let out = expand_command(&cmd("see @big.txt"), "", &c);
        assert!(
            out.contains("truncated"),
            "over-limit inlined file must report truncation, got {} chars",
            out.chars().count()
        );
        assert!(
            out.chars().count() <= crate::HOOK_OUTPUT_LIMIT + 120,
            "capped inlined file must stay near the limit, got {} chars",
            out.chars().count()
        );
    }
}
