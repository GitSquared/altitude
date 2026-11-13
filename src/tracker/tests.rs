use super::{
    IssueDependency, IssueReference, IssueScope, IssueSnapshot, IssueSummary, TrackerKind,
};

fn snapshot(blocked_by: Vec<IssueDependency>) -> IssueSnapshot {
    IssueSnapshot {
        issue: IssueSummary {
            reference: IssueReference {
                provider: TrackerKind::GitHub,
                workspace_id: "Satwa".to_owned(),
                id: "Satwa/supercal-v2#212".to_owned(),
                identifier: "Satwa/supercal-v2#212".to_owned(),
                url: "https://github.com/Satwa/supercal-v2/issues/212".to_owned(),
            },
            title: "skip the web supabase stack for unrelated ci changes".to_owned(),
            project: None,
            scope: Some(IssueScope {
                id: "Satwa/supercal-v2".to_owned(),
                aliases: Vec::new(),
                label: "GitHub repository Satwa/supercal-v2".to_owned(),
            }),
        },
        description: "### Goal\n\nrun the web stack only when it can change.".to_owned(),
        comments: Vec::new(),
        blocked_by,
    }
}

fn dependency(identifier: &str, open: bool) -> IssueDependency {
    IssueDependency {
        identifier: identifier.to_owned(),
        title: "restore the github actions minutes".to_owned(),
        open,
    }
}

#[test]
fn an_open_blocker_is_named_and_held_against_starting() {
    let intent = snapshot(vec![
        dependency("Satwa/supercal-v2#210", true),
        dependency("Satwa/supercal-v2#82", false),
    ])
    .intent(None);

    assert!(intent.contains("Blocked by:\n"));
    assert!(intent.contains("- Satwa/supercal-v2#210 (open): restore"));
    assert!(intent.contains("- Satwa/supercal-v2#82 (closed): restore"));
    assert!(intent.contains("not ready to start"));
}

#[test]
fn a_closed_blocker_no_longer_holds_anything() {
    let intent = snapshot(vec![dependency("Satwa/supercal-v2#82", false)]).intent(None);

    assert!(intent.contains("- Satwa/supercal-v2#82 (closed):"));
    assert!(!intent.contains("not ready to start"));
}

#[test]
fn no_blockers_says_nothing_about_blockers() {
    let intent = snapshot(Vec::new()).intent(None);

    assert!(!intent.contains("Blocked by"));
    assert!(!intent.contains("not ready to start"));
}
