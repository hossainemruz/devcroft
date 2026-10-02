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
    /// The prompt is ready, but visible history cannot confirm the turn result.
    Ready(Option<String>),
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
    // V2 keeps the composer after a turn, but removes its placeholder. Read
    // only its footer so transcript and draft text cannot override readiness.
    if let Some((transcript, footer)) = opencode_composer(live) {
        if opencode_working(footer) || footer.contains("[⋯]") {
            return Some(TerminalEvidence::Working(Some(
                "OpenCode is working".to_owned(),
            )));
        }
        // When scrolled away, the summary above this navigation control can
        // belong to an older turn. Report readiness without inferring its result.
        if transcript
            .lines()
            .any(|line| line.trim() == "jump to latest ↓")
        {
            return Some(TerminalEvidence::Ready(Some(
                "OpenCode is ready".to_owned(),
            )));
        }
        // V2 appends this suffix to the latest assistant's turn summary.
        // An older interrupted turn must not suppress a newer completion.
        let latest = transcript
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('┃'));
        if latest.is_some_and(|line| line.ends_with(" · interrupted")) {
            return Some(TerminalEvidence::Interrupted(
                "OpenCode was interrupted".to_owned(),
            ));
        }
        return Some(TerminalEvidence::Idle(Some("OpenCode is ready".to_owned())));
    }
    if live.contains("△ permission required")
        || ((live.contains("esc dismiss") || live.contains("esc close")) && live.contains("enter "))
        || (live.contains("enter confirm")
            && (live.contains("⇆ select")
                || (live.contains("reject permission") && live.contains("esc cancel"))))
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
    if opencode_working(live) {
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

fn opencode_working(screen: &str) -> bool {
    screen.contains("esc to interrupt")
        || screen.contains("ctrl+c to interrupt")
        || screen.contains("esc interrupt")
        || screen.contains("again to interrupt")
        || has_progress_bar(screen)
}

/// V2's prompt ends with two left-bordered rows, its metadata, a border row,
/// and a single footer row. Transparent themes render that border as spaces.
/// Require this structure at the bottom, rather than a corner in prose.
fn opencode_composer(screen: &str) -> Option<(&str, &str)> {
    let mut previous = "";
    let mut before_previous = "";
    let mut offset = 0;
    for line in screen.split_inclusive('\n') {
        let row = line.trim();
        let border = row
            .strip_prefix('╹')
            .is_some_and(|rest| rest.chars().all(|ch| ch == '▀'));
        let transparent = row.is_empty()
            && previous
                .strip_prefix('┃')
                .is_some_and(|text| !text.trim().is_empty())
            && !previous.contains("enter ")
            && !previous.contains("esc ");
        if (border || transparent) && previous.starts_with('┃') && before_previous.starts_with('┃')
        {
            let footer = &screen[offset + line.len()..];
            if footer
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
                <= 1
            {
                return Some((&screen[..offset], footer));
            }
        }
        before_previous = previous;
        previous = row;
        offset += line.len();
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
    fn opencode_v2_busy_footer_is_working() {
        // The v2 composer status bar while a turn runs: an eight-cell
        // `■`/`⬝` progress bar and the `esc interrupt` hint.
        let busy = "\n  ┃\n  ┃  Reply with exactly: ok\n  ┃\n\n\n\n\n\n\n\n\n\n  ┃\n  ┃\n  ┃\n  ┃  General · DeepSeek V4.1 Flash OpenCode Go · max\n  ╹▀▀▀▀▀▀▀▀▀▀▀▀\n   ■■■⬝⬝⬝⬝⬝ esc interrupt                                        shift+tab agents  ctrl+p commands";
        assert!(matches!(
            classify(AgentKind::Opencode, &observation("OpenCode", busy)),
            Some(TerminalEvidence::Working(_))
        ));
        // Some frames carry only the hint (the bar fills or is absent), so
        // each affordance must stand alone.
        let hint_only = "  ┃\n  ┃  General · DeepSeek V4.1 Flash OpenCode Go · max\n  ╹▀▀▀▀▀▀\n   esc interrupt                      122.5K (12%) · $0.06  ctrl+p commands";
        assert!(matches!(
            classify(AgentKind::Opencode, &observation("OpenCode", hint_only)),
            Some(TerminalEvidence::Working(_))
        ));
        // The first Escape arms the interrupt confirmation for five seconds;
        // on a wide pane the status line keeps its details while the hint
        // flips, so the armed wording must stay working.
        let armed = "  ┃\n  ┃  General · DeepSeek V4.1 Flash OpenCode Go · max\n  ╹▀▀▀▀▀▀\n  [⋯] esc again to interrupt           122.5K (12%) · $0.06  ctrl+p commands";
        assert!(matches!(
            classify(AgentKind::Opencode, &observation("OpenCode", armed)),
            Some(TerminalEvidence::Working(_))
        ));
    }

    #[test]
    fn opencode_v2_settled_composer_is_idle() {
        // Regression: after a completed turn OpenCode v2 redraws the
        // composer without the `Ask anything…` placeholder and without the
        // busy affordances. The composer structure alone must read as idle,
        // otherwise the pane stays stuck on the last Working projection
        // while it sits at the prompt.
        let settled = "\n  ┃\n  ┃  Reply with exactly: ok\n  ┃\n\n     ok\n\n     General · DeepSeek V4.1 Flash · 3.5s · 0.6 tok/s\n\n\n\n\n\n\n\n\n\n  ┃\n  ┃\n  ┃\n  ┃  General · DeepSeek V4.1 Flash OpenCode Go · max\n  ╹▀▀▀▀▀▀▀▀▀▀▀▀\n  /private/tmp/checkout                     8.9K (1%) · $0.00  ctrl+p commands";
        assert!(matches!(
            classify(AgentKind::Opencode, &observation("OpenCode", settled)),
            Some(TerminalEvidence::Idle(_))
        ));
        // Narrow panes drop the shortcut hint and, once the usage readout no
        // longer fits either, render no details at all. The composer corner
        // must still identify the settled frame.
        let usage_only =
            "  ┃\n  ┃  General · DeepSeek V4.1 Flash OpenCode Go · max\n  ╹\n  8.7K (1%) · $0.00";
        assert!(matches!(
            classify(AgentKind::Opencode, &observation("OpenCode", usage_only)),
            Some(TerminalEvidence::Idle(_))
        ));
        let no_details = "  ┃\n  ┃  General · DeepSeek V4.1 Flash OpenCode Go · max\n  ╹\n";
        assert!(matches!(
            classify(AgentKind::Opencode, &observation("OpenCode", no_details)),
            Some(TerminalEvidence::Idle(_))
        ));
        // The same status bar with a live progress bar stays working.
        let busy = "  ┃\n  ┃  General · DeepSeek V4.1 Flash OpenCode Go · max\n  ╹▀▀▀▀▀▀\n   ■■■⬝⬝⬝⬝⬝ esc interrupt                      122.5K (12%) · $0.06  ctrl+p commands";
        assert!(matches!(
            classify(AgentKind::Opencode, &observation("OpenCode", busy)),
            Some(TerminalEvidence::Working(_))
        ));
    }

    #[test]
    fn opencode_without_a_live_footer_has_no_verdict() {
        // Transcript frames without the composer must stay undecided: the
        // idle fallback is anchored on the composer structure, not on the
        // absence of the placeholder alone.
        let transcript = "  ⏺ execute\n    > rg activity src/\n\n  Everything is green.\n";
        assert!(classify(AgentKind::Opencode, &observation("OpenCode", transcript)).is_none());
        // A composer picker replaces the prompt while a turn keeps running;
        // its shell output may carry percentage-looking text that must not
        // be mistaken for the composer status line.
        let picker =
            "  ⏺ shell\n    > printf 'Progress (50%)\\n'; sleep 30\n\n    Progress (50%)\n";
        assert!(classify(AgentKind::Opencode, &observation("OpenCode", picker)).is_none());
        // Prose that merely mentions the composer corner is not the composer:
        // the fallback requires the border row's exact shape.
        let prose = "  the composer corner ╹▀ marks the prompt\n";
        assert!(classify(AgentKind::Opencode, &observation("OpenCode", prose)).is_none());
    }

    #[test]
    fn opencode_v2_footer_wins_over_transcript_and_draft_text() {
        for stale in [
            "△ Permission required",
            "esc dismiss enter confirm",
            "conversation interrupted",
            "esc interrupt",
            "■■■■",
            "ask anything",
        ] {
            let screen = format!(
                "  {stale}\n  ┃\n  ┃  {stale}\n  ┃  General · Model\n  ╹▀▀▀▀\n  /repo ctrl+p commands"
            );
            assert!(
                matches!(
                    classify(AgentKind::Opencode, &observation("OpenCode", &screen)),
                    Some(TerminalEvidence::Idle(_))
                ),
                "stale text: {stale}"
            );
        }
    }

    #[test]
    fn opencode_v2_narrow_and_transparent_composers() {
        for border in ["╹▀▀▀▀", ""] {
            for metadata in ["General · Model", "Model", "Shell"] {
                for footer in ["[⋯]", "■■■■", "esc interrupt", "esc again to interrupt"] {
                    let screen = format!("  ┃\n  ┃\n  ┃  {metadata}\n  {border}\n  {footer}\n\n");
                    assert!(
                        matches!(
                            classify(AgentKind::Opencode, &observation("OpenCode", &screen)),
                            Some(TerminalEvidence::Working(_))
                        ),
                        "metadata: {metadata}, footer: {footer}"
                    );
                }
                for draft in ["", "draft", "esc interrupt", "△ Permission required"] {
                    let idle =
                        format!("  ┃\n  ┃  {draft}\n  ┃  {metadata}\n  {border}\n  /repo\n\n");
                    assert!(
                        matches!(
                            classify(AgentKind::Opencode, &observation("OpenCode", &idle)),
                            Some(TerminalEvidence::Idle(_))
                        ),
                        "draft: {draft}"
                    );
                }
            }
        }
    }

    #[test]
    fn opencode_v2_permission_rejection_and_form_actions_need_attention() {
        for screen in [
            "  ┃  △ Reject permission\n  ┃  Tell OpenCode what to do differently\n  ┃  enter confirm  esc cancel",
            // A tall permission diff can push its header out of the observed region.
            "  ┃  + changed line\n  ┃  Allow once  Reject  ⇆ select  enter confirm",
            "  ┃  enter edit  esc dismiss",
            "  ┃  enter done  esc close",
            "  ┃  enter open link  c copy  esc dismiss",
            "  ┃  enter I finished  c copy  esc dismiss",
            "  ┃  enter continue  c copy  esc dismiss",
            "  ┃  enter save  esc dismiss",
        ] {
            // Real terminal observations include the empty rows below forms.
            let padded = format!("{screen}\n\n\n");
            for frame in [screen, &padded] {
                assert!(
                    matches!(
                        classify(AgentKind::Opencode, &observation("OpenCode", frame)),
                        Some(TerminalEvidence::NeedsAttention(_))
                    ),
                    "screen: {frame}"
                );
            }
        }
    }

    #[test]
    fn opencode_v2_latest_turn_interruption_is_not_completion() {
        let prompt = "  ┃\n  ┃\n  ┃  General · Model\n  ╹▀▀▀▀\n  /repo";
        let interrupted = format!("  General · Model · 1.5s · interrupted\n\n{prompt}");
        assert!(matches!(
            classify(AgentKind::Opencode, &observation("OpenCode", &interrupted)),
            Some(TerminalEvidence::Interrupted(_))
        ));
        let jump = interrupted.replace("\n\n", "\n\n  Jump to latest ↓\n");
        assert!(matches!(
            classify(AgentKind::Opencode, &observation("OpenCode", &jump)),
            Some(TerminalEvidence::Ready(_))
        ));
        let completed =
            format!("  General · Model · interrupted\n  ok\n  General · Model · 2s\n\n{prompt}");
        assert!(matches!(
            classify(AgentKind::Opencode, &observation("OpenCode", &completed)),
            Some(TerminalEvidence::Idle(_))
        ));
        let working = interrupted.replace("/repo", "[⋯] esc interrupt");
        assert!(matches!(
            classify(AgentKind::Opencode, &observation("OpenCode", &working)),
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
