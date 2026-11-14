//! The vault source Find / replacement preview view (RFC-0006 S07).
//!
//! Rendering only: host state lives in [`crate::host::replace`]. The view
//! makes the source/full-text distinction explicit, shows the frozen
//! scope, truncated scope, typed exclusions and exact totals, and exposes
//! sealed-preview candidates with readable changes and note selection.

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

pub fn input_id(tab: TabId, suffix: &str) -> iced::widget::Id {
    iced::widget::Id::from(format!("{REPLACE_INPUT_ID_PREFIX}{}{suffix}", tab.raw()))
}

pub(crate) fn observed_control<'a>(
    app: &App,
    tab: TabId,
    name: impl Into<String>,
    input: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let name = name.into();
    let generation = app.replace.view_state(tab).frame_generation();
    crate::workspace::bounds::observe_named(
        input,
        format!(
            "vault-search-{tab}-{name}-{}-{:?}",
            app.replace.view_state(tab).frame_generation(),
            app.window_size()
        ),
        move |rect| {
            Message::VaultSearch(crate::host::vault_search::VaultSearchMessage::Bounds(
                tab,
                name.clone(),
                generation,
                rect,
            ))
        },
    )
}

/// Render the Replace view for one tab.
#[allow(clippy::too_many_lines)]
pub(crate) fn content(app: &App, tab: TabId) -> Element<'_, Message> {
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

    let mut controls = column![observed_control(
        app,
        tab,
        "query",
        text_input("Find in Markdown source…", &query)
            .id(input_id(tab, "-query"))
            .on_input(move |value| Message::ReplaceEdit {
                tab,
                field: ReplaceField::Query,
                value
            })
            .on_submit(if state.mode == ReplacePanelMode::Preview {
                Message::ReplaceRunPreview { tab }
            } else {
                Message::ReplaceSubmitNow { tab }
            })
            .padding(tab_strip::chip_padding())
            .size(metrics.text_sm)
            .width(Length::Fill)
    )]
    .spacing(metrics.spacing_sm);
    if state.mode == ReplacePanelMode::Preview {
        controls = controls.push(observed_control(
            app,
            tab,
            "replacement",
            text_input("Replace with… ($1 in regex)", &replacement)
                .id(input_id(tab, "-replacement"))
                .on_input(move |value| Message::ReplaceEdit {
                    tab,
                    field: ReplaceField::Replacement,
                    value,
                })
                .on_submit(Message::ReplaceRunPreview { tab })
                .padding(tab_strip::chip_padding())
                .size(metrics.text_sm)
                .width(Length::Fill),
        ));
    }
    controls = controls.push(
        row![
            observed_control(
                app,
                tab,
                "regex",
                toggle(
                    "Regex",
                    state.regex,
                    Message::ReplaceToggle {
                        tab,
                        flag: ReplaceFlag::Regex
                    }
                )
            ),
            observed_control(
                app,
                tab,
                "case",
                toggle(
                    "Aa",
                    state.case_sensitive,
                    Message::ReplaceToggle {
                        tab,
                        flag: ReplaceFlag::CaseSensitive
                    }
                )
            ),
            observed_control(
                app,
                tab,
                "word",
                toggle(
                    "Word",
                    state.whole_word,
                    Message::ReplaceToggle {
                        tab,
                        flag: ReplaceFlag::WholeWord
                    }
                )
            ),
        ]
        .spacing(metrics.spacing_sm)
        .wrap(),
    );
    controls = controls.push(observed_control(
        app,
        tab,
        "folder",
        text_input("Folder (empty = whole vault)", &state.folder)
            .id(input_id(tab, "-folder"))
            .on_input(move |value| Message::ReplaceEdit {
                tab,
                field: ReplaceField::Folder,
                value,
            })
            .padding(tab_strip::chip_padding())
            .size(metrics.text_sm)
            .width(Length::Fill),
    ));
    controls = controls.push(observed_control(
        app,
        tab,
        "drafts",
        checkbox(state.include_drafts)
            .label("Include unsaved notes")
            .on_toggle(move |_| Message::ReplaceToggle {
                tab,
                flag: ReplaceFlag::IncludeDrafts,
            }),
    ));
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
        parts.push(
            if state.include_drafts {
                "saved notes and unsaved drafts"
            } else {
                "saved notes; unsaved notes excluded"
            }
            .to_owned(),
        );
        parts.join(" · ")
    } else if let Some(plan) = &state.plan {
        format!(
            "Review {} matches in {} notes · {} excluded ({} unsaved)",
            plan.matches, plan.matched_notes, plan.excluded, plan.drafts,
        )
    } else {
        String::from(
            "Find and replace cover Markdown source, including code blocks, frontmatter and link targets. Unsaved notes are included only when enabled.",
        )
    };

    let mut body = column![].spacing(2);
    if state.mode == ReplacePanelMode::Find {
        for result in &state.rows {
            body = body.push(find_row(app, tab, state, result));
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
            body = body.push(candidate_row(app, tab, state, candidate));
        }
        if state.candidates.is_empty() && state.error.is_none() && !state.updating {
            body = body.push(
                text("Run a preview to seal exact before/after payloads.").style(style::dim_text),
            );
        }
    }

    if state.earlier_results_removed {
        body = body.push(
            text("Showing the most recent 2,000 matches. Refresh to return to the beginning.")
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    let mut actions = row![
        observed_control(
            app,
            tab,
            "mode-find",
            mode_button("Find", ReplacePanelMode::Find)
        ),
        observed_control(
            app,
            tab,
            "mode-replace",
            mode_button("Replace", ReplacePanelMode::Preview)
        ),
        Space::new().width(8),
    ]
    .spacing(metrics.spacing_sm);
    match state.mode {
        ReplacePanelMode::Find => {
            if state.has_next() {
                actions = actions.push(observed_control(
                    app,
                    tab,
                    "next",
                    button(text("Next page"))
                        .padding(tab_strip::chip_padding())
                        .style(style::ghost())
                        .on_press(Message::ReplaceNextPage { tab }),
                ));
            }
            actions = actions.push(observed_control(
                app,
                tab,
                "find",
                button(text("Find in vault"))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::ReplaceSubmitNow { tab }),
            ));
        }
        ReplacePanelMode::Preview => {
            actions = actions.push(observed_control(
                app,
                tab,
                "run-preview",
                button(text("Run preview"))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::ReplaceRunPreview { tab }),
            ));
            if state.preview_id.is_some() {
                actions = actions.push(observed_control(
                    app,
                    tab,
                    "select-all",
                    button(text("Select all"))
                        .padding(tab_strip::chip_padding())
                        .style(style::ghost())
                        .on_press(Message::ReplaceSelect {
                            target: state.preview_target().expect("sealed preview control"),
                            tab,
                            selection: SelectAction::All,
                        }),
                ));
                actions = actions.push(observed_control(
                    app,
                    tab,
                    "select-none",
                    button(text("Select none"))
                        .padding(tab_strip::chip_padding())
                        .style(style::ghost())
                        .on_press(Message::ReplaceSelect {
                            target: state.preview_target().expect("sealed preview control"),
                            tab,
                            selection: SelectAction::None,
                        }),
                ));
            }
        }
    }
    if state.mode == ReplacePanelMode::Preview
        && state.selection_ready
        && let Some(plan) = &state.plan
        && plan.selection_revision.is_some()
    {
        actions = actions.push(
            text(format!("{} notes selected", state.selected_count()))
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
        if state.can_apply() {
            actions = actions.push(observed_control(
                app,
                tab,
                "apply",
                button(text("Apply"))
                    .padding(tab_strip::chip_padding())
                    .style(style::list_row(true))
                    .on_press(Message::ReplaceApply {
                        tab,
                        target: state.preview_target().expect("selected preview control"),
                    }),
            ));
        }
    }
    if let Some(job) = &state.job {
        let mut job_line = format!(
            "Replacement · {} · saved {} · conflicts {} · stale {}",
            job.phase, job.saved, job.conflicts, job.stale
        );
        if job.applied_drafts > 0 {
            job_line.push_str(&format!(" · drafts applied {}", job.applied_drafts));
        }
        if job.reverted > 0 || job.reverted_drafts > 0 {
            job_line.push_str(&format!(
                " · reverted {} ({} drafts)",
                job.reverted + job.reverted_drafts,
                job.reverted_drafts
            ));
        }
        if job.index_pending > 0 {
            job_line.push_str(&format!(" · index pending {}", job.index_pending));
        }
        if job.not_started > 0 {
            job_line.push_str(&format!(" · not started {}", job.not_started));
        }
        actions = actions.push(text(job_line).size(metrics.text_sm).style(style::dim_text));
        if state.can_cancel() {
            actions = actions.push(observed_control(
                app,
                tab,
                "cancel",
                button(text("Cancel"))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::ReplaceCancel {
                        tab,
                        job_id: state
                            .job
                            .as_ref()
                            .expect("visible job control")
                            .job_id
                            .clone(),
                    }),
            ));
        }
        if state.can_resume() {
            actions = actions.push(observed_control(
                app,
                tab,
                "resume",
                button(text("Resume"))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::ReplaceResume {
                        tab,
                        job_id: state
                            .job
                            .as_ref()
                            .expect("visible job control")
                            .job_id
                            .clone(),
                    }),
            ));
        }
        if state.can_revert() {
            actions = actions.push(observed_control(
                app,
                tab,
                "revert",
                button(text("Revert"))
                    .padding(tab_strip::chip_padding())
                    .style(style::list_row(true))
                    .on_press(Message::ReplaceRevert {
                        tab,
                        job_id: state
                            .job
                            .as_ref()
                            .expect("visible job control")
                            .job_id
                            .clone(),
                    }),
            ));
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
            actions.wrap(),
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
    app: &App,
    tab: TabId,
    state: &crate::host::replace::ReplaceTabState,
    result: &FindRowView,
) -> Element<'static, Message> {
    let metrics = app.appearance.metrics();
    let closed = state.collapsed.contains(&result.path);
    let mut body = column![observed_control(
        app,
        tab,
        format!("group:{}", result.ordinal),
        button(text(format!(
            "{} {} · {} matches{}",
            if closed { "▸" } else { "▾" },
            result.path,
            result.note_matches,
            if result.source_kind == "draft" {
                " · unsaved"
            } else {
                ""
            }
        )))
        .width(Length::Fill)
        .style(style::ghost())
        .on_press(Message::ReplaceCollapse {
            tab,
            path: result.path.clone()
        })
    )]
    .spacing(2);
    if !closed {
        for hit in &result.contexts {
            let accent = style::accent_text(&app.appearance.theme())
                .color
                .unwrap_or(iced::Color::WHITE);
            let label = row![
                text(format!("{}:{}", hit.line, hit.column))
                    .size(metrics.text_sm)
                    .style(style::dim_text),
                iced::widget::rich_text::<(), Message, iced::Theme, iced::Renderer>([
                    iced::widget::span(hit.before.clone()),
                    iced::widget::span(if hit.matched.is_empty() {
                        "│".into()
                    } else {
                        hit.matched.clone()
                    })
                    .color(accent),
                    iced::widget::span(hit.after.clone())
                ])
                .size(metrics.text_sm)
                .font(iced::Font::MONOSPACE),
            ]
            .spacing(metrics.spacing_sm);
            body = body.push(observed_control(
                app,
                tab,
                format!("match:{}:{}", result.ordinal, hit.start),
                button(label)
                    .width(Length::Fill)
                    .style(style::ghost())
                    .on_press(Message::VaultSearch(
                        crate::host::vault_search::VaultSearchMessage::OpenMatch {
                            owner: tab,
                            finder: state.finder.clone().unwrap_or_default(),
                            path: result.path.clone(),
                            hash: result.content_hash.clone(),
                            start: hit.start,
                            end: hit.end,
                        },
                    )),
            ));
        }
    }
    container(body).width(Length::Fill).padding(4).into()
}

fn candidate_row(
    app: &App,
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
    if candidate.source_kind == "draft" {
        label.push_str(" · draft");
    }
    if candidate.no_change {
        label.push_str(" · NoChange");
    }
    let native = candidate.path_native.clone();
    let target = state
        .preview_target()
        .expect("candidate belongs to a sealed preview");
    let selected_keys: Vec<_> = state
        .selected
        .iter()
        .filter(|(_, selected)| **selected)
        .map(|(path, _)| path.clone())
        .collect();
    let checkbox = checkbox(selected).label(label).on_toggle(move |on| {
        let mut keys = selected_keys.clone();
        keys.retain(|key| key != &native);
        if on {
            keys.push(native.clone());
        }
        Message::ReplaceSelect {
            target: target.clone(),
            tab,
            selection: SelectAction::Keys(keys),
        }
    });
    let mut detail = column![
        observed_control(
            app,
            tab,
            format!("candidate:{}", candidate.item_id),
            checkbox
        ),
        observed_control(
            app,
            tab,
            format!("inspect:{}", candidate.item_id),
            button(text(if state.inspected.contains(&candidate.item_id) {
                "Hide changes"
            } else {
                "Inspect changes"
            }))
            .style(style::ghost())
            .on_press(Message::ReplaceInspect {
                tab,
                item_id: candidate.item_id
            })
        )
    ]
    .spacing(4);
    if state.inspected.contains(&candidate.item_id) {
        match state.diffs.get(&candidate.item_id) {
            Some(Ok(diff)) => {
                detail = detail.push(
                    text(format!(
                        "From line {}{}",
                        diff.first_line,
                        if diff.truncated {
                            " · excerpt; additional text omitted"
                        } else {
                            ""
                        }
                    ))
                    .size(11),
                );
                detail = detail.push(
                    text(format!("− {}", diff.before.replace('\n', "\n− ")))
                        .font(iced::Font::MONOSPACE)
                        .size(12),
                );
                detail = detail.push(
                    text(format!("+ {}", diff.after.replace('\n', "\n+ ")))
                        .font(iced::Font::MONOSPACE)
                        .size(12)
                        .style(style::accent_text),
                );
            }
            Some(Err(error)) => detail = detail.push(text(error.clone()).size(12)),
            None => detail = detail.push(text("Loading sealed changes…").size(12)),
        }
    }
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
