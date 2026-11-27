use serde_json::json;
use std::str::FromStr;
use zen_core::{FunctionDefinition, ToolDefinition, ToolRegistry};

pub fn estimate_schema_tokens(def: &ToolDefinition) -> u64 {
    serde_json::to_string(def)
        .map(|s| s.len() as u64 / 4)
        .unwrap_or(0)
}

pub fn total_schema_tokens(defs: &[ToolDefinition]) -> u64 {
    defs.iter().map(estimate_schema_tokens).sum()
}

pub fn per_tool_costs(defs: &[ToolDefinition]) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = defs
        .iter()
        .map(|d| (d.function.name.clone(), estimate_schema_tokens(d)))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolPhase {
    Core,

    Read,

    Write,

    Exec,
}

impl ToolPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolPhase::Core => "core",
            ToolPhase::Read => "read",
            ToolPhase::Write => "write",
            ToolPhase::Exec => "exec",
        }
    }
}

pub fn tool_phase(name: &str) -> Option<ToolPhase> {
    match name {
        "TodoWrite" => Some(ToolPhase::Core),
        "Read" | "Glob" | "Grep" | "WebFetch" | "WebSearch" => Some(ToolPhase::Read),
        "Write" | "Edit" | "MultiEdit" => Some(ToolPhase::Write),
        "Bash" | "JobOutput" | "JobKill" => Some(ToolPhase::Exec),
        // Sub-agent tools spawn children that can do anything the child
        // role allows, so they are Exec even though they report
        // `is_read_only() == true` (delegation itself mutates nothing;
        // the child does). A `core` bundle must not expose them.
        "Task" | "delegate" => Some(ToolPhase::Exec),
        // Loads skill instructions as text only.
        "Skill" => Some(ToolPhase::Read),
        // Browser tools (Ch.1): the four observers are read-only,
        // click/type mutate the page.
        "browser_navigate" | "browser_content" | "browser_screenshot" | "browser_wait" => {
            Some(ToolPhase::Read)
        }
        "browser_click" | "browser_type" => Some(ToolPhase::Exec),
        // LSP navigation tools are genuinely read-only.
        "goto_definition" | "hover" | "find_references" | "diagnostics" => Some(ToolPhase::Read),
        // MCP tools have no per-tool read-only flag visible here:
        // `McpTool::is_read_only()` is always `false` and `RemoteTool`
        // carries no read-only annotation, so every `mcp__*` tool is
        // conservatively Exec (full only in `all`/`exec` bundles).
        // Plumbing the server's own read-only annotation through is a
        // separate `zen-mcp` change; see issue #350.
        _ if name.starts_with("mcp__") => Some(ToolPhase::Exec),
        _ => None,
    }
}

/// Registry-aware phase: the name table first, then a fail-closed
/// fallback for any tool nobody classified yet — read-only tools map
/// to `Read`, everything else to `Exec` — so no registered tool ever
/// keeps its full schema in every bundle (issue #350). The name table
/// must win over the fallback: `Task`/`delegate` report
/// `is_read_only() == true` but spawn sub-agents, while `WebFetch` /
/// `WebSearch` report `false` but are read-phase tools, so
/// `is_read_only` alone cannot classify a tool.
pub fn tool_phase_for(registry: &ToolRegistry, name: &str) -> Option<ToolPhase> {
    if let Some(phase) = tool_phase(name) {
        return Some(phase);
    }
    registry.get(name).map(|tool| {
        if tool.is_read_only() {
            ToolPhase::Read
        } else {
            ToolPhase::Exec
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolBundle {
    #[default]
    All,

    Core,

    Read,

    Write,

    Exec,
}

impl ToolBundle {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolBundle::All => "all",
            ToolBundle::Core => "core",
            ToolBundle::Read => "read",
            ToolBundle::Write => "write",
            ToolBundle::Exec => "exec",
        }
    }

    fn full_phases(self) -> Option<&'static [ToolPhase]> {
        match self {
            ToolBundle::All => None,
            ToolBundle::Core => Some(&[ToolPhase::Core]),
            ToolBundle::Read => Some(&[ToolPhase::Core, ToolPhase::Read]),
            ToolBundle::Write => Some(&[ToolPhase::Core, ToolPhase::Write]),
            ToolBundle::Exec => Some(&[ToolPhase::Core, ToolPhase::Exec]),
        }
    }
}

impl FromStr for ToolBundle {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "all" => Ok(ToolBundle::All),
            "core" => Ok(ToolBundle::Core),
            "read" => Ok(ToolBundle::Read),
            "write" => Ok(ToolBundle::Write),
            "exec" => Ok(ToolBundle::Exec),
            other => Err(format!(
                "unknown tool bundle '{other}'; expected one of: all, core, read, write, exec"
            )),
        }
    }
}

pub const SCHEMA_TOKEN_BUDGETS: &[(ToolBundle, u64)] = &[
    (ToolBundle::Core, 1_000),
    (ToolBundle::Read, 1_700),
    (ToolBundle::Write, 1_500),
    (ToolBundle::Exec, 1_500),
];

pub fn deferred_stub(name: &str) -> ToolDefinition {
    ToolDefinition {
        tool_type: "function".into(),
        function: FunctionDefinition {
            name: name.to_string(),
            description: "[deferred] Full schema not loaded in the active tool bundle; \
                          do not call this tool."
                .to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        },
    }
}

pub fn definitions_for_bundle(registry: &ToolRegistry, bundle: ToolBundle) -> Vec<ToolDefinition> {
    let mut defs: Vec<ToolDefinition> = registry
        .definitions()
        .into_iter()
        .map(|d| {
            let name = d.function.name.as_str();
            let full = match bundle.full_phases() {
                None => true,
                Some(phases) => match tool_phase_for(registry, name) {
                    None => true,
                    Some(p) => phases.contains(&p),
                },
            };
            if full { d } else { deferred_stub(name) }
        })
        .collect();
    defs.sort_by(|a, b| a.function.name.cmp(&b.function.name));
    defs
}

pub fn is_deferred(registry: &ToolRegistry, bundle: ToolBundle, name: &str) -> bool {
    if registry.get(name).is_none() {
        return false;
    }
    match bundle.full_phases() {
        None => false,
        Some(phases) => match tool_phase_for(registry, name) {
            None => false,
            Some(p) => !phases.contains(&p),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use zen_core::{Tool, ToolContext, ToolOutput};

    struct FakeTool {
        name: &'static str,
        description: &'static str,
        read_only: bool,
    }

    #[async_trait::async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.description
        }
        fn parameters_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Target path for the operation."},
                    "limit": {"type": "integer", "description": "Maximum items to return."},
                    "options": {"type": "object", "description": "Extra options bag."},
                },
                "required": ["path"]
            })
        }
        fn is_read_only(&self) -> bool {
            self.read_only
        }
        async fn call(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
            ToolOutput::ok("ok")
        }
    }

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();

        for (name, desc, read_only) in [
            (
                "Write",
                "Create a file or overwrite it completely. Parent directories are created. \
                 If the file already exists it must have been read earlier in this session, \
                 otherwise the call is rejected to prevent accidental overwrites of unseen content.",
                false,
            ),
            (
                "Bash",
                "Run a shell command with bash -c in the session working directory. The directory \
                 is persistent across calls. Long output is cut to head and tail with the full text \
                 saved to a file whose path is reported, along with the exit code.",
                false,
            ),
            (
                "Read",
                "Read a text file from disk and return its lines with 1-based line numbers. Use \
                 offset and limit to page through large files without loading them fully.",
                true,
            ),
            (
                "TodoWrite",
                "Replace the session task list with a new one.",
                true,
            ),
            (
                // Read-only here on purpose: the name table must override
                // the fallback and classify these as Exec anyway.
                "Task",
                "Delegate a self-contained task to a sub-agent.",
                true,
            ),
            (
                "delegate",
                "Delegate work to sub-agents (Scout/Worker/Reviewer) in one session.",
                true,
            ),
            (
                // Read-only here on purpose: the `mcp__` prefix rule must
                // override the fallback and classify this as Exec anyway.
                "mcp__srv__ext",
                "External MCP tool with a longer description for realism, plus extra detail.",
                true,
            ),
        ] {
            r.register(Arc::new(FakeTool {
                name,
                description: desc,
                read_only,
            }));
        }
        r
    }

    #[test]
    fn tool_phase_classifies_every_known_tool() {
        assert_eq!(tool_phase("TodoWrite"), Some(ToolPhase::Core));
        assert_eq!(tool_phase("Read"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("Glob"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("Grep"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("WebFetch"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("WebSearch"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("Write"), Some(ToolPhase::Write));
        assert_eq!(tool_phase("Edit"), Some(ToolPhase::Write));
        assert_eq!(tool_phase("MultiEdit"), Some(ToolPhase::Write));
        assert_eq!(tool_phase("Bash"), Some(ToolPhase::Exec));
        assert_eq!(tool_phase("JobOutput"), Some(ToolPhase::Exec));
        assert_eq!(tool_phase("JobKill"), Some(ToolPhase::Exec));
        assert_eq!(tool_phase("Task"), Some(ToolPhase::Exec));
        assert_eq!(tool_phase("delegate"), Some(ToolPhase::Exec));
        assert_eq!(tool_phase("mcp__srv__ext"), Some(ToolPhase::Exec));
        assert_eq!(tool_phase("mcp__fs__read"), Some(ToolPhase::Exec));
        assert_eq!(tool_phase("Skill"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("goto_definition"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("hover"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("find_references"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("diagnostics"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("browser_navigate"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("browser_content"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("browser_screenshot"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("browser_wait"), Some(ToolPhase::Read));
        assert_eq!(tool_phase("browser_click"), Some(ToolPhase::Exec));
        assert_eq!(tool_phase("browser_type"), Some(ToolPhase::Exec));
        // The bare name table is intentionally partial: an unknown name
        // stays `None` here and only the registry-aware `tool_phase_for`
        // can classify it (via the `is_read_only` fallback).
        assert_eq!(tool_phase("Nope"), None);
    }

    #[test]
    fn bundle_parsing_roundtrip() {
        for (s, b) in [
            ("all", ToolBundle::All),
            ("core", ToolBundle::Core),
            ("read", ToolBundle::Read),
            ("write", ToolBundle::Write),
            ("exec", ToolBundle::Exec),
            ("READ", ToolBundle::Read),
        ] {
            assert_eq!(s.parse::<ToolBundle>().unwrap(), b);
            assert_eq!(b.as_str(), s.to_ascii_lowercase());
        }
        assert!("nope".parse::<ToolBundle>().is_err());
    }

    #[test]
    fn stable_ordering_sorted_by_name() {
        let r = registry();
        for bundle in [
            ToolBundle::All,
            ToolBundle::Core,
            ToolBundle::Read,
            ToolBundle::Write,
            ToolBundle::Exec,
        ] {
            let defs = definitions_for_bundle(&r, bundle);
            let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
            let mut sorted = names.clone();
            sorted.sort_unstable();
            assert_eq!(names, sorted, "bundle {:?} not sorted", bundle);

            let again = definitions_for_bundle(&r, bundle);
            let names2: Vec<&str> = again.iter().map(|d| d.function.name.as_str()).collect();
            assert_eq!(names, names2);
        }
    }

    #[test]
    fn phase_scoping_reduces_tokens() {
        let r = registry();
        let all = total_schema_tokens(&definitions_for_bundle(&r, ToolBundle::All));
        for bundle in [
            ToolBundle::Core,
            ToolBundle::Read,
            ToolBundle::Write,
            ToolBundle::Exec,
        ] {
            let scoped = total_schema_tokens(&definitions_for_bundle(&r, bundle));
            assert!(
                scoped < all,
                "bundle {:?}: scoped {} not < all {}",
                bundle,
                scoped,
                all
            );
        }
    }

    #[test]
    fn stubs_carry_no_schema_cost() {
        let stub = deferred_stub("Write");
        assert_eq!(stub.function.name, "Write");
        assert!(stub.function.description.contains("[deferred]"));

        assert!(estimate_schema_tokens(&stub) < 60);
    }

    // Regression test for issue #350. This replaces the old
    // `unclassified_tools_keep_full_schema`, which pinned the bug: it
    // asserted that an unclassified tool keeps its full schema in every
    // bundle, so `--tool-bundle read` still sent the full schema of an
    // MCP filesystem/shell tool and a `core` bundle still exposed
    // sub-agent spawning, while `Write` in the same bundle was stubbed
    // and rejected. That was inconsistent with the bundle's documented
    // purpose ("Restrict the run to a named tool bundle",
    // `zen-cli/src/cli.rs`) and with the `is_deferred` rejection in
    // `agent.rs`, so the old test was wrong, not the fix. Unclassified
    // is no longer a free pass: known names are classified above and
    // anything else registered falls back to `is_read_only`.
    #[test]
    fn unclassified_tools_are_gated() {
        let r = registry();
        for bundle in [ToolBundle::Core, ToolBundle::Read, ToolBundle::Write] {
            let defs = definitions_for_bundle(&r, bundle);
            for name in ["mcp__srv__ext", "Task", "delegate"] {
                let def = defs
                    .iter()
                    .find(|d| d.function.name == name)
                    .unwrap_or_else(|| panic!("{name} missing in {bundle:?}"));
                assert!(
                    def.function.description.contains("[deferred]"),
                    "{name} should be stubbed in {bundle:?}"
                );
                assert!(
                    is_deferred(&r, bundle, name),
                    "{name} should be deferred in {bundle:?}"
                );
            }
        }
        // Exec is the permissive non-`all` bundle: the tools are full
        // there (and in `all`), exactly like `Bash`.
        for bundle in [ToolBundle::Exec, ToolBundle::All] {
            let defs = definitions_for_bundle(&r, bundle);
            for name in ["mcp__srv__ext", "Task", "delegate"] {
                let def = defs
                    .iter()
                    .find(|d| d.function.name == name)
                    .unwrap_or_else(|| panic!("{name} missing in {bundle:?}"));
                assert!(
                    !def.function.description.contains("[deferred]"),
                    "{name} should be full in {bundle:?}"
                );
                assert!(!is_deferred(&r, bundle, name));
            }
        }
    }

    #[test]
    fn registry_fallback_gates_unknown_tools() {
        let mut r = ToolRegistry::new();
        for (name, read_only) in [("MysteryRead", true), ("MysteryMut", false)] {
            r.register(Arc::new(FakeTool {
                name,
                description: "A tool no name table entry covers.",
                read_only,
            }));
        }
        // A read-only unknown tool behaves like a read-phase tool.
        assert_eq!(tool_phase_for(&r, "MysteryRead"), Some(ToolPhase::Read));
        assert!(!is_deferred(&r, ToolBundle::Read, "MysteryRead"));
        assert!(is_deferred(&r, ToolBundle::Write, "MysteryRead"));
        assert!(is_deferred(&r, ToolBundle::Exec, "MysteryRead"));
        assert!(is_deferred(&r, ToolBundle::Core, "MysteryRead"));
        // A mutating unknown tool behaves like an exec-phase tool.
        assert_eq!(tool_phase_for(&r, "MysteryMut"), Some(ToolPhase::Exec));
        assert!(!is_deferred(&r, ToolBundle::Exec, "MysteryMut"));
        assert!(is_deferred(&r, ToolBundle::Read, "MysteryMut"));
        assert!(is_deferred(&r, ToolBundle::Write, "MysteryMut"));
        assert!(is_deferred(&r, ToolBundle::Core, "MysteryMut"));
        // Neither keeps its full schema in a bundle that excludes it.
        let read_defs = definitions_for_bundle(&r, ToolBundle::Read);
        let mut_def = read_defs
            .iter()
            .find(|d| d.function.name == "MysteryMut")
            .expect("MysteryMut missing");
        assert!(mut_def.function.description.contains("[deferred]"));
        // `tool_phase` alone still returns `None` for unknown names;
        // only the registry-aware lookup classifies them.
        assert_eq!(tool_phase("MysteryRead"), None);
        assert_eq!(tool_phase("MysteryMut"), None);
    }

    #[test]
    fn is_deferred_matches_bundle_contents() {
        let r = registry();

        assert!(!is_deferred(&r, ToolBundle::Read, "Read"));
        assert!(!is_deferred(&r, ToolBundle::Read, "TodoWrite"));
        assert!(is_deferred(&r, ToolBundle::Read, "Write"));
        assert!(is_deferred(&r, ToolBundle::Read, "Bash"));

        assert!(!is_deferred(&r, ToolBundle::All, "Write"));

        assert!(!is_deferred(&r, ToolBundle::Read, "Nope"));
    }

    #[test]
    fn per_tool_costs_sorted_desc() {
        let r = registry();
        let defs = definitions_for_bundle(&r, ToolBundle::All);
        let costs = per_tool_costs(&defs);
        assert_eq!(costs.len(), defs.len());
        for w in costs.windows(2) {
            assert!(w[0].1 >= w[1].1, "costs not descending: {:?}", costs);
        }

        let names: Vec<&str> = costs.iter().map(|(n, _)| n.as_str()).collect();
        for expected in ["Bash", "Read", "Write", "TodoWrite", "mcp__srv__ext"] {
            assert!(names.contains(&expected), "missing {}", expected);
        }

        assert!(costs.iter().all(|(_, t)| *t > 0));
    }
}
