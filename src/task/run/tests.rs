use tokio::sync::watch;

use crate::{
    agent::AgentKind,
    config::InstallationIdentity,
    db::Database,
    isolation::{CommandEvent, IsolationEngine, IsolationStatus, TestbedIsolation},
    task::{TaskDraft, TaskExecutionStatus, TaskService, TaskStore},
    testkit,
};

// The whole turn, in process: a task is founded in the real store, provisioned into
// a real sandbox by the testbed provider, and run against a scripted harness whose
// stream the real decoder reads. Nothing here is a mock of a subsystem — only the
// harness, the version-control client and the machine are stood in for.
#[tokio::test]
async fn a_task_runs_a_scripted_turn() {
    let root = tempfile::tempdir().expect("a temporary root");
    let config = testkit::config(root.path());
    let database = Database::open(&config).await.expect("a database");
    let store = TaskStore::new(&database);

    let isolation = TestbedIsolation::new(&config);
    let (status, _rx) = watch::channel(IsolationStatus::Starting);
    if let Err(error) = isolation.prepare(&status).await {
        // The testbed sandbox is a chroot over this machine and needs mount
        // namespaces and a writable `/root`, so it runs where Altitude itself
        // can — a Linux sandbox with its account as root. Elsewhere there is
        // nothing to exercise, and a skip is not a failure.
        eprintln!(
            "skipping the scripted turn: the testbed isolation is unavailable here ({error})"
        );
        return;
    }

    let identity = InstallationIdentity {
        id: "test-install".to_owned(),
        label: "test".to_owned(),
    };
    let mut service = TaskService::new(
        store.clone(),
        isolation,
        config.clone(),
        testkit::engines(),
        identity,
    );
    service.override_recipe("vcs/github", testkit::scripted_vcs_recipe());

    let task = store
        .create(
            TaskDraft {
                intent: "run the scripted turn".to_owned(),
                project: testkit::project("scripted"),
                identity_id: "test-install".to_owned(),
                identity_label: "test".to_owned(),
                host: "test-host".to_owned(),
                agent_kind: Some(AgentKind::Pi),
                agent_conversation_id: Some("scripted-conversation".to_owned()),
            },
            None,
            &[],
        )
        .await
        .expect("a founded task");

    let (task, _run, mut session) = service.start(task).await.expect("a started turn");
    assert_eq!(task.facts.execution_status, TaskExecutionStatus::Running);

    let mut output = String::new();
    while let Some(event) = session.events.recv().await {
        match event {
            CommandEvent::Output { bytes, .. } => {
                output.push_str(&String::from_utf8_lossy(&bytes));
            }
            CommandEvent::Exited(code) => {
                assert_eq!(code, 0, "the scripted turn exited badly:\n{output}");
                break;
            }
            CommandEvent::Failed(error) => panic!("the scripted turn failed: {error}"),
        }
    }
    assert!(output.contains("scripted turn complete"), "{output}");

    // The same bytes the turn loop would feed the decoder say what the harness did.
    let mut decoder = service.stream_decoder(&task).expect("a decoder");
    decoder.process(output.as_bytes());
    assert_eq!(decoder.take_model().as_deref(), Some("scripted-1"));
    let usage = decoder.take_usage().expect("usage from the stream");
    assert_eq!(usage.cost_usd_micros, 1200);

    // The transcript the harness wrote is on the machine side of the profile mount.
    let transcript = config
        .profile_dir("agents/pi")
        .join("sessions/scripted-conversation.jsonl");
    assert!(
        transcript.is_file(),
        "no transcript at {}",
        transcript.display()
    );
}
