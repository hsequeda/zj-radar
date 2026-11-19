//! Zellij's native per-pane attention (Zellij ≥ 0.46: the Program Status
//! Protocol, OSC 7501, plus shell-command tracking), mirrored as plain data so
//! the policy stays host-testable — `zellij-tile` is wasm-only. The wasm glue
//! (`lib.rs`) copies `PaneInfo::attention` / `program_status` into these types;
//! [`NativePane::observation`] turns them into the observation the rail rolls
//! up.

use crate::kind::Kind;
use crate::observation::{ObservationOrigin, TrackedObservation};
use crate::status::Status;
use crate::task::{BgTask, BgTasks, TaskState};

/// Which kind of signal decided the pane's attention (`AttentionSource`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Source {
    /// An OSC 7501 record (`ProgramStatus`).
    Program,
    /// A shell command, command pane, or foreground process.
    Command,
    /// Bell, notification, idle rule, watch, unseen output — or none.
    #[default]
    Other,
}

/// `PaneInfo::attention`, reduced to what the rail shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PaneAttention {
    pub status: Status,
    pub source: Source,
    /// Zellij's display text: already carries the kind label and progress
    /// (`needs approval: …`, `working 40%: …`).
    pub text: String,
    pub app: Option<String>,
    pub since_ms: u64,
    /// The program's deciding record, kept after the pane is seen: a focused
    /// pane's Done is seen at once and never shows as the status.
    pub remembered: Option<Remembered>,
}

/// `PaneAttention::remembered`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Remembered {
    pub status: Status,
    pub text: String,
    pub since_ms: u64,
}

/// A completion's ledger label: Zellij's text without its `done: ` /
/// `failed: ` head, which the ledger glyph already says.
pub(crate) fn completion_label(text: &str) -> &str {
    text.strip_prefix("done: ").or_else(|| text.strip_prefix("failed: ")).unwrap_or(text)
}

/// One `ProgramStatusRecord`: the root (`id == None`) or a child.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Record {
    pub id: Option<String>,
    pub status: Status,
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
    /// `progress=` (0–100), shown ahead of the row text while working.
    pub progress: Option<u8>,
    pub updated_ms: u64,
}

impl Record {
    /// Title, else msg, else id.
    fn label(&self) -> String {
        [&self.title, &self.msg, &self.id]
            .into_iter()
            .flatten()
            .find(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_default()
    }

    /// The row text for a record that decides the pane: msg, else title,
    /// else id. A working record leads with its progress (`40% · msg`);
    /// a completion never does, so its text matches the label Zellij's
    /// `remembered` carries and the two ledger edges merge.
    fn row_text(&self) -> String {
        let text = [&self.msg, &self.title, &self.id]
            .into_iter()
            .flatten()
            .find(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_default();
        match self.progress.map(|p| p.min(100)) {
            Some(p) if self.status == Status::Running && text.is_empty() => format!("{p}%"),
            Some(p) if self.status == Status::Running => format!("{p}% · {text}"),
            _ => text,
        }
    }
}

/// Everything the manifest says about one terminal pane's attention.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NativePane {
    pub attention: PaneAttention,
    pub records: Vec<Record>,
}

/// The agent mark for an OSC 7501 `app`. `zellij` is a nested or remote
/// Zellij forwarding its session's aggregate status.
pub(crate) fn kind_for_app(app: &str) -> Kind {
    match app {
        "claude-code" => Kind::Claude,
        "zellij" => Kind::Remote,
        other => Kind::from_source(other),
    }
}

impl NativePane {
    /// The rail observation for this pane, or `None` when no OSC 7501 program
    /// reports on it. Only program status counts: Zellij's shell-command
    /// attention stays with the `CommandStore`, and bells, idle rules and
    /// watches have no program to show. `prev` is the pane's
    /// observation from the previous manifest: an unchanged status keeps its
    /// `last_change_tick`, so the spinner's ease and the rows memo hold.
    pub(crate) fn observation(&self, prev: Option<&TrackedObservation>, tick: u64) -> Option<TrackedObservation> {
        let a = &self.attention;
        if self.records.is_empty() && a.source != Source::Program {
            return None;
        }
        let app = a.app.as_deref().or_else(|| self.records.iter().find_map(|r| r.app.as_deref()));
        let program_kind = app.map_or(Kind::Other, kind_for_app);
        // Zellij raises a pane from its child records only for `blocked` and
        // `error` (`deciding_record`): children that are only `working` with no
        // root record (Opencode) leave the pane Idle. Take the most severe record.
        // With a root, Zellij's status stands (a seen Done reads Idle).
        let has_root = self.records.iter().any(|r| r.id.is_none());
        let deciding = (a.source != Source::Program && !has_root)
            .then(|| self.records.iter().enumerate().max_by_key(|(_, r)| r.status))
            .flatten();
        let (status, msg, since_ms) = match deciding {
            Some((_, r)) => (r.status, r.row_text(), r.updated_ms),
            None => {
                let root = self.records.iter().find(|r| r.id.is_none()).map(Record::row_text);
                if a.source == Source::Program {
                    let msg = root.filter(|m| !m.is_empty()).unwrap_or_else(|| a.text.clone());
                    (a.status, msg, a.since_ms)
                } else {
                    // Zellij decided on something other than the program: the
                    // root was seen (a seen Done reads Idle), or a shell command
                    // or bell took over. The program has nothing active to say.
                    (Status::Idle, root.unwrap_or_default(), a.since_ms)
                }
            }
        };
        if status == Status::Idle && self.records.is_empty() {
            return None;
        }
        let since_s = since_ms / 1000;
        let skip = deciding.map(|(i, _)| i);
        let items = self
            .records
            .iter()
            .enumerate()
            .filter(|&(i, r)| r.id.is_some() && Some(i) != skip)
            .map(|(_, r)| BgTask {
                id: r.id.clone().unwrap_or_default(),
                label: r.label(),
                holds: true,
                state: match r.status {
                    Status::Done => TaskState::Completed,
                    Status::Error => TaskState::Failed,
                    Status::Idle => TaskState::Ended,
                    Status::Running | Status::Pending => TaskState::Running,
                },
                // ponytail: OSC 7501 has no start time, `updated_ms` stands in; add one if ages mislead
                started_epoch_s: r.updated_ms / 1000,
                ended_epoch_s: r.status.is_completion().then_some(r.updated_ms / 1000),
            })
            .collect();
        Some(
            TrackedObservation {
                origin: ObservationOrigin::StatusPipe,
                status,
                repo: String::new(),
                branch: String::new(),
                msg,
                task: String::new(),
                kind: program_kind,
                last_change_tick: prev.filter(|p| p.status == status).map_or(tick, |p| p.last_change_tick),
                ever_active: true,
                exit_code: None,
                completed_epoch_s: status.is_completion().then_some(since_s),
                pending_epoch_s: (status == Status::Pending).then_some(since_s),
                // The `✓` gesture still speaks the status pipe (`Effect::BroadcastStatus`),
                // which a native pane ignores: keep the hotspot off until it marks the pane seen.
                acknowledged: true,
                tasks: BgTasks { items, waiting: false },
            }
            .sanitized(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(status: Status, app: &str, text: &str) -> NativePane {
        NativePane {
            attention: PaneAttention {
                status,
                source: Source::Program,
                text: text.into(),
                app: Some(app.into()),
                since_ms: 5_000,
                ..PaneAttention::default()
            },
            records: vec![Record { status, app: Some(app.into()), ..Record::default() }],
        }
    }

    fn child(id: &str, status: Status, title: &str) -> Record {
        Record { id: Some(id.into()), status, title: Some(title.into()), updated_ms: 9_000, ..Record::default() }
    }

    #[test]
    fn idle_pane_without_records_is_untracked() {
        assert_eq!(NativePane::default().observation(None, 3), None);
    }

    #[test]
    fn program_status_maps_app_and_text() {
        let o = program(Status::Running, "claude-code", "working 40%: Editing main.rs").observation(None, 7).unwrap();
        assert_eq!(o.status, Status::Running);
        assert_eq!(o.kind, Kind::Claude);
        assert_eq!(o.origin, ObservationOrigin::StatusPipe);
        assert_eq!(o.msg, "working 40%: Editing main.rs");
        assert_eq!(o.last_change_tick, 7);
        assert_eq!(o.exit_code, None);
    }

    #[test]
    fn pending_stamps_the_wait_clock() {
        let o = program(Status::Pending, "codex", "needs approval: approve Bash").observation(None, 1).unwrap();
        assert_eq!(o.kind, Kind::Codex);
        assert_eq!(o.pending_epoch_s, Some(5));
        assert_eq!(o.completed_epoch_s, None);
    }

    #[test]
    fn unchanged_status_keeps_its_tick() {
        let pane = program(Status::Running, "pi", "x");
        let first = pane.observation(None, 2).unwrap();
        assert_eq!(pane.observation(Some(&first), 9).unwrap().last_change_tick, 2);
        let done = program(Status::Done, "pi", "x").observation(Some(&first), 9).unwrap();
        assert_eq!(done.last_change_tick, 9);
        assert_eq!(done.completed_epoch_s, Some(5));
    }

    #[test]
    fn idle_program_keeps_its_row_and_root_label() {
        let mut pane = program(Status::Idle, "claude-code", "");
        pane.records[0].msg = Some("ready".into());
        let o = pane.observation(None, 1).unwrap();
        assert_eq!(o.status, Status::Idle);
        assert_eq!(o.msg, "ready");
    }

    #[test]
    fn working_children_without_root_raise_the_pane() {
        // Opencode: one child record per session, no root; Zellij leaves the pane Idle.
        let pane = NativePane {
            attention: PaneAttention::default(),
            records: vec![
                Record { app: Some("opencode".into()), ..child("s1", Status::Done, "old") },
                child("s2", Status::Running, "Fix the build"),
            ],
        };
        let o = pane.observation(None, 4).unwrap();
        assert_eq!(o.status, Status::Running);
        assert_eq!(o.kind, Kind::Opencode);
        assert_eq!(o.msg, "Fix the build");
        // The deciding record is the pane line; only the other one is a sub-line.
        assert_eq!(o.tasks.items.len(), 1);
        assert_eq!(o.tasks.items[0].state, TaskState::Completed);
    }

    #[test]
    fn seen_done_with_a_root_stays_idle() {
        // A focused pane's Done is seen after ~2 s: Zellij drops the source and
        // reports Idle, while the root and child records stay.
        let mut pane = program(Status::Idle, "claude-code", "");
        pane.attention.source = Source::Other;
        pane.records[0].status = Status::Done;
        pane.records[0].msg = Some("Turn finished".into());
        pane.records.push(child("t1", Status::Done, ""));
        let o = pane.observation(None, 1).unwrap();
        assert_eq!((o.status, o.kind, o.msg.as_str()), (Status::Idle, Kind::Claude, "Turn finished"));
    }

    #[test]
    fn program_source_ignores_child_severity() {
        let mut pane = program(Status::Running, "claude-code", "turn");
        pane.records.push(child("a4cc", Status::Running, "Explore docs"));
        let o = pane.observation(None, 1).unwrap();
        assert_eq!(o.status, Status::Running);
        assert_eq!(o.msg, "turn");
        let task = &o.tasks.items[0];
        assert_eq!((task.label.as_str(), task.state, task.holds), ("Explore docs", TaskState::Running, true));
    }

    #[test]
    fn non_program_attention_is_left_to_the_other_sources() {
        // Shell commands belong to the CommandStore; bells, idle rules and
        // watches have no program behind them.
        let cases = [
            ("shell command, no records", Source::Command, Status::Error, false, None),
            ("bell, no records", Source::Other, Status::Pending, false, None),
            ("shell command over an idle root", Source::Command, Status::Error, true, Some(Status::Idle)),
            ("bell over an idle root", Source::Other, Status::Pending, true, Some(Status::Idle)),
        ];
        for (name, source, status, with_root, want) in cases {
            let pane = NativePane {
                attention: PaneAttention { status, source, text: "cargo test".into(), ..PaneAttention::default() },
                records: if with_root { vec![Record { app: Some("pi".into()), ..Record::default() }] } else { vec![] },
            };
            let o = pane.observation(None, 1);
            assert_eq!(o.as_ref().map(|o| o.status), want, "{name}");
            if let Some(o) = o {
                assert_eq!((o.origin, o.kind, o.exit_code), (ObservationOrigin::StatusPipe, Kind::Pi, None), "{name}");
            }
        }
    }

    #[test]
    fn row_text_is_msg_with_progress_while_working() {
        let record = |status, msg: Option<&str>, title: Option<&str>, progress| Record {
            status,
            msg: msg.map(Into::into),
            title: title.map(Into::into),
            progress,
            ..Record::default()
        };
        let cases = [
            ("msg wins over title", record(Status::Running, Some("Editing"), Some("Turn"), None), "Editing"),
            ("title when no msg", record(Status::Pending, None, Some("Approve Bash"), None), "Approve Bash"),
            ("progress leads while working", record(Status::Running, Some("Editing"), None, Some(40)), "40% · Editing"),
            ("progress alone", record(Status::Running, None, None, Some(7)), "7%"),
            ("progress capped at 100", record(Status::Running, Some("x"), None, Some(250)), "100% · x"),
            ("no progress on a completion", record(Status::Done, Some("Shipped"), None, Some(100)), "Shipped"),
        ];
        for (name, r, want) in cases {
            assert_eq!(r.row_text(), want, "{name}");
        }
    }

    #[test]
    fn program_row_uses_the_root_msg_over_zellij_text() {
        let mut pane = program(Status::Running, "claude-code", "working 40%: Editing main.rs");
        pane.records[0].msg = Some("Editing main.rs".into());
        pane.records[0].progress = Some(40);
        let o = pane.observation(None, 1).unwrap();
        assert_eq!(o.msg, "40% · Editing main.rs");
    }

    #[test]
    fn program_text_is_sanitized() {
        let o = program(Status::Running, "x", "a\x1b[31mb\nc").observation(None, 1).unwrap();
        assert!(!o.msg.contains('\x1b') && !o.msg.contains('\n'), "{:?}", o.msg);
        assert_eq!(o.kind, Kind::Other);
    }

    #[test]
    fn completion_labels_drop_the_status_head() {
        assert_eq!(completion_label("done: Refactor complete"), "Refactor complete");
        assert_eq!(completion_label("failed: exit 2"), "exit 2");
        assert_eq!(completion_label("cargo test"), "cargo test");
    }

    #[test]
    fn app_marks() {
        assert_eq!(kind_for_app("claude-code"), Kind::Claude);
        assert_eq!(kind_for_app("zellij"), Kind::Remote);
        assert_eq!(kind_for_app("omp"), Kind::Omp);
        assert_eq!(kind_for_app("vim"), Kind::Other);
    }
}
