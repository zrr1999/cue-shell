use cue_core::job::JobStatus;

use crate::component::main_view::Card;
use crate::display::DisplayPreview;
use crate::record_format;

pub(crate) struct CardJob<'a> {
    pub(crate) id: &'a str,
    pub(crate) status: &'a JobStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CardAction {
    Foreground { job_id: String },
    Tail { job_id: String },
    Preview(DisplayPreview),
}

pub(crate) fn inspect_card_action(
    index: usize,
    card: &Card,
    job: Option<CardJob<'_>>,
) -> CardAction {
    if let Some(job) = job {
        if matches!(job.status, JobStatus::Running) {
            return CardAction::Foreground {
                job_id: job.id.to_string(),
            };
        }

        if job.status.is_terminal() {
            return CardAction::Tail {
                job_id: job.id.to_string(),
            };
        }
    }

    CardAction::Preview(DisplayPreview::new(
        format!("card:{index}"),
        card.label
            .clone()
            .map(|label| format!("record {label}"))
            .unwrap_or_else(|| "record".to_string()),
        record_format::format_card_preview(card),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cue_core::Mode;
    use cue_core::job::CancelReason;

    use crate::component::main_view::{Card, CardStatus};

    fn card(input: &str) -> Card {
        let mut card = Card::new(input.to_string(), Mode::Job);
        card.status = CardStatus::Success;
        card.output = "done".into();
        card
    }

    #[test]
    fn inspect_card_action_foregrounds_running_jobs() {
        let card = card("vim notes.md");

        assert_eq!(
            inspect_card_action(
                3,
                &card,
                Some(CardJob {
                    id: "J7",
                    status: &JobStatus::Running,
                }),
            ),
            CardAction::Foreground {
                job_id: "J7".into(),
            },
        );
    }

    #[test]
    fn inspect_card_action_tails_terminal_jobs() {
        let card = card("cargo test");

        for status in [
            JobStatus::Done,
            JobStatus::Failed,
            JobStatus::Killed,
            JobStatus::Cancelled(CancelReason::User),
        ] {
            assert_eq!(
                inspect_card_action(
                    3,
                    &card,
                    Some(CardJob {
                        id: "J9",
                        status: &status,
                    }),
                ),
                CardAction::Tail {
                    job_id: "J9".into(),
                },
            );
        }
    }

    #[test]
    fn inspect_card_action_previews_non_job_or_non_terminal_cards() {
        let mut card = card("cargo check");
        card.label = Some("build".into());

        assert_eq!(
            inspect_card_action(
                4,
                &card,
                Some(CardJob {
                    id: "J4",
                    status: &JobStatus::Pending,
                }),
            ),
            CardAction::Preview(DisplayPreview::new(
                "card:4",
                "record build",
                "mode: JOB\ninput: cargo check\nstatus: success\nlabel: build\n\ndone",
            )),
        );

        assert!(matches!(
            inspect_card_action(2, &card, None),
            CardAction::Preview(_)
        ));
    }
}
