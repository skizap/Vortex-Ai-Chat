//! Captured completions verify the cache independently of a live provider.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;
use vortex_agents::{Engine, Hub, PreviewRegistry, Scheduler};
use vortex_core::{AttachmentStore, Db};
use vortex_llm::llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent, PdfEngine};
use vortex_llm::LlmError;
use vortex_types::{
    AttachmentKind, AttachmentOrigin, AttachmentRef, ChatMessage, ContentPart, Mode, ModelInfo,
    Role, RunStatus, Settings, StoredMessage,
};

struct Capture(mpsc::Sender<CompletionRequest>);

#[async_trait]
impl LlmClient for Capture {
    async fn complete(
        &self,
        req: &CompletionRequest,
        _: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, LlmError> {
        let initial = req
            .messages
            .last()
            .is_some_and(|m| m.content == "initial PDFs");
        let annotations = initial.then(|| {
            let mut annotations: Vec<_> = names(req)
                .into_iter()
                .map(|name| annotation(Some(name)))
                .collect();
            annotations.reverse(); // Identifying by response order would swap these.
            annotations.push(annotation(Some("duplicate.pdf")));
            annotations.push(annotation(None));
            annotations
        });
        self.0.send(req.clone()).await.unwrap();
        Ok(CompletionResponse {
            content: "reply".into(),
            tool_calls: vec![],
            usage: None,
            finish_reason: "stop".into(),
            annotations,
        })
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        panic!("PDF-only requests must not fetch the catalog")
    }

    fn describe(&self) -> &'static str {
        "pdf-fixture"
    }
}

fn names(req: &CompletionRequest) -> Vec<&str> {
    req.messages
        .iter()
        .flat_map(|m| m.parts.iter().flatten())
        .filter_map(|part| {
            if let ContentPart::File { file } = part {
                Some(file.filename.as_str())
            } else {
                None
            }
        })
        .collect()
}

fn annotation(name: Option<&str>) -> Value {
    let mut annotation = json!({"type":"file","file":{"hash":"opaque","content":[{"type":"text","text":name.unwrap_or("unknown")}]}});
    if let Some(name) = name {
        annotation["file"]["name"] = json!(name);
    }
    annotation
}

async fn run(
    engine: &Arc<Engine>,
    rx: &mut mpsc::Receiver<CompletionRequest>,
    conv: &str,
    history: Vec<StoredMessage>,
    text: &str,
    refs: Vec<AttachmentRef>,
) -> CompletionRequest {
    let preflight = engine.preflight_chat(conv, &refs).await.unwrap();
    let run = engine
        .start_preflighted_chat_run(conv, Mode::Chat, history, text, refs, preflight)
        .await;
    let req = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if engine
                .db
                .get_run(run.clone())
                .await
                .unwrap()
                .unwrap()
                .status
                == RunStatus::Completed
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    req
}

#[tokio::test]
async fn distinct_pdf_ids_replay_only_on_the_original_assistant_with_the_same_parser() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("db.sqlite")).unwrap();
    let store = Arc::new(AttachmentStore::new(dir.path().join("media"), db.clone()).unwrap());
    let conv = db
        .create_conversation("PDF cache", Mode::Chat, None)
        .await
        .unwrap()
        .id;
    let foreign = db
        .create_conversation("Other", Mode::Chat, None)
        .await
        .unwrap()
        .id;
    let mut refs = Vec::new();
    for bytes in [b"%PDF fixture A", b"%PDF fixture B"] {
        let meta = store
            .save_bytes(
                &conv,
                AttachmentKind::Pdf,
                AttachmentOrigin::Upload,
                "duplicate.pdf",
                "application/pdf",
                bytes,
            )
            .await
            .unwrap();
        refs.push(
            db.get_attachment(meta.id)
                .await
                .unwrap()
                .unwrap()
                .attachment_ref(),
        );
    }
    let (tx, mut rx) = mpsc::channel(4);
    let hub = Arc::new(Hub::new());
    let previews = Arc::new(PreviewRegistry::new());
    let engine = Engine::new(
        Arc::new(Capture(tx)),
        vortex_agents::build_registry(previews.clone(), Arc::new(std::sync::OnceLock::new())),
        hub.clone(),
        Arc::new(Scheduler::new(1)),
        Arc::new(vortex_agents::approvals::ApprovalManager::new(
            db.clone(),
            hub,
            30,
        )),
        db.clone(),
        Arc::new(vortex_agents::browser::BrowserManager::new(
            dir.path().join("profile"),
            true,
        )),
        previews,
        None,
        vortex_agents::safe_http::SafeHttp::new(false),
        Settings::default(),
        Some(store.clone()),
    );

    let mut user = ChatMessage::user("initial PDFs");
    user.attachments.clone_from(&refs);
    db.add_message(conv.clone(), user).await.unwrap();
    let initial = run(
        &engine,
        &mut rx,
        &conv,
        vec![],
        "initial PDFs",
        refs.clone(),
    )
    .await;
    assert!(initial.assistant_annotations.is_empty());
    let filenames = names(&initial);
    assert_eq!(filenames.len(), 2);
    assert_ne!(filenames[0], filenames[1]);
    let original_history = db.get_messages(conv.clone()).await.unwrap();
    assert_eq!(original_history.len(), 2);
    assert_eq!(original_history[1].role, Role::Assistant);
    for (r, wire_name) in refs.iter().zip(&filenames) {
        let cached = db
            .get_file_annotations(conv.clone(), r.id.clone(), 2_000_000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cached["parser"], "cloudflare-ai");
        assert_eq!(cached["assistant_id"], original_history[1].id);
        assert_eq!(cached["annotation"]["file"]["name"], *wire_name);
        assert!(db
            .get_file_annotations(foreign.clone(), r.id.clone(), 2_000_000)
            .await
            .unwrap()
            .is_none());
    }

    let retry = run(&engine, &mut rx, &conv, vec![], "retry", refs.clone()).await;
    assert_eq!(names(&retry), filenames);
    assert!(
        retry.assistant_annotations.is_empty(),
        "retry lacks a suitable assistant message"
    );

    let followup = run(
        &engine,
        &mut rx,
        &conv,
        original_history.clone(),
        "follow up",
        vec![],
    )
    .await;
    assert_eq!(names(&followup), filenames);
    assert_eq!(followup.assistant_annotations.len(), 1);
    let replayed = followup.assistant_annotations.get(&2).unwrap();
    assert_eq!(replayed.len(), 2);
    assert!(filenames
        .iter()
        .all(|name| replayed.contains(&annotation(Some(name)))));

    let mut settings = engine.current_settings().await;
    settings.media.pdf_engine = "mistral-ocr".into();
    engine.update_settings(settings.clone()).await;
    let switched = run(
        &engine,
        &mut rx,
        &conv,
        original_history.clone(),
        "parser changed",
        vec![],
    )
    .await;
    assert_eq!(switched.pdf_engine, Some(PdfEngine::MistralOcr));
    assert_eq!(names(&switched), filenames);
    assert!(switched.assistant_annotations.is_empty());

    settings.media.pdf_engine = "cloudflare-ai".into();
    engine.update_settings(settings).await;
    assert!(store.delete(&refs[0].id).await.unwrap());
    let deleted = run(
        &engine,
        &mut rx,
        &conv,
        original_history,
        "one deleted",
        vec![],
    )
    .await;
    assert_eq!(names(&deleted), [filenames[1]]);
    assert_eq!(
        deleted.assistant_annotations.get(&2),
        Some(&vec![annotation(Some(filenames[1]))])
    );
    assert!(db
        .get_file_annotations(conv, refs[0].id.clone(), 2_000_000)
        .await
        .unwrap()
        .is_none());
}
