//! The vault source Find / replacement preview view (RFC-0006 S07).
//!
//! Rendering only: host state lives in [`crate::host::replace`]. The view
//! makes the source/full-text distinction explicit, shows the frozen
//! scope, truncated scope, typed exclusions and exact totals, and exposes
//! sealed-preview candidates (before/after hashes) with selection.

use crate::app::{App, Message};
use crate::host::replace::{
    CandidateView, FindRowView, ReplaceField, ReplaceFlag, ReplacePanelMode, SelectAction,
};
use crate::style;
use crate::workspace::tab_strip;
use flavite_workspace::TabId;
use iced::widget::{Space, button, checkbox, column, container, row, scrollable, text, text_input};
use iced::{Element, Length};

/// Input widget id prefix for semantic tests.
pub const REPLACE_INPUT_ID_PREFIX: &str = "replace-input-";

fn input_id(tab: TabId, suffix: &str) -> iced::widget::Id {
    iced::widget::Id::from(format!("{REPLACE_INPUT_ID_PREFIX}{}{suffix}", tab.raw()))
}

/// Render the Replace view for one tab.
#[allow(clippy::too_many_lines)]
pub fn view(app: &App, tab: TabId) -> Element<'_, Message> {
    let metrics = app.appearance.metrics();
    let state = app.replace.view_state(tab);
    let query = state.query.clone();
    let replacement = state.replacement.clone();
    let mode_button = |label: &'static str, mode: ReplacePanelMode| {
        let active = state.mode == mode;
        button(text(label))
            .padding(tab_strip::chip_padding())
            .style(move |theme, status| {
                if active {
                    style::list_row(true)(theme, status)
                } else {
                    style::ghost()(theme, status)
                }
            })
            .on_press_maybe((!active).then_some(Message::ReplaceSetMode { tab, mode }))
    };
    let toggle = |label: &'static str, active: bool, message: Message| {
        button(text(label).style(if active {
            |theme: &iced::Theme| iced::widget::text::Style {
                color: Some(theme.palette().text),
            }
        } else {
            style::dim_text
        }))
        .padding(tab_strip::chip_padding())
        .style(move |theme, status| {
            if active {
                style::list_row(true)(theme, status)
            } else {
                style::ghost()(theme, status)
            }
        })
        .on_press(message)
    };

    let mut controls = column![
        row![
            text_input("Find in saved note source…", &query)
                .id(input_id(tab, "-query"))
                .on_input(move |value| Message::ReplaceEdit {
                    tab,
                    field: ReplaceField::Query,
                    value,
                })
                .on_submit(Message::ReplaceSubmitNow { tab })
                .padding(tab_strip::chip_padding())
                .size(metrics.text_sm)
                .width(Length::Fill),
            toggle(
                "Regex",
                state.regex,
                Message::ReplaceToggle {
                    tab,
                    flag: ReplaceFlag::Regex
                }
            ),
            toggle(
                "Aa",
                state.case_sensitive,
                Message::ReplaceToggle {
                    tab,
                    flag: ReplaceFlag::CaseSensitive
                }
            ),
            toggle(
                "Word",
                state.whole_word,
                Message::ReplaceToggle {
                    tab,
                    flag: ReplaceFlag::WholeWord
                }
            ),
        ]
        .spacing(metrics.spacing_sm),
    ];
    if state.mode == ReplacePanelMode::Preview {
        controls = controls.push(
            text_input("Replacement text (literal; $1/$0 in regex)", &replacement)
                .id(input_id(tab, "-replacement"))
                .on_input(move |value| Message::ReplaceEdit {
                    tab,
                    field: ReplaceField::Replacement,
                    value,
                })
                .padding(tab_strip::chip_padding())
                .size(metrics.text_sm)
                .width(Length::Fill),
        );
    }
    let status = if state.updating {
        String::from(if state.mode == ReplacePanelMode::Preview {
            "Preparing sealed preview…"
        } else {
            "Scanning saved sources…"
        })
    } else if let Some(error) = &state.error {
        error.clone()
    } else if let Some((epoch, generation)) = state.snapshot {
        let mut parts = vec![format!(
            "Frozen snapshot {epoch}/{generation} · {} notes · {} matches",
            state.total_notes.unwrap_or(0),
            state.total_matches.unwrap_or(0)
        )];
        if state.truncated_scope {
            parts.push("scope truncated at the note bound".to_owned());
        }
        if !state.exclusions.is_empty() {
            parts.push(format!("excluded: {}", state.exclusions.len()));
        }
        parts.push("saved content only — unsaved edits are not scanned".to_owned());
        parts.join(" · ")
    } else if let Some(plan) = &state.plan {
        format!(
            "Sealed preview · {} matched notes · {} matches · {} excluded ({} drafts) · {} payload bytes · expires {}",
            plan.matched_notes,
            plan.matches,
            plan.excluded,
            plan.drafts,
            plan.payload_bytes,
            plan.expires_at_unix_ms,
        )
    } else {
        String::from(
            "Source matching covers serialized Markdown source — code blocks, frontmatter and link targets included. Vault Find/Replace reads saved notes only.",
        )
    };

    let mut body = column![].spacing(2);
    if state.mode == ReplacePanelMode::Find {
        for result in &state.rows {
            body = body.push(find_row(*metrics, tab, result));
        }
        if state.rows.is_empty() && state.error.is_none() && !state.updating {
            body = body.push(
                text(if state.query.trim().is_empty() {
                    "Source find matches saved Markdown source across the whole vault."
                } else {
                    "No saved source matches."
                })
                .style(style::dim_text),
            );
        }
    } else {
        for candidate in &state.candidates {
            body = body.push(candidate_row(*metrics, tab, state, candidate));
        }
        if state.candidates.is_empty() && state.error.is_none() && !state.updating {
            body = body.push(
                text("Run a preview to seal exact before/after payloads.").style(style::dim_text),
            );
        }
    }

    let mut actions = row![
        mode_button("Find", ReplacePanelMode::Find),
        mode_button("Preview", ReplacePanelMode::Preview),
        Space::new().width(Length::Fill).height(Length::Fixed(0.0)),
    ]
    .spacing(metrics.spacing_sm);
    match state.mode {
        ReplacePanelMode::Find => {
            if state.has_next() {
                actions = actions.push(
                    button(text("Next page"))
                        .padding(tab_strip::chip_padding())
                        .style(style::ghost())
                        .on_press(Message::ReplaceNextPage { tab }),
                );
            }
            actions = actions.push(
                button(text("Find in vault"))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::ReplaceSubmitNow { tab }),
            );
        }
        ReplacePanelMode::Preview => {
            actions = actions.push(
                button(text("Run preview"))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::ReplaceRunPreview { tab }),
            );
            if state.preview_id.is_some() {
                actions = actions.push(
                    button(text("Select all"))
                        .padding(tab_strip::chip_padding())
                        .style(style::ghost())
                        .on_press(Message::ReplaceSelect {
                            tab,
                            selection: SelectAction::All,
                        }),
                );
                actions = actions.push(
                    button(text("Select none"))
                        .padding(tab_strip::chip_padding())
                        .style(style::ghost())
                        .on_press(Message::ReplaceSelect {
                            tab,
                            selection: SelectAction::None,
                        }),
                );
            }
        }
    }
    if state.mode == ReplacePanelMode::Preview
        && state.selection_ready
        && let Some(plan) = &state.plan
        && let Some(revision) = plan.selection_revision
    {
        actions = actions.push(
            text(format!(
                "Selection rev {revision} · {} notes",
                state.selected_count()
            ))
            .size(metrics.text_sm)
            .style(style::dim_text),
        );
        if state.can_apply() {
            actions = actions.push(
                button(text("Apply"))
                    .padding(tab_strip::chip_padding())
                    .style(style::list_row(true))
                    .on_press(Message::ReplaceApply { tab }),
            );
        }
    }
    if let Some(job) = &state.job {
        let mut job_line = format!(
            "Job {} · {} · saved {} · conflicts {} · stale {}",
            &job.job_id[..8],
            job.phase,
            job.saved,
            job.conflicts,
            job.stale
        );
        if job.index_pending > 0 {
            job_line.push_str(&format!(" · index pending {}", job.index_pending));
        }
        if job.not_started > 0 {
            job_line.push_str(&format!(" · not started {}", job.not_started));
        }
        actions = actions.push(text(job_line).size(metrics.text_sm).style(style::dim_text));
        if state.can_cancel() {
            actions = actions.push(
                button(text("Cancel"))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::ReplaceCancel { tab }),
            );
            if matches!(
                job.phase.as_str(),
                "paused" | "partial" | "cancelled" | "applying"
            ) {
                actions = actions.push(
                    button(text("Resume"))
                        .padding(tab_strip::chip_padding())
                        .style(style::ghost())
                        .on_press(Message::ReplaceResume { tab }),
                );
            }
        }
    }

    container(
        column![
            controls,
            text(status).size(metrics.text_sm).style(style::dim_text),
            container(scrollable(body).height(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(metrics.spacing_sm)
                .style(style::card),
            actions,
        ]
        .spacing(metrics.spacing_sm),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(metrics.spacing_lg)
    .style(style::content)
    .into()
}

fn find_row(
    metrics: crate::style::Metrics,
    tab: TabId,
    result: &FindRowView,
) -> Element<'static, Message> {
    let open = Message::ReplaceOpenNote {
        tab,
        path: result.path.clone(),
    };
    let mut body = column![
        text(if result.title.is_empty() {
            result.path.clone()
        } else {
            format!("{} — {}", result.title, result.path)
        }),
        text(format!("{} match(es)", result.note_matches))
            .size(metrics.text_sm)
            .style(style::dim_text),
    ]
    .spacing(2);
    let preview: Vec<String> = result
        .matches
        .iter()
        .take(24)
        .map(|(start, end)| format!("{start}–{end}"))
        .collect();
    if !preview.is_empty() {
        body = body.push(
            text(preview.join(" "))
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    button(body)
        .width(Length::Fill)
        .padding(iced::Padding {
            top: 6.0,
            bottom: 6.0,
            left: 10.0,
            right: 10.0,
        })
        .style(style::list_row(false))
        .on_press(open)
        .into()
}

fn candidate_row(
    _metrics: crate::style::Metrics,
    tab: TabId,
    state: &crate::host::replace::ReplaceTabState,
    candidate: &CandidateView,
) -> Element<'static, Message> {
    let selected = state
        .selected
        .get(&candidate.path_native)
        .copied()
        .unwrap_or(false);
    let mut label = format!(
        "{} · {} match(es) · {} B output",
        candidate.path, candidate.match_count, candidate.output_bytes
    );
    if candidate.no_change {
        label.push_str(" · NoChange");
    }
    let native = candidate.path_native.clone();
    let checkbox = checkbox(selected)
        .label(label)
        .on_toggle(move |_| Message::ReplaceSelect {
            tab,
            selection: SelectAction::Keys(vec![native.clone()]),
        });
    let mut detail = column![
        checkbox,
        text(format!(
            "before {} · after {}",
            &candidate.before_hash[..8],
            &candidate.after_hash[..8]
        ))
        .size(10),
    ]
    .spacing(2);
    detail = detail.push(Space::new().height(4.0));
    container(detail)
        .width(Length::Fill)
        .padding(iced::Padding {
            top: 4.0,
            bottom: 4.0,
            left: 10.0,
            right: 10.0,
        })
        .into()
}
