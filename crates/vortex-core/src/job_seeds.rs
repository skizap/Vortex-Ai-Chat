//! Shipped draft routine templates (SA8 hardening).
//!
//! Three starter routines are seeded once at server startup, and ONLY into an
//! empty routines table: an existing library (even a single archived routine)
//! is never touched, and nothing is ever overwritten or activated. Every
//! template lands as a DRAFT at version 1 — the same safety contract as
//! agent-authored routines: a human must activate a template in the Jobs tab
//! (and pin it to a job) before it can run or be scheduled.
//!
//! The templates are deliberately conservative: read-only inspection, honest
//! reporting of missing setup, and no unattended consequential actions.

use crate::db::Db;
use crate::error::Result;
use vortex_types::{Mode, NewRoutine};

/// Seed the shipped draft routine templates. Idempotent: returns the number
/// of routines created (3 on a fresh database, 0 when the routines table is
/// non-empty). Never activates, schedules, or overwrites anything.
pub async fn seed_draft_routines(db: &Db) -> Result<usize> {
    if !db.list_routines().await?.is_empty() {
        return Ok(0);
    }
    let templates = [
        NewRoutine {
            name: "Morning research digest".into(),
            description: "Daily sourced summary of the last 24 hours on your topics.".into(),
            instructions: concat!(
                "Search the web for the most important developments from the last 24 hours ",
                "on the user's topics. The topics arrive via the job's steering note; if no ",
                "steering note with topics is present, say so honestly and stop instead of ",
                "inventing topics. Summarize up to 5 items, each with a short note on why it ",
                "matters. End with a Sources list containing only actual URLs you retrieved ",
                "with web_search/fetch_url — never fabricate or guess a URL. If no search ",
                "provider is configured, report the missing setup (Settings → Search) instead ",
                "of guessing."
            )
            .into(),
            mode: Mode::Research,
            model: None,
        },
        NewRoutine {
            name: "Repository health check".into(),
            description:
                "Read-only inspection of the job workspace; honest about what could not run.".into(),
            instructions: concat!(
                "READ-ONLY inspection of the job workspace. If the workspace is a git ",
                "repository, report git status and the recent log; if it is not, say so. ",
                "Run the project's test/lint commands only when command execution is ",
                "enabled in Settings → Tools AND the project clearly defines them (e.g. ",
                "Cargo.toml, package.json); report failures and drift. Commands that need ",
                "approval will park for a human decision — never try to bypass that. Never ",
                "modify, create, or delete files, and never run commands that write. End ",
                "with an honest summary of what could not run and why."
            )
            .into(),
            mode: Mode::Coding,
            model: None,
        },
        NewRoutine {
            name: "Workspace inbox triage".into(),
            description: "Prioritized summary of notes/TODO/inbox files, with file evidence."
                .into(),
            instructions: concat!(
                "Read the notes/TODO/inbox files in the job workspace (e.g. TODO.md, ",
                "NOTES.md, inbox/ — use list_files and search_files to find them). ",
                "Categorize items by urgency (urgent / soon / later / unclear) and produce ",
                "a prioritized summary with file evidence (file path plus the quoted line) ",
                "for every item. Execute nothing and change nothing: this routine is ",
                "strictly read-only. If no such files exist, say so honestly instead of ",
                "inventing items."
            )
            .into(),
            mode: Mode::Agent,
            model: None,
        },
    ];
    let mut seeded = 0;
    for template in templates {
        db.create_routine(template).await?;
        seeded += 1;
    }
    Ok(seeded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_types::RoutineStatus;

    fn test_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Db::open(dir.path().join("test.db")).unwrap(), dir)
    }

    #[tokio::test]
    async fn fresh_db_seeds_three_drafts() {
        let (db, _dir) = test_db();
        assert_eq!(seed_draft_routines(&db).await.unwrap(), 3);
        let routines = db.list_routines().await.unwrap();
        assert_eq!(routines.len(), 3);
        for (name, mode) in [
            ("Morning research digest", Mode::Research),
            ("Repository health check", Mode::Coding),
            ("Workspace inbox triage", Mode::Agent),
        ] {
            let r = routines
                .iter()
                .find(|r| r.name == name)
                .unwrap_or_else(|| panic!("missing template {name}"));
            assert_eq!(r.status, RoutineStatus::Draft);
            assert_eq!(r.current_version, 1);
            assert!(!r.description.is_empty());
            let v1 = db
                .get_routine_version(r.id.clone(), 1)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(v1.mode, mode);
            assert_eq!(v1.model, None);
            assert!(!v1.instructions.trim().is_empty());
        }
        // Templates never create jobs or schedules.
        assert!(db.list_jobs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn second_call_seeds_nothing() {
        let (db, _dir) = test_db();
        assert_eq!(seed_draft_routines(&db).await.unwrap(), 3);
        let before = db.list_routines().await.unwrap();
        assert_eq!(seed_draft_routines(&db).await.unwrap(), 0);
        let after = db.list_routines().await.unwrap();
        assert_eq!(after.len(), 3);
        let mut ids_before: Vec<String> = before.iter().map(|r| r.id.clone()).collect();
        let mut ids_after: Vec<String> = after.iter().map(|r| r.id.clone()).collect();
        ids_before.sort();
        ids_after.sort();
        assert_eq!(ids_before, ids_after, "seeding must never duplicate");
    }

    #[tokio::test]
    async fn non_empty_db_is_untouched() {
        let (db, _dir) = test_db();
        let existing = db
            .create_routine(NewRoutine {
                name: "user routine".into(),
                description: "mine".into(),
                instructions: "my instructions".into(),
                mode: Mode::Chat,
                model: Some("openai/gpt-4o".into()),
            })
            .await
            .unwrap();
        db.activate_routine(existing.id.clone()).await.unwrap();
        assert_eq!(seed_draft_routines(&db).await.unwrap(), 0);
        let routines = db.list_routines().await.unwrap();
        assert_eq!(
            routines.len(),
            1,
            "no templates added to a non-empty library"
        );
        let got = db.get_routine(existing.id.clone()).await.unwrap().unwrap();
        assert_eq!(
            got.status,
            RoutineStatus::Active,
            "status never overwritten"
        );
        assert_eq!(got.name, "user routine");
        let v1 = db
            .get_routine_version(existing.id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v1.instructions, "my instructions");
        assert_eq!(v1.mode, Mode::Chat);
        assert_eq!(v1.model.as_deref(), Some("openai/gpt-4o"));
    }

    #[tokio::test]
    async fn all_seeded_statuses_are_draft() {
        let (db, _dir) = test_db();
        seed_draft_routines(&db).await.unwrap();
        let routines = db.list_routines().await.unwrap();
        assert!(routines.iter().all(|r| r.status == RoutineStatus::Draft));
        // Even after a human activates one template, re-seeding stays a no-op.
        db.activate_routine(routines[0].id.clone()).await.unwrap();
        assert_eq!(seed_draft_routines(&db).await.unwrap(), 0);
        let after = db.list_routines().await.unwrap();
        assert_eq!(after.len(), 3);
        assert_eq!(
            after
                .iter()
                .filter(|r| r.status == RoutineStatus::Draft)
                .count(),
            2,
            "seeding never re-activates or resets human decisions"
        );
    }
}
