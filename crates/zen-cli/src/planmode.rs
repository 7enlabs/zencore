use zen_agent::AgentConfig;
use zen_core::PermissionMode;

pub const PLAN_BADGE: &str = "[PLAN]";

/// Approve words match exactly: "yes please", "approved", and "approve the
/// plan" are all rejected, so an ambiguous or past-tense reply can never
/// accidentally approve a plan (plan mode gates real file changes).
pub fn is_approve_word(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "approve" | "yes")
}

/// Strip plan-exit markers only from the end: a marker mentioned inside the
/// plan body is plan content and must survive, only a trailing marker is an
/// instruction to leave plan mode.
pub fn strip_plan_exit_markers(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    while let Some(last) = lines.last() {
        let t = last.trim().trim_matches('`').to_ascii_lowercase();
        if t == "exitplanmode" || t == "/exitplan" {
            lines.pop();
        } else {
            break;
        }
    }
    lines.join("\n").trim_end().to_string()
}

/// Deliver plan-mode text: strip the trailing exit marker when the run
/// is in plan mode and the text carries it. In any other mode the text
/// is returned unchanged — the marker is only meaningful in plan mode,
/// so a model that writes it as prose outside plan mode keeps it.
/// The transcript keeps the raw model text; only delivery goes through
/// here.
pub fn deliver_plan_text(mode: PermissionMode, text: &str) -> String {
    if mode == PermissionMode::Plan && zen_agent::plan::looks_like_plan_exit(text) {
        strip_plan_exit_markers(text)
    } else {
        text.to_string()
    }
}
/// instead of appending or stripping a banner: toggling plan mode twice
/// must never stack the plan banner, and leaving plan mode must restore the
/// exact base prompt.
pub fn set_permission_mode(
    config: &mut AgentConfig,
    mode: PermissionMode,
    base_system_prompt: &str,
) {
    config.permission_mode = mode;
    config.system_prompt = zen_agent::plan::system_prompt_for_mode(base_system_prompt, mode);
}

pub fn prompt_for(mode: PermissionMode) -> &'static str {
    if mode == PermissionMode::Plan {
        "[PLAN] > "
    } else {
        "> "
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approve_words_match_exactly() {
        assert!(is_approve_word("approve"));
        assert!(is_approve_word("YES"));
        assert!(is_approve_word("  Approve  "));
        assert!(!is_approve_word("yes please"));
        assert!(!is_approve_word("approved"));
        assert!(!is_approve_word("approve the plan"));
        assert!(!is_approve_word(""));
        assert!(!is_approve_word("no"));
    }

    #[test]
    fn strip_markers_only_trailing() {
        assert_eq!(strip_plan_exit_markers("1. do X\nExitPlanMode"), "1. do X");
        assert_eq!(
            strip_plan_exit_markers("1. do X\n`ExitPlanMode`"),
            "1. do X"
        );
        assert_eq!(strip_plan_exit_markers("plan\n/exitplan\n"), "plan");
        assert_eq!(
            strip_plan_exit_markers("1. do X\nExitPlanMode\n/exitplan"),
            "1. do X"
        );

        assert_eq!(
            strip_plan_exit_markers("ExitPlanMode\n1. do X"),
            "ExitPlanMode\n1. do X"
        );
        assert_eq!(strip_plan_exit_markers("no markers"), "no markers");
        assert_eq!(strip_plan_exit_markers(""), "");
    }

    #[test]
    fn deliver_strips_marker_only_in_plan_mode() {
        assert_eq!(
            deliver_plan_text(PermissionMode::Plan, "1. do X\nExitPlanMode"),
            "1. do X"
        );
        assert_eq!(
            deliver_plan_text(PermissionMode::Default, "1. do X\nExitPlanMode"),
            "1. do X\nExitPlanMode"
        );
        assert_eq!(
            deliver_plan_text(PermissionMode::Plan, "1. do X"),
            "1. do X"
        );
        assert_eq!(
            deliver_plan_text(PermissionMode::Plan, "ExitPlanMode\n1. do X"),
            "ExitPlanMode\n1. do X"
        );
    }

    #[test]
    fn prompt_badge_only_in_plan_mode() {
        assert_eq!(prompt_for(PermissionMode::Plan), "[PLAN] > ");
        assert_eq!(prompt_for(PermissionMode::Default), "> ");
        assert_eq!(prompt_for(PermissionMode::AcceptEdits), "> ");
        assert_eq!(prompt_for(PermissionMode::BypassPermissions), "> ");
        assert_eq!(prompt_for(PermissionMode::DontAsk), "> ");
        assert_eq!(prompt_for(PermissionMode::AskAll), "> ");
        assert!(prompt_for(PermissionMode::Plan).contains(PLAN_BADGE));
    }

    #[test]
    fn toggle_on_off_is_clean_and_idempotent() {
        let base = "you are a test";
        let mut config = AgentConfig {
            permission_mode: PermissionMode::Default,
            system_prompt: base.to_string(),
            ..Default::default()
        };

        set_permission_mode(&mut config, PermissionMode::Plan, base);
        assert_eq!(config.permission_mode, PermissionMode::Plan);
        assert!(config.system_prompt.contains("# Plan mode (active)"));
        assert_eq!(
            config.system_prompt.matches("# Plan mode (active)").count(),
            1
        );

        set_permission_mode(&mut config, PermissionMode::Plan, base);
        assert_eq!(
            config.system_prompt.matches("# Plan mode (active)").count(),
            1
        );

        set_permission_mode(&mut config, PermissionMode::AcceptEdits, base);
        assert_eq!(config.permission_mode, PermissionMode::AcceptEdits);
        assert_eq!(config.system_prompt, base);
        assert!(!config.system_prompt.contains("# Plan mode (active)"));
    }
}
