//! Bounded, provider-specific classification of the current terminal frame.
//!
//! These are deliberately compiled rules. The classifier inspects the live
//! screen supplied by the pane, never raw ANSI chunks or accumulated output.

use crate::agent::AgentKind;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TerminalObservation {
    pub(crate) title: String,
    pub(crate) screen: String,
}

pub(super) enum TerminalEvidence {
    NeedsAttention(String),
    Working(Option<String>),
    Idle(Option<String>),
    Interrupted(String),
}

pub(super) fn classify(
    agent: AgentKind,
    observation: &TerminalObservation,
) -> Option<TerminalEvidence> {
    let title = observation.title.trim();
    let screen = observation.screen.to_lowercase();
    match agent {
        AgentKind::Codex => classify_codex(title, &screen),
        AgentKind::Claude => classify_claude(title, &screen),
        AgentKind::Opencode => classify_opencode(&screen),
        AgentKind::Omp => None,
    }
}

fn classify_codex(title: &str, screen: &str) -> Option<TerminalEvidence> {
    let title_lower = title.to_lowercase();
    let live = bottom_lines(screen, 14);
    let top = top_lines(screen, 20);
    if title_lower.contains("action required")
        || live.contains("press enter to confirm or esc to cancel")
        || live.contains("enter to submit answer")
        || live.contains("enter to submit all")
        || live.contains("allow command?")
        || (top.contains("do you trust the contents of") && top.contains("this directory"))
        || (live.contains("do you want to") && (live.contains("[y/n]") || live.contains("yes (y)")))
        || (live.contains("would you like to") && (live.contains("[y/n]") || live.contains("❯")))
    {
        return Some(TerminalEvidence::NeedsAttention(
            "Codex is waiting for input".to_owned(),
        ));
    }
    if live.contains("conversation interrupted") || live.contains("turn interrupted") {
        return Some(TerminalEvidence::Interrupted(
            "Codex was interrupted".to_owned(),
        ));
    }
    if title.split_whitespace().any(is_spinner)
        || (live.contains("working (") && live.contains("esc to interrupt"))
        || live.contains("press esc to interrupt")
    {
        return Some(TerminalEvidence::Working(Some(
            "Codex is working".to_owned(),
        )));
    }
    if !title.is_empty() {
        return Some(TerminalEvidence::Idle(Some("Codex is ready".to_owned())));
    }
    None
}

fn classify_claude(title: &str, screen: &str) -> Option<TerminalEvidence> {
    let live = bottom_lines(screen, 18);
    let form = live.contains("esc to cancel")
        && (live.contains("enter to confirm")
            || live.contains("enter to select")
            || live.contains("accept") && live.contains("decline"));
    if form
        || live.contains("run a dynamic workflow?")
        || live.contains("requests your input")
        || live.contains("do you want to proceed?")
        || live.contains("waiting for permission")
        || live.contains("do you want to allow this connection?")
        || live.contains("review your answers")
        || live.contains("skip interview and plan immediately")
    {
        return Some(TerminalEvidence::NeedsAttention(
            "Claude is waiting for input".to_owned(),
        ));
    }
    let first = title.chars().next();
    let title_working = first.is_some_and(|ch| {
        matches!(ch, '\u{25d0}'..='\u{25d3}') || ('\u{2800}'..='\u{28ff}').contains(&ch)
    });
    if live.contains("conversation interrupted") || live.contains("request interrupted") {
        return Some(TerminalEvidence::Interrupted(
            "Claude was interrupted".to_owned(),
        ));
    }
    if title_working
        || live.contains("esc to interrupt")
        || (live.contains("waiting for ") && live.contains("background agent"))
        || live.contains("mcp task") && live.contains("still running")
    {
        return Some(TerminalEvidence::Working(Some(
            "Claude is working".to_owned(),
        )));
    }
    let prompt_visible = live.lines().any(|line| line.trim_start().starts_with('❯'));
    if title.starts_with('✳') || prompt_visible {
        return Some(TerminalEvidence::Idle(Some("Claude is ready".to_owned())));
    }
    None
}

fn classify_opencode(screen: &str) -> Option<TerminalEvidence> {
    let live = bottom_lines(screen, 18);
    if live.contains("△ permission required")
        || (live.contains("esc dismiss")
            && (live.contains("enter confirm")
                || live.contains("enter submit")
                || live.contains("enter toggle")))
    {
        return Some(TerminalEvidence::NeedsAttention(
            "OpenCode is waiting for input".to_owned(),
        ));
    }
    if live.contains("conversation interrupted") || live.contains("request interrupted") {
        return Some(TerminalEvidence::Interrupted(
            "OpenCode was interrupted".to_owned(),
        ));
    }
    if live.contains("esc to interrupt")
        || live.contains("ctrl+c to interrupt")
        || live.contains("press esc to interrupt")
        || has_progress_bar(live)
    {
        return Some(TerminalEvidence::Working(Some(
            "OpenCode is working".to_owned(),
        )));
    }
    // OpenCode's prompt wording has changed across releases; these phrases
    // have remained input-box affordances rather than transcript content.
    if live.contains("ask anything") || live.contains("type a message") {
        return Some(TerminalEvidence::Idle(Some("OpenCode is ready".to_owned())));
    }
    None
}

fn bottom_lines(value: &str, count: usize) -> &str {
    value
        .match_indices('\n')
        .rev()
        .nth(count.saturating_sub(1))
        .map_or(value, |(index, _)| &value[index + 1..])
}

fn top_lines(value: &str, count: usize) -> &str {
    value
        .match_indices('\n')
        .nth(count.saturating_sub(1))
        .map_or(value, |(index, _)| &value[..index])
}

fn is_spinner(value: &str) -> bool {
    matches!(
        value,
        "⠋" | "⠙" | "⠹" | "⠸" | "⠼" | "⠴" | "⠦" | "⠧" | "⠇" | "⠏"
    )
}

fn has_progress_bar(screen: &str) -> bool {
    screen.lines().any(|line| {
        let run = line
            .chars()
            .fold((0_usize, 0_usize), |(best, current), ch| {
                if ch == '■' || ch == '⬝' {
                    (best.max(current + 1), current + 1)
                } else {
                    (best, 0)
                }
            })
            .0;
        run >= 4
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(title: &str, screen: &str) -> TerminalObservation {
        TerminalObservation {
            title: title.to_owned(),
            screen: screen.to_owned(),
        }
    }

    #[test]
    fn codex_prefers_attention_over_working() {
        let result = classify(
            AgentKind::Codex,
            &observation("⠋ Action Required", "Allow command?\nEsc to interrupt"),
        );
        assert!(matches!(result, Some(TerminalEvidence::NeedsAttention(_))));
    }

    #[test]
    fn claude_does_not_treat_plain_prompt_as_blocked() {
        let result = classify(AgentKind::Claude, &observation("✳ Claude", "❯ "));
        assert!(matches!(result, Some(TerminalEvidence::Idle(_))));
    }

    #[test]
    fn opencode_permission_and_progress_are_distinct() {
        assert!(matches!(
            classify(
                AgentKind::Opencode,
                &observation("", "△ Permission required\nenter confirm")
            ),
            Some(TerminalEvidence::NeedsAttention(_))
        ));
        assert!(matches!(
            classify(AgentKind::Opencode, &observation("", "■■■■")),
            Some(TerminalEvidence::Working(_))
        ));
    }

    #[test]
    fn stale_attention_above_live_region_does_not_block() {
        let old = "Allow command?\n";
        let filler = (0..15).map(|_| "old output\n").collect::<String>();
        let screen = format!("{old}{filler}ready");
        let result = classify(AgentKind::Codex, &observation("Codex", &screen));
        assert!(matches!(result, Some(TerminalEvidence::Idle(_))));
    }

    #[test]
    fn interruption_is_not_completion() {
        let result = classify(
            AgentKind::Claude,
            &observation("✳ Claude", "Conversation interrupted by user\n❯ "),
        );
        assert!(matches!(result, Some(TerminalEvidence::Interrupted(_))));
    }
}
