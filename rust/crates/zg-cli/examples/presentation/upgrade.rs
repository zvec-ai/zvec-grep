//! Upgrade outcome fixtures; the production renderers perform no filesystem work.
//!
//! Counts, paths, and error causes below are explicit samples. No global user
//! configuration is inspected, migrated, backed up, or changed by these cards.

use std::{io, path::Path};

use zg_cli::{
    ColorMode, IntegrationUpgradeEntryKind, IntegrationUpgradeSummary,
    write_integration_upgrade_entry, write_integration_upgrade_summary,
};

use super::{Fixture, render};

struct Case {
    id: &'static str,
    title: &'static str,
    kind: Option<IntegrationUpgradeEntryKind>,
    summary: IntegrationUpgradeSummary,
    preview: bool,
    terminal: bool,
}

fn cases() -> [Case; 7] {
    [
        Case {
            id: "upgrade-upgraded",
            title: "Integration upgraded · fixture",
            kind: Some(IntegrationUpgradeEntryKind::Upgraded),
            summary: IntegrationUpgradeSummary {
                updated: 1,
                ..Default::default()
            },
            preview: false,
            terminal: true,
        },
        Case {
            id: "upgrade-preview",
            title: "Upgrade preview · fixture",
            kind: Some(IntegrationUpgradeEntryKind::Preview),
            summary: IntegrationUpgradeSummary {
                updated: 1,
                ..Default::default()
            },
            preview: true,
            terminal: true,
        },
        Case {
            id: "upgrade-compatible",
            title: "Existing integration compatible · fixture",
            kind: Some(IntegrationUpgradeEntryKind::Compatible),
            summary: IntegrationUpgradeSummary {
                compatible: 1,
                ..Default::default()
            },
            preview: false,
            terminal: true,
        },
        Case {
            id: "upgrade-manual",
            title: "Manual migration required · sample fixture",
            kind: Some(IntegrationUpgradeEntryKind::Manual),
            summary: IntegrationUpgradeSummary {
                manual: 1,
                ..Default::default()
            },
            preview: false,
            terminal: true,
        },
        Case {
            id: "upgrade-failed",
            title: "Upgrade failed · sample fixture",
            kind: Some(IntegrationUpgradeEntryKind::Failed),
            summary: IntegrationUpgradeSummary {
                failed: 1,
                ..Default::default()
            },
            preview: false,
            terminal: true,
        },
        Case {
            id: "upgrade-empty",
            title: "No integrations to upgrade · fixture",
            kind: None,
            summary: IntegrationUpgradeSummary::default(),
            preview: false,
            terminal: true,
        },
        Case {
            id: "upgrade-redirected",
            title: "Upgrade output redirected · fixture",
            kind: Some(IntegrationUpgradeEntryKind::Upgraded),
            summary: IntegrationUpgradeSummary {
                updated: 1,
                ..Default::default()
            },
            preview: false,
            terminal: false,
        },
    ]
}

pub(super) fn fixtures(root: &Path) -> io::Result<Vec<Fixture>> {
    cases().into_iter().map(|case| {
        let command = if case.preview {
            "zg --install --upgrade-installed --dry-run"
        } else if !case.terminal {
            "zg --install --upgrade-installed > upgrade.log"
        } else {
            "zg --install --upgrade-installed"
        };
        render(case.id, case.title, command, |output| {
            let color = if case.terminal { ColorMode::Always } else { ColorMode::Never };
            if let Some(kind) = case.kind {
                let path = root.join(".config/opencode/opencode.json");
                let detail = match kind {
                    IntegrationUpgradeEntryKind::Failed => format!(
                        "{}: Permission denied (sample)", path.display()
                    ),
                    IntegrationUpgradeEntryKind::Manual => format!(
                        "{} [zvec_grep] has an unsupported launcher; use zg --server --stdio for stdio MCP",
                        path.display()
                    ),
                    IntegrationUpgradeEntryKind::Upgraded | IntegrationUpgradeEntryKind::Preview => {
                        format!("{} (1 entries)", path.display())
                    }
                    _ => format!("{} [zvec_grep]", path.display()),
                };
                if matches!(kind, IntegrationUpgradeEntryKind::Upgraded) {
                    write_integration_upgrade_entry(
                        &mut *output, IntegrationUpgradeEntryKind::Backup,
                        &format!("{}.zg-upgrade-fixture.bak", path.display()), color, case.terminal,
                    )?;
                }
                write_integration_upgrade_entry(&mut *output, kind, &detail, color, case.terminal)?;
            }
            write_integration_upgrade_summary(
                output, &case.summary, case.preview, color, case.terminal,
            )
        })
    }).collect()
}
