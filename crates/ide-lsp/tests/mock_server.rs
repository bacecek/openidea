//! Protocol tests against in-memory mock language servers.

use ide_core::event::{LspConflict, LspPrompt, NotifyLevel, ServerStatus};
use ide_core::{DocId, FileEdit, IdeEvent, LanguageId, Pos, Range, ServerId, Severity, WorkspaceEdit};
use ide_jsonrpc::{Handler, NoopHandler, Peer};
use ide_language::{LanguageConfig, LanguageRegistry, LanguageServerConfig};
use ide_lsp::types::*;
use ide_lsp::{LaunchError, Launcher, LspManager};
use ide_text::AppliedChange;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

// ------------------------------------------------------------------ mock server

#[derive(Default)]
struct MockState {
    caps: Mutex<Value>,
    missing: std::sync::atomic::AtomicBool,
    launches: AtomicUsize,
    notifications: Mutex<Vec<(String, Value)>>,
    requests: Mutex<Vec<(String, Value)>>,
    /// uri → text as reconstructed by the mock from didOpen/didChange.
    docs: Mutex<HashMap<String, String>>,
    responses: Mutex<HashMap<String, Value>>,
    errors: Mutex<HashMap<String, (i64, String)>>,
    exit_code: Mutex<Option<i32>>,
    init_gate: Mutex<Option<Arc<Gate>>>,
    shutdown_gate: Mutex<Option<Arc<Gate>>>,
    /// Computes a response from the request and the open documents (before `responses`).
    responder: Mutex<Option<Responder>>,
    peer: Mutex<Option<Peer>>,
}

type Responder = Arc<dyn Fn(&MockState, &str, &Value) -> Option<Value> + Send + Sync>;

impl MockState {
    fn notified(&self, method: &str) -> Vec<Value> {
        self.notifications.lock().iter().filter(|(m, _)| m == method).map(|(_, p)| p.clone()).collect()
    }
    fn requested(&self, method: &str) -> Vec<Value> {
        self.requests.lock().iter().filter(|(m, _)| m == method).map(|(_, p)| p.clone()).collect()
    }
    fn respond(&self, method: &str, v: Value) {
        self.responses.lock().insert(method.to_owned(), v);
    }
    fn peer(&self) -> Peer {
        self.peer.lock().clone().expect("server running")
    }
}

fn utf16_to_byte(text: &str, line: u64, col: u64) -> usize {
    let mut offset = 0;
    for (i, l) in text.split('\n').enumerate() {
        if i as u64 == line {
            let mut units = 0u64;
            for (b, c) in l.char_indices() {
                if units >= col {
                    return offset + b;
                }
                units += c.len_utf16() as u64;
            }
            return offset + l.len();
        }
        offset += l.len() + 1;
    }
    text.len()
}

struct MockHandler(Arc<MockState>);

impl Handler for MockHandler {
    fn on_notification(&self, method: &str, params: Value) {
        let s = &self.0;
        // Held throughout: a test that sees the effect (a document, the peer gone) also sees the record.
        let mut log = s.notifications.lock();
        let uri = params["textDocument"]["uri"].as_str().unwrap_or_default().to_owned();
        match method {
            "textDocument/didOpen" => {
                s.docs.lock().insert(uri, params["textDocument"]["text"].as_str().unwrap().to_owned());
            }
            "textDocument/didChange" => {
                let mut docs = s.docs.lock();
                let text = docs.get_mut(&uri).expect("didChange before didOpen");
                for change in params["contentChanges"].as_array().unwrap() {
                    let new = change["text"].as_str().unwrap();
                    match change.get("range") {
                        Some(r) => {
                            let a = utf16_to_byte(
                                text,
                                r["start"]["line"].as_u64().unwrap(),
                                r["start"]["character"].as_u64().unwrap(),
                            );
                            let b = utf16_to_byte(
                                text,
                                r["end"]["line"].as_u64().unwrap(),
                                r["end"]["character"].as_u64().unwrap(),
                            );
                            text.replace_range(a..b, new);
                        }
                        None => *text = new.to_owned(),
                    }
                }
            }
            "textDocument/didClose" => {
                s.docs.lock().remove(&uri);
            }
            "exit" => {
                if let Some(p) = s.peer.lock().take() {
                    p.kill();
                }
            }
            _ => {}
        }
        log.push((method.to_owned(), params));
    }

    fn on_request(&self, method: &str, params: Value) -> Result<Value, (i64, String)> {
        let s = &self.0;
        s.requests.lock().push((method.to_owned(), params.clone()));
        match method {
            "initialize" => {
                if let Some(gate) = s.init_gate.lock().clone() {
                    gate.hold();
                }
                Ok(json!({ "capabilities": s.caps.lock().clone(), "serverInfo": { "name": "mock" } }))
            }
            "shutdown" => {
                if let Some(gate) = s.shutdown_gate.lock().clone() {
                    gate.hold();
                }
                Ok(Value::Null)
            }
            _ => {
                if let Some(e) = s.errors.lock().get(method) {
                    return Err(e.clone());
                }
                let responder = s.responder.lock().clone();
                if let Some(v) = responder.and_then(|f| f(s, method, &params)) {
                    return Ok(v);
                }
                Ok(s.responses.lock().get(method).cloned().unwrap_or(Value::Null))
            }
        }
    }
}

#[derive(Default)]
struct MockLauncher {
    servers: Mutex<HashMap<String, Arc<MockState>>>,
}

impl MockLauncher {
    fn state(&self, name: &str) -> Arc<MockState> {
        self.servers.lock().entry(name.to_owned()).or_default().clone()
    }
}

impl Launcher for MockLauncher {
    fn launch(
        &self,
        config: &LanguageServerConfig,
        root: &Path,
        handler: Arc<dyn Handler>,
        on_stderr: Box<dyn Fn(String) + Send + Sync>,
    ) -> Result<Peer, LaunchError> {
        let state = self.state(&config.name);
        if state.missing.load(Ordering::SeqCst) {
            return Err(LaunchError::Missing);
        }
        state.launches.fetch_add(1, Ordering::SeqCst);
        if let Some(code) = *state.exit_code.lock() {
            let script = format!("exit {code}");
            return Peer::spawn("sh", &["-c".into(), script], root, &[], handler, on_stderr)
                .map_err(|e| LaunchError::Failed(e.to_string()));
        }
        state.docs.lock().clear();
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let (sr, sw) = tokio::io::split(server_io);
        let server = Peer::connect(sr, sw, Arc::new(MockHandler(state.clone())));
        *state.peer.lock() = Some(server);
        let (cr, cw) = tokio::io::split(client_io);
        Ok(Peer::connect(cr, cw, handler))
    }
}

// ------------------------------------------------------------------ harness

fn incremental_caps() -> Value {
    json!({
        "textDocumentSync": { "openClose": true, "change": 2, "save": { "includeText": true } },
        "completionProvider": { "resolveProvider": true, "triggerCharacters": ["."] },
        "hoverProvider": true,
        "definitionProvider": true,
        "referencesProvider": true,
        "codeActionProvider": { "resolveProvider": true },
        "renameProvider": { "prepareProvider": true },
        "documentFormattingProvider": true,
        "executeCommandProvider": { "commands": ["mock.run"] },
        "semanticTokensProvider": { "legend": { "tokenTypes": ["function", "variable"], "tokenModifiers": ["declaration"] }, "full": true },
        "codeLensProvider": { "resolveProvider": true },
        "callHierarchyProvider": true,
        "inlayHintProvider": true
    })
}

struct Harness {
    _dir: tempfile::TempDir,
    root: PathBuf,
    mgr: Arc<LspManager>,
    launcher: Arc<MockLauncher>,
    events: Arc<Mutex<Vec<IdeEvent>>>,
    lang: LanguageId,
}

fn server_cfg(name: &str) -> LanguageServerConfig {
    LanguageServerConfig {
        name: name.into(),
        command: name.into(),
        args: vec![],
        env: Default::default(),
        root_markers: vec!["mock.toml".into()],
        initialization_options: Some(toml::Value::Table(toml::toml! { flag = true nested = { deep = 1 } })),
        settings: None,
        install_hint: Some(format!("install {name}")),
        connect: None,
        fallback: false,
        max_java: None,
        launch_error: None,
    }
}

fn harness(servers: &[(&str, Value)]) -> Harness {
    harness_with_fallbacks(servers, &[])
}

/// `fallbacks`: servers configured as alternatives to the ones listed before them.
fn harness_with_fallbacks(servers: &[(&str, Value)], fallbacks: &[&str]) -> Harness {
    harness_for("mock", "mk", servers, fallbacks)
}

fn harness_for(lang_id: &str, ext: &str, servers: &[(&str, Value)], fallbacks: &[&str]) -> Harness {
    let lang = LanguageConfig {
        id: LanguageId::new(lang_id),
        name: "Mock".into(),
        extensions: vec![ext.into()],
        filenames: vec![],
        first_line: None,
        line_comment: None,
        block_comment: None,
        indent: None,
        ui_mode: None,
        tree_sitter: None,
        language_servers: servers
            .iter()
            .map(|(n, _)| LanguageServerConfig { fallback: fallbacks.contains(n), ..server_cfg(n) })
            .collect(),
        debugger: None,
    };
    harness_with_registry(LanguageRegistry::from_configs(vec![lang]), lang_id, servers)
}

fn bundled_harness(lang_id: &str, servers: &[(&str, Value)]) -> Harness {
    harness_with_registry(LanguageRegistry::load(None).unwrap(), lang_id, servers)
}

fn harness_with_registry(registry: LanguageRegistry, lang_id: &str, servers: &[(&str, Value)]) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let registry = Arc::new(registry);
    let launcher = Arc::new(MockLauncher::default());
    for (name, caps) in servers {
        *launcher.state(name).caps.lock() = caps.clone();
    }
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let events = events.clone();
        Arc::new(move |e: IdeEvent| events.lock().push(e))
    };
    let mgr = LspManager::with_launcher(root.clone(), registry, sink, launcher.clone());
    Harness { _dir: dir, root, mgr, launcher, events, lang: LanguageId::new(lang_id) }
}

impl Harness {
    fn open(&self, name: &str, text: &str) -> (DocId, PathBuf) {
        let path = self.root.join(name);
        std::fs::write(&path, text).unwrap();
        let doc = DocId::next();
        self.mgr.did_open(doc, &path, &self.lang, 1, text);
        (doc, path)
    }

    fn statuses(&self, name: &str) -> Vec<ServerStatus> {
        self.events
            .lock()
            .iter()
            .filter_map(|e| match e {
                IdeEvent::LanguageServer { name: n, status, .. } if n == name => Some(*status),
                _ => None,
            })
            .collect()
    }

    /// Whether every instance of `name` was reported as removed from the server list.
    fn removed(&self, name: &str) -> bool {
        let events = self.events.lock();
        let ids: HashSet<ServerId> = events
            .iter()
            .filter_map(|e| match e {
                IdeEvent::LanguageServer { server, name: n, .. } if n == name => Some(*server),
                _ => None,
            })
            .collect();
        !ids.is_empty()
            && ids.iter().all(|id| {
                events.iter().any(|e| matches!(e, IdeEvent::LanguageServerRemoved { server } if server == id))
            })
    }

    async fn wait_status(&self, name: &str, status: ServerStatus) {
        eventually(|| self.statuses(name).last() == Some(&status)).await;
    }
}

/// Waits for `f`; the bound only keeps a broken test from hanging (machines under load are slow).
async fn eventually(mut f: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !f() {
        assert!(std::time::Instant::now() < deadline, "condition not reached in time");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Holds mock request handlers until the test opens it.
#[derive(Default)]
struct Gate {
    entered: std::sync::atomic::AtomicBool,
    open: std::sync::atomic::AtomicBool,
}

impl Gate {
    /// Called from a handler: blocks until [`Gate::open`] (or a generous bound).
    fn hold(&self) {
        self.entered.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !self.open.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn entered(&self) -> bool {
        self.entered.load(Ordering::SeqCst)
    }
    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
    }
}

fn uri_of(path: &Path) -> String {
    ide_lsp::path_to_uri_string(path)
}

fn rng(l0: u32, c0: u32, l1: u32, c1: u32) -> Range {
    Range::new(Pos::new(l0, c0), Pos::new(l1, c1))
}

fn change(range: Range, text: &str) -> AppliedChange {
    AppliedChange { range, text: text.into() }
}

// ------------------------------------------------------------------ tests

#[tokio::test(flavor = "multi_thread")]
async fn initialize_and_incremental_sync() {
    let h = harness(&[("mock", incremental_caps())]);
    let (doc, path) = h.open("a.mk", "fn main() {\n}\n");
    h.wait_status("mock", ServerStatus::Ready).await;
    let s = h.launcher.state("mock");

    let init = &s.requested("initialize")[0];
    assert_eq!(init["clientInfo"]["name"], "OpenIDEA");
    assert_eq!(init["capabilities"]["general"]["positionEncodings"], json!(["utf-16"]));
    assert_eq!(init["capabilities"]["textDocument"]["completion"]["completionItem"]["snippetSupport"], true);
    assert_eq!(init["initializationOptions"], json!({ "flag": true, "nested": { "deep": 1 } }));
    assert_eq!(init["rootUri"], json!(uri_of(&h.root)));
    eventually(|| s.notified("initialized").len() == 1).await;
    assert_eq!(h.statuses("mock"), vec![ServerStatus::Starting, ServerStatus::Ready]);

    let uri = uri_of(&path);
    eventually(|| s.docs.lock().get(&uri).map(String::as_str) == Some("fn main() {\n}\n")).await;
    let open = &s.notified("textDocument/didOpen")[0];
    assert_eq!(open["textDocument"]["languageId"], "mock");
    assert_eq!(open["textDocument"]["version"], 1);

    let full = || -> String { panic!("full text must not be needed") };
    h.mgr.did_change(doc, 2, &[change(rng(0, 11, 0, 11), "\n    let é😀 = 1;")], &full);
    h.mgr.did_change(doc, 3, &[change(rng(1, 10, 1, 10), "x"), change(rng(1, 4, 1, 7), "const")], &full);
    let expected = "fn main() {\n    const é😀x = 1;\n}\n";
    eventually(|| s.docs.lock().get(&uri).map(String::as_str) == Some(expected)).await;
    let changes = s.notified("textDocument/didChange");
    assert_eq!(changes.last().unwrap()["textDocument"]["version"], 3);
    assert_eq!(changes.last().unwrap()["contentChanges"].as_array().unwrap().len(), 2);

    h.mgr.did_save(doc, expected);
    eventually(|| s.notified("textDocument/didSave").len() == 1).await;
    assert_eq!(s.notified("textDocument/didSave")[0]["text"], expected);

    h.mgr.did_close(doc);
    eventually(|| s.notified("textDocument/didClose").len() == 1).await;
    assert!(s.docs.lock().is_empty());

    let infos = h.mgr.servers();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].name, "mock");
    assert_eq!(infos[0].root, h.root);
    assert_eq!(infos[0].status, ServerStatus::Ready);
}

#[tokio::test(flavor = "multi_thread")]
async fn full_sync_and_desync_recovery() {
    let h = harness(&[("full", json!({ "textDocumentSync": 1 })), ("inc", json!({ "textDocumentSync": 2 }))]);
    let (doc, path) = h.open("a.mk", "abc");
    h.wait_status("full", ServerStatus::Ready).await;
    h.wait_status("inc", ServerStatus::Ready).await;
    let (full, inc) = (h.launcher.state("full"), h.launcher.state("inc"));
    let uri = uri_of(&path);
    eventually(|| full.docs.lock().contains_key(&uri) && inc.docs.lock().contains_key(&uri)).await;

    h.mgr.did_change(doc, 2, &[change(rng(0, 3, 0, 3), "d")], &|| unreachable!());
    eventually(|| full.docs.lock()[&uri] == "abcd" && inc.docs.lock()[&uri] == "abcd").await;
    assert!(full.notified("textDocument/didChange")[0]["contentChanges"][0].get("range").is_none());
    assert!(inc.notified("textDocument/didChange")[0]["contentChanges"][0].get("range").is_some());

    // A change that does not fit the tracked text forces a full resync from the source of truth.
    h.mgr.did_change(doc, 3, &[change(rng(7, 0, 7, 0), "zz")], &|| "truth".to_owned());
    eventually(|| full.docs.lock()[&uri] == "truth" && inc.docs.lock()[&uri] == "truth").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn completion_mapping_merge_and_resolve() {
    let h = harness(&[("a", incremental_caps()), ("b", json!({ "textDocumentSync": 2, "completionProvider": {} }))]);
    let (doc, _) = h.open("a.mk", "let x = foo.ba");
    h.wait_status("a", ServerStatus::Ready).await;
    h.wait_status("b", ServerStatus::Ready).await;
    let (a, b) = (h.launcher.state("a"), h.launcher.state("b"));
    a.respond(
        "textDocument/completion",
        json!({
            "isIncomplete": false,
            "itemDefaults": { "data": { "from": "defaults" } },
            "items": [{
                "label": "bar", "kind": 2, "detail": "fn() -> u8", "labelDetails": { "detail": "()" },
                "textEdit": { "newText": "bar()$0", "insert": {"start":{"line":0,"character":12},"end":{"line":0,"character":14}},
                              "replace": {"start":{"line":0,"character":12},"end":{"line":0,"character":15}} },
                "insertTextFormat": 2, "sortText": "0001"
            }]
        }),
    );
    b.respond("textDocument/completion", json!({ "isIncomplete": true, "items": [{ "label": "baz" }] }));
    a.respond("completionItem/resolve", json!({
        "label": "bar", "documentation": { "kind": "markdown", "value": "**docs**" },
        "additionalTextEdits": [{ "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":0}}, "newText": "use foo;\n" }]
    }));

    let list = h.mgr.completion(doc, Pos::new(0, 14), Some(".".into()), Default::default()).await.unwrap();
    assert!(list.is_incomplete);
    assert_eq!(list.items.len(), 2);
    let bar = list.items.iter().find(|i| i.label == "bar").unwrap().clone();
    assert_eq!(bar.kind, Some(CompletionKind::Method));
    assert_eq!(bar.insert_text, "bar()$0");
    assert!(bar.is_snippet);
    assert_eq!(bar.label_detail.as_deref(), Some("()"));
    assert_eq!(bar.replace_range, Some(rng(0, 12, 0, 14)));
    assert_eq!(bar.replace_range_tab, Some(rng(0, 12, 0, 15)));
    assert_eq!(bar.data, Some(json!({ "from": "defaults" })));
    let baz = list.items.iter().find(|i| i.label == "baz").unwrap();
    assert_eq!(baz.insert_text, "baz");
    assert_ne!(baz.server, bar.server);

    let req = &a.requested("textDocument/completion")[0];
    assert_eq!(req["context"], json!({ "triggerKind": 2, "triggerCharacter": "." }));
    assert_eq!(b.requested("textDocument/completion")[0]["context"], json!({ "triggerKind": 1 }));

    let resolved = h.mgr.resolve_completion(doc, bar.clone()).await.unwrap();
    assert_eq!(resolved.documentation.as_ref().unwrap().value, "**docs**");
    assert_eq!(resolved.additional_edits[0].new_text, "use foo;\n");
    assert_eq!(resolved.insert_text, "bar()$0", "fields absent from the resolve reply are kept");
    assert_eq!(resolved.replace_range_tab, Some(rng(0, 12, 0, 15)));
    let sent = &a.requested("completionItem/resolve")[0];
    assert_eq!(sent["data"], json!({ "from": "defaults" }));
    assert_eq!(sent["textEdit"]["replace"]["end"]["character"], 15);

    // Server b has no resolve support: the item comes back unchanged.
    let baz2 = h.mgr.resolve_completion(doc, baz.clone()).await.unwrap();
    assert_eq!(baz2.label, "baz");
    assert!(b.requested("completionItem/resolve").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn inline_completion_only_from_servers_that_advertise_it() {
    let h = harness(&[
        ("plain", incremental_caps()),
        ("ghost", json!({ "textDocumentSync": 2, "inlineCompletionProvider": {} })),
        ("late", json!({ "textDocumentSync": 2 })),
    ]);
    let (doc, _) = h.open("a.mk", "let x = fo");
    for name in ["plain", "ghost", "late"] {
        h.wait_status(name, ServerStatus::Ready).await;
    }
    let (plain, ghost) = (h.launcher.state("plain"), h.launcher.state("ghost"));
    let init = &plain.requested("initialize")[0];
    assert_eq!(init["capabilities"]["textDocument"]["inlineCompletion"], json!({ "dynamicRegistration": true }));
    ghost.respond(
        "textDocument/inlineCompletion",
        json!({ "items": [
            { "insertText": "foo(1)", "range": {"start":{"line":0,"character":8},"end":{"line":0,"character":10}},
              "command": { "title": "accepted", "command": "ghost.accepted", "arguments": [7] } },
            { "insertText": { "kind": "snippet", "value": "foo(${1:a}, $2)$0" }, "filterText": "fo" },
            { "insertText": "" }
        ] }),
    );
    let items = h.mgr.inline_completion(doc, Pos::new(0, 10), true, Default::default()).await.unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].insert_text, "foo(1)");
    assert_eq!(items[0].range, Some(rng(0, 8, 0, 10)));
    assert_eq!(items[0].command.as_ref().unwrap().command, "ghost.accepted");
    assert_eq!(items[1].insert_text, "foo(a, )");
    assert_eq!(items[1].filter_text.as_deref(), Some("fo"));
    let req = &ghost.requested("textDocument/inlineCompletion")[0];
    assert_eq!(req["context"], json!({ "triggerKind": 2 }));
    assert_eq!(req["position"], json!({ "line": 0, "character": 10 }));
    assert!(plain.requested("textDocument/inlineCompletion").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostics_become_events_and_feed_code_actions() {
    let h = harness(&[("mock", incremental_caps())]);
    let (doc, path) = h.open("a.mk", "let unused = 1;\nlet y = 2;\n");
    h.wait_status("mock", ServerStatus::Ready).await;
    let s = h.launcher.state("mock");
    let diag = |line: u32| {
        json!({
            "range": {"start":{"line":line,"character":4},"end":{"line":line,"character":10}},
            "severity": 2, "code": "W1", "message": "unused variable", "tags": [1], "data": { "fix": line }
        })
    };
    s.peer()
        .notify("textDocument/publishDiagnostics", json!({ "uri": uri_of(&path), "diagnostics": [diag(0), diag(1)] }))
        .unwrap();
    eventually(|| h.events.lock().iter().any(|e| matches!(e, IdeEvent::Diagnostics { .. }))).await;
    {
        let events = h.events.lock();
        let (p, source, items) = events
            .iter()
            .find_map(|e| match e {
                IdeEvent::Diagnostics { path, source, items } => Some((path.clone(), source.clone(), items.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(p, path);
        assert_eq!(source, "mock");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].severity, Severity::Warning);
        assert_eq!(items[0].code.as_deref(), Some("W1"));
        assert!(items[0].unnecessary);
        assert_eq!(items[0].range, rng(0, 4, 0, 10));
    }

    // A reloaded UI asks for them again.
    h.events.lock().clear();
    h.mgr.resend_diagnostics();
    let again = h.events.lock().iter().find_map(|e| match e {
        IdeEvent::Diagnostics { path, items, .. } => Some((path.clone(), items.len())),
        _ => None,
    });
    assert_eq!(again, Some((path.clone().into(), 2)));

    s.respond(
        "textDocument/codeAction",
        json!([
            { "title": "Remove", "kind": "quickfix", "isPreferred": true, "data": { "id": 7 } },
            { "title": "Run", "command": "mock.run", "arguments": [1] }
        ]),
    );
    s.respond(
        "codeAction/resolve",
        json!({
            "title": "Remove", "edit": { "changes": { uri_of(&path): [
                { "range": {"start":{"line":0,"character":0},"end":{"line":1,"character":0}}, "newText": "" } ] } }
        }),
    );
    let actions = h.mgr.code_actions(doc, rng(0, 5, 0, 5), vec!["quickfix".into()]).await.unwrap();
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0].kind.as_deref(), Some("quickfix"));
    assert!(actions[0].is_preferred);
    assert_eq!(actions[1].command.as_ref().unwrap().command, "mock.run");
    let req = &s.requested("textDocument/codeAction")[0];
    assert_eq!(req["context"]["only"], json!(["quickfix"]));
    let ctx = req["context"]["diagnostics"].as_array().unwrap();
    assert_eq!(ctx.len(), 1, "only diagnostics overlapping the range");
    assert_eq!(ctx[0]["data"], json!({ "fix": 0 }));

    let resolved = h.mgr.resolve_code_action(doc, actions[0].clone()).await.unwrap();
    assert_eq!(s.requested("codeAction/resolve")[0]["data"], json!({ "id": 7 }));
    let edit = resolved.edit.unwrap();
    assert!(
        matches!(&edit.files[0], FileEdit::Edit { path: p, edits } if p == &path && edits[0].range == rng(0, 0, 1, 0))
    );

    h.mgr.execute_command(doc, actions[1].command.clone().unwrap()).await.unwrap();
    assert_eq!(s.requested("workspace/executeCommand")[0], json!({ "command": "mock.run", "arguments": [1] }));
    let err = h
        .mgr
        .execute_command(doc, Command { title: "x".into(), command: "other".into(), arguments: vec![] })
        .await
        .unwrap_err();
    assert_eq!(err.kind, ide_core::error::ErrorKind::Unavailable);
}

#[tokio::test(flavor = "multi_thread")]
async fn apply_edit_hook_and_server_requests() {
    let h = harness(&[("mock", incremental_caps())]);
    let (_doc, path) = h.open("a.mk", "abc\n");
    h.wait_status("mock", ServerStatus::Ready).await;
    let s = h.launcher.state("mock");
    let applied: Arc<Mutex<Vec<WorkspaceEdit>>> = Arc::default();
    {
        let applied = applied.clone();
        h.mgr.set_apply_edit_hook(Box::new(move |e| {
            applied.lock().push(e);
            true
        }));
    }
    let new_file = h.root.join("new.mk");
    let reply: Value = s
        .peer()
        .request(
            "workspace/applyEdit",
            json!({ "edit": { "documentChanges": [
                { "kind": "create", "uri": uri_of(&new_file) },
                { "textDocument": { "uri": uri_of(&path), "version": 1 },
                  "edits": [{ "range": {"start":{"line":0,"character":1},"end":{"line":0,"character":2}}, "newText": "X" }] }
            ] } }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(reply, json!({ "applied": true }));
    let edits = applied.lock().clone();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].files[0], FileEdit::Create { path: new_file.into(), overwrite: false });
    assert!(matches!(&edits[0].files[1], FileEdit::Edit { path: p, edits } if p == &path && edits[0].new_text == "X"));

    let peer = s.peer();
    let cfg: Value = peer
        .request(
            "workspace/configuration",
            json!({ "items": [{ "section": "mock" }, { "section": "nested.deep" }, { "section": "nope" }] }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(cfg, json!([{ "flag": true, "nested": { "deep": 1 } }, 1, null]));
    let folders: Value = peer.request("workspace/workspaceFolders", Value::Null, None).await.unwrap();
    assert_eq!(folders[0]["uri"], json!(uri_of(&h.root)));
    let v: Value = peer.request("window/workDoneProgress/create", json!({ "token": "t" }), None).await.unwrap();
    assert_eq!(v, Value::Null);
    let v: Value = peer
        .request("window/showMessageRequest", json!({ "type": 1, "message": "boom", "actions": [] }), None)
        .await
        .unwrap();
    assert_eq!(v, Value::Null);
    let v: Value = peer
        .request(
            "client/registerCapability",
            json!({ "registrations": [{ "id": "1", "method": "textDocument/documentHighlight" }] }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(v, Value::Null);
    assert!(peer.request::<_, Value>("unknown/method", Value::Null, None).await.is_err());

    peer.notify("window/showMessage", json!({ "type": 2, "message": "careful" })).unwrap();
    peer.notify("window/showMessage", json!({ "type": 3, "message": "info only" })).unwrap();
    peer.notify("window/logMessage", json!({ "type": 1, "message": "log" })).unwrap();
    eventually(|| {
        h.events.lock().iter().any(|e| matches!(e, IdeEvent::Notify { level: ide_core::event::NotifyLevel::Warning, body: Some(b), .. } if b == "careful"))
    })
    .await;
    let notifies = h.events.lock().iter().filter(|e| matches!(e, IdeEvent::Notify { .. })).count();
    assert_eq!(notifies, 2, "showMessageRequest error + showMessage warning; info is not surfaced");

    // Dynamically registered capability is now usable.
    s.respond(
        "textDocument/documentHighlight",
        json!([{ "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":3}}, "kind": 3 }]),
    );
    let hl = h.mgr.document_highlights(_doc, Pos::new(0, 1)).await.unwrap();
    assert_eq!(hl[0].kind, HighlightKind::Write);
}

#[tokio::test(flavor = "multi_thread")]
async fn progress_tasks_and_busy_status() {
    let h = harness(&[("mock", incremental_caps())]);
    h.open("a.mk", "x");
    h.wait_status("mock", ServerStatus::Ready).await;
    let peer = h.launcher.state("mock").peer();
    peer.notify(
        "$/progress",
        json!({ "token": 1, "value": { "kind": "begin", "title": "Indexing", "percentage": 0 } }),
    )
    .unwrap();
    peer.notify(
        "$/progress",
        json!({ "token": 1, "value": { "kind": "report", "message": "3/10", "percentage": 30 } }),
    )
    .unwrap();
    h.wait_status("mock", ServerStatus::Busy).await;
    peer.notify("$/progress", json!({ "token": 1, "value": { "kind": "end" } })).unwrap();
    h.wait_status("mock", ServerStatus::Ready).await;
    let tasks: Vec<(String, Option<String>, Option<f32>, bool)> = h
        .events
        .lock()
        .iter()
        .filter_map(|e| match e {
            IdeEvent::Task { title, message, fraction, done, .. } => {
                Some((title.clone(), message.clone(), *fraction, *done))
            }
            _ => None,
        })
        .collect();
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[0], ("Indexing".into(), None, Some(0.0), false));
    assert_eq!(tasks[1], ("Indexing".into(), Some("3/10".into()), Some(0.3), false));
    assert!(tasks[2].3);
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_restarts_and_reopens_documents() {
    let h = harness(&[("mock", incremental_caps())]);
    let (doc, path) = h.open("a.mk", "one");
    h.wait_status("mock", ServerStatus::Ready).await;
    let s = h.launcher.state("mock");
    let uri = uri_of(&path);
    eventually(|| s.docs.lock().contains_key(&uri)).await;
    s.peer()
        .notify(
            "textDocument/publishDiagnostics",
            json!({ "uri": uri, "diagnostics": [
        { "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}}, "message": "e" }] }),
        )
        .unwrap();
    eventually(|| {
        h.events.lock().iter().any(|e| matches!(e, IdeEvent::Diagnostics { items, .. } if !items.is_empty()))
    })
    .await;

    // Edit while running, then crash.
    h.mgr.did_change(doc, 2, &[change(rng(0, 3, 0, 3), " two")], &|| unreachable!());
    eventually(|| s.docs.lock().get(&uri).map(String::as_str) == Some("one two")).await;
    s.peer().kill();

    eventually(|| h.statuses("mock").contains(&ServerStatus::Crashed)).await;
    // Diagnostics of the dead instance are withdrawn.
    eventually(|| h.events.lock().iter().any(|e| matches!(e, IdeEvent::Diagnostics { items, .. } if items.is_empty())))
        .await;
    eventually(|| s.launches.load(Ordering::SeqCst) == 2).await;
    h.wait_status("mock", ServerStatus::Ready).await;
    eventually(|| s.docs.lock().get(&uri).map(String::as_str) == Some("one two")).await;
    let opens = s.notified("textDocument/didOpen");
    assert_eq!(opens.last().unwrap()["textDocument"]["version"], 2);

    // Features work again on the new instance.
    s.respond("textDocument/hover", json!({ "contents": { "kind": "markdown", "value": "hi" } }));
    let hover = h.mgr.hover(doc, Pos::new(0, 1), Default::default()).await.unwrap().unwrap();
    assert_eq!(hover.contents.value, "hi");
}

#[tokio::test(flavor = "multi_thread")]
async fn gives_up_after_repeated_crashes_and_manual_restart_works() {
    let h = harness(&[("mock", incremental_caps())]);
    h.open("a.mk", "x");
    let s = h.launcher.state("mock");
    let count = |status| h.statuses("mock").iter().filter(|s| **s == status).count();
    for n in 1..=4 {
        // Counted, not the last status: a restart may already have replaced it.
        eventually(|| count(ServerStatus::Ready) == n).await;
        assert_eq!(s.launches.load(Ordering::SeqCst), n);
        s.peer().kill();
        eventually(|| count(ServerStatus::Crashed) == n).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.launches.load(Ordering::SeqCst), 4, "initial start + 3 restarts");
    let last = h.events.lock().iter().rev().find_map(|e| match e {
        IdeEvent::LanguageServer { message, .. } => message.clone(),
        _ => None,
    });
    assert!(last.unwrap().contains("restart it manually"));

    let id = h.mgr.servers()[0].id;
    h.mgr.restart(id).await.unwrap();
    assert_eq!(s.launches.load(Ordering::SeqCst), 5);
    assert_eq!(h.mgr.servers()[0].status, ServerStatus::Ready);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_server_reports_once() {
    let h = harness(&[("ghost", json!({}))]);
    h.launcher.state("ghost").missing.store(true, Ordering::SeqCst);
    let (doc, _) = h.open("a.mk", "x");
    h.wait_status("ghost", ServerStatus::Missing).await;
    h.open("b.mk", "y");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let missing: Vec<Option<String>> = h
        .events
        .lock()
        .iter()
        .filter_map(|e| match e {
            IdeEvent::LanguageServer { status: ServerStatus::Missing, message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(missing, vec![Some("install ghost".to_owned())]);
    let err = h.mgr.hover(doc, Pos::new(0, 0), Default::default()).await.unwrap_err();
    assert_eq!(err.kind, ide_core::error::ErrorKind::Unavailable);
    assert_eq!(h.mgr.document_symbols(doc).await.unwrap(), None);
    assert!(h.mgr.semantic_tokens(doc).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn launch_error_fails_without_starting_the_server() {
    let h = harness(&[("mock", incremental_caps())]);
    h.mgr.set_config_hook(Box::new(|cfg, _| cfg.launch_error = Some("needs JDK 21".into())));
    assert!(!h.mgr.serves_path(&h.root.join("a.mk")), "batch inspection skips it");
    h.open("a.mk", "x");
    // Not a crash: nothing ran, the configuration has to change.
    h.wait_status("mock", ServerStatus::Misconfigured).await;
    assert_eq!(h.launcher.state("mock").launches.load(Ordering::SeqCst), 0);
    let reason = h.events.lock().iter().find_map(|e| match e {
        IdeEvent::LanguageServer { status: ServerStatus::Misconfigured, message, .. } => message.clone(),
        _ => None,
    });
    assert_eq!(reason.as_deref(), Some("needs JDK 21"));
    assert!(!h.statuses("mock").contains(&ServerStatus::Crashed));
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_changes_reach_servers_without_restarts() {
    let h = harness(&[("mock", incremental_caps()), ("extra", incremental_caps())]);
    let settings: Arc<Mutex<Option<Value>>> = Default::default();
    let disabled: Arc<Mutex<Vec<String>>> = Default::default();
    {
        let settings = settings.clone();
        h.mgr.set_config_hook(Box::new(move |cfg, _| cfg.settings = settings.lock().clone()));
    }
    {
        let disabled = disabled.clone();
        h.mgr.set_servers_hook(Box::new(move |_, defined| {
            defined.iter().filter(|c| !disabled.lock().contains(&c.name)).cloned().collect()
        }));
    }
    h.open("a.mk", "x");
    h.wait_status("mock", ServerStatus::Ready).await;
    h.wait_status("extra", ServerStatus::Ready).await;

    // Only `settings` changed: pushed to the running server, which keeps running.
    let s = h.launcher.state("mock");
    *settings.lock() = Some(json!({ "nested": { "other": 2 } }));
    h.mgr.reconfigure().await;
    eventually(|| !s.notified("workspace/didChangeConfiguration").is_empty()).await;
    assert_eq!(
        s.notified("workspace/didChangeConfiguration")[0],
        json!({ "settings": { "flag": true, "nested": { "deep": 1, "other": 2 } } })
    );
    assert_eq!(s.launches.load(Ordering::SeqCst), 1);
    let cfg: Value = s
        .peer()
        .request("workspace/configuration", json!({ "items": [{ "section": "nested.other" }] }), None)
        .await
        .unwrap();
    assert_eq!(cfg, json!([2]));

    // Disabled: stopped and forgotten; enabled again: attached to the open document.
    disabled.lock().push("extra".into());
    h.mgr.reconfigure().await;
    assert_eq!(h.mgr.servers().iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["mock"]);
    assert_eq!(h.statuses("extra").last(), Some(&ServerStatus::Stopped));
    assert!(h.removed("extra"));
    assert!(!h.removed("mock"));
    disabled.lock().clear();
    h.mgr.reconfigure().await;
    h.wait_status("extra", ServerStatus::Ready).await;
    eventually(|| h.launcher.state("extra").notified("textDocument/didOpen").len() == 2).await;
    assert_eq!(s.launches.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn fallback_servers_only_run_when_the_preferred_one_is_missing() {
    // Preferred installed: the alternative is never started.
    let h = harness_with_fallbacks(&[("main", incremental_caps()), ("alt", incremental_caps())], &["alt"]);
    let (doc, _) = h.open("a.mk", "x");
    h.wait_status("main", ServerStatus::Ready).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.statuses("alt").is_empty());
    assert!(h.mgr.hover(doc, Pos::new(0, 0), Default::default()).await.is_ok());

    // Preferred missing: open documents move to the alternative.
    let h = harness_with_fallbacks(&[("ghost", json!({})), ("alt", incremental_caps())], &["alt"]);
    h.launcher.state("ghost").missing.store(true, Ordering::SeqCst);
    let (doc, path) = h.open("a.mk", "x");
    h.wait_status("ghost", ServerStatus::Missing).await;
    h.wait_status("alt", ServerStatus::Ready).await;
    let uri = uri_of(&path);
    eventually(|| h.launcher.state("alt").docs.lock().contains_key(&uri)).await;
    let (_, other) = h.open("b.mk", "y");
    let other = uri_of(&other);
    eventually(|| h.launcher.state("alt").docs.lock().contains_key(&other)).await;
    assert!(h.mgr.hover(doc, Pos::new(0, 0), Default::default()).await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fallback_leaves_when_the_preferred_server_is_enabled_again() {
    let h = harness_with_fallbacks(&[("main", incremental_caps()), ("alt", incremental_caps())], &["alt"]);
    let disabled: Arc<Mutex<Vec<String>>> = Default::default();
    {
        let disabled = disabled.clone();
        h.mgr.set_servers_hook(Box::new(move |_, defined| {
            defined.iter().filter(|c| !disabled.lock().contains(&c.name)).cloned().collect()
        }));
    }
    let (doc, path) = h.open("a.mk", "x");
    let uri = uri_of(&path);
    h.wait_status("main", ServerStatus::Ready).await;
    assert!(h.statuses("alt").is_empty());

    // Preferred disabled: the fallback takes over the open document.
    disabled.lock().push("main".into());
    h.mgr.reconfigure().await;
    h.wait_status("alt", ServerStatus::Ready).await;
    eventually(|| h.launcher.state("alt").docs.lock().contains_key(&uri)).await;
    assert_eq!(h.mgr.servers().iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["alt"]);
    assert!(h.removed("main"));

    // Enabled again: the fallback closes the document and stops.
    disabled.lock().clear();
    h.mgr.reconfigure().await;
    h.wait_status("main", ServerStatus::Ready).await;
    eventually(|| h.launcher.state("main").docs.lock().contains_key(&uri)).await;
    eventually(|| h.removed("alt")).await;
    assert!(!h.launcher.state("alt").notified("textDocument/didClose").is_empty());
    assert_eq!(h.statuses("alt").last(), Some(&ServerStatus::Stopped));
    assert_eq!(h.mgr.servers().iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["main"]);
    assert!(h.mgr.hover(doc, Pos::new(0, 0), Default::default()).await.is_ok());
    assert_eq!(h.launcher.state("alt").requested("textDocument/hover").len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn implementations_per_position() {
    let caps = json!({ "textDocumentSync": 1, "implementationProvider": true });
    let h = harness(&[("mock", caps)]);
    let (doc, path) = h.open("a.mk", "iface A {}\nimpl B {}\nimpl C {}\n");
    h.wait_status("mock", ServerStatus::Ready).await;
    let s = h.launcher.state("mock");
    let uri = uri_of(&path);
    *s.responder.lock() = Some(Arc::new(move |_, method, params| {
        if method != "textDocument/implementation" {
            return None;
        }
        // Line 0 is implemented on lines 1 and 2; nothing else is.
        Some(match params["position"]["line"].as_u64() {
            Some(0) => json!([
                { "uri": uri, "range": {"start":{"line":1,"character":5},"end":{"line":1,"character":6}} },
                { "uri": uri, "range": {"start":{"line":2,"character":5},"end":{"line":2,"character":6}} },
            ]),
            _ => json!([]),
        })
    }));
    let found = h.mgr.implementations(doc, &[Pos::new(0, 6), Pos::new(1, 5)], Default::default()).await.unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].iter().map(|l| l.range.start.line).collect::<Vec<_>>(), vec![1, 2]);
    assert!(found[1].is_empty());
    assert!(h.mgr.implementations(doc, &[], Default::default()).await.unwrap().is_empty());

    let cancel = ide_core::CancellationToken::new();
    cancel.cancel();
    let err = h.mgr.implementations(doc, &[Pos::new(0, 6)], cancel).await.unwrap_err();
    assert_eq!(err.kind, ide_core::error::ErrorKind::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn navigation_rename_format_and_decorations() {
    let h = harness(&[("mock", incremental_caps())]);
    let (doc, path) = h.open("a.mk", "fn foo() {}\nfoo();\n");
    h.wait_status("mock", ServerStatus::Ready).await;
    let s = h.launcher.state("mock");
    let uri = uri_of(&path);
    let r = |l0: u32, c0: u32, l1: u32, c1: u32| json!({"start":{"line":l0,"character":c0},"end":{"line":l1,"character":c1}});

    s.respond(
        "textDocument/definition",
        json!([{ "targetUri": uri, "targetRange": r(0,0,0,11), "targetSelectionRange": r(0,3,0,6) }]),
    );
    let defs = h.mgr.goto(doc, Pos::new(1, 1), GotoKind::Definition).await.unwrap();
    assert_eq!(defs, vec![ide_core::Location::file(path.clone(), rng(0, 3, 0, 6))]);
    let err = h.mgr.goto(doc, Pos::new(1, 1), GotoKind::TypeDefinition).await.unwrap_err();
    assert_eq!(err.kind, ide_core::error::ErrorKind::Unavailable);

    s.respond("textDocument/references", json!([{ "uri": uri, "range": r(0,3,0,6) }, { "uri": uri, "range": r(1,0,1,3) }, { "uri": uri, "range": r(1,0,1,3) }]));
    let refs = h.mgr.references(doc, Pos::new(1, 1), true, true, Default::default()).await.unwrap().locations;
    assert_eq!(refs.len(), 2);
    assert_eq!(s.requested("textDocument/references")[0]["context"]["includeDeclaration"], true);

    s.respond("textDocument/prepareRename", json!({ "range": r(1,0,1,3), "placeholder": "foo" }));
    assert_eq!(h.mgr.prepare_rename(doc, Pos::new(1, 1)).await.unwrap(), Some(rng(1, 0, 1, 3)));
    s.respond("textDocument/rename", json!({ "changes": { uri.clone(): [{ "range": r(0,3,0,6), "newText": "bar" }, { "range": r(1,0,1,3), "newText": "bar" }] } }));
    let edit = h.mgr.rename(doc, Pos::new(1, 1), "bar".into()).await.unwrap();
    assert!(matches!(&edit.files[0], FileEdit::Edit { edits, .. } if edits.len() == 2));
    assert_eq!(s.requested("textDocument/rename")[0]["newName"], "bar");

    s.respond("textDocument/formatting", json!([{ "range": r(0,10,0,11), "newText": "\n}" }]));
    let edits = h.mgr.format(doc, None, FormatOptions { tab_size: 2, insert_spaces: true }).await.unwrap();
    assert_eq!(edits[0].range, rng(0, 10, 0, 11));
    assert_eq!(s.requested("textDocument/formatting")[0]["options"], json!({ "tabSize": 2, "insertSpaces": true }));
    assert!(
        h.mgr.format(doc, Some(rng(0, 0, 1, 0)), FormatOptions { tab_size: 2, insert_spaces: true }).await.is_err()
    );

    s.respond("textDocument/semanticTokens/full", json!({ "data": [0, 3, 3, 0, 1, 1, 0, 3, 0, 0] }));
    let tokens = h.mgr.semantic_tokens(doc).await.unwrap().unwrap();
    assert_eq!(tokens.data, vec![0, 3, 3, 0, 1, 1, 0, 3, 0, 0]);
    assert_eq!(tokens.token_types, vec!["function", "variable"]);

    s.respond("textDocument/codeLens", json!([{ "range": r(0,0,0,2), "data": 1 }, { "range": r(1,0,1,2), "command": { "title": "Run", "command": "mock.run" } }]));
    s.respond(
        "codeLens/resolve",
        json!({ "range": r(0,0,0,2), "command": { "title": "1 reference", "command": "mock.refs" } }),
    );
    let lenses = h.mgr.code_lens(doc).await.unwrap();
    assert_eq!(lenses.len(), 2);
    assert_eq!(lenses[0].command.as_ref().unwrap().title, "1 reference");
    assert_eq!(s.requested("codeLens/resolve").len(), 1);

    s.respond(
        "textDocument/inlayHint",
        json!([{ "position": {"line":1,"character":5}, "label": [{"value": "x:"}], "kind": 2, "paddingRight": true }]),
    );
    let hints = h.mgr.inlay_hints(doc, rng(0, 0, 2, 0)).await.unwrap();
    assert_eq!(hints[0].label, "x:");
    assert_eq!(hints[0].kind, InlayKind::Parameter);

    // Nothing to prepare at the position: NotFound, so the UI can retry at the enclosing declaration.
    s.respond("textDocument/prepareCallHierarchy", json!(null));
    let err = h.mgr.call_hierarchy(doc, Pos::new(0, 1), HierarchyDirection::Incoming).await.unwrap_err();
    assert_eq!(err.kind, ide_core::error::ErrorKind::NotFound);

    let item = json!({ "name": "foo", "kind": 12, "uri": uri, "range": r(0,0,0,11), "selectionRange": r(0,3,0,6), "data": "id" });
    s.respond("textDocument/prepareCallHierarchy", json!([item]));
    s.respond("callHierarchy/incomingCalls", json!([{ "from": { "name": "main", "kind": 12, "uri": uri, "range": r(1,0,1,6), "selectionRange": r(1,0,1,3) }, "fromRanges": [r(1,0,1,3)] }]));
    let calls = h.mgr.call_hierarchy(doc, Pos::new(0, 4), HierarchyDirection::Incoming).await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "main");
    assert_eq!(calls[0].from_ranges, vec![rng(1, 0, 1, 3)]);
    assert_eq!(s.requested("callHierarchy/incomingCalls")[0]["item"]["data"], "id");

    // No documentSymbolProvider → None so callers fall back to tree-sitter.
    assert_eq!(h.mgr.document_symbols(doc).await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn utf8_server_positions_are_converted() {
    let h = harness(&[(
        "u8",
        json!({ "positionEncoding": "utf-8", "textDocumentSync": 2, "hoverProvider": true, "definitionProvider": true }),
    )]);
    let (doc, path) = h.open("a.mk", "é😀x = é😀x\n");
    h.wait_status("u8", ServerStatus::Ready).await;
    let s = h.launcher.state("u8");
    let uri = uri_of(&path);
    s.respond(
        "textDocument/definition",
        json!([{ "uri": uri, "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":7}} }]),
    );
    // UTF-16 col 3 ("x" after "é😀") is byte 6.
    let defs = h.mgr.goto(doc, Pos::new(0, 3), GotoKind::Definition).await.unwrap();
    assert_eq!(s.requested("textDocument/definition")[0]["position"], json!({ "line": 0, "character": 6 }));
    assert_eq!(defs[0].range, rng(0, 0, 0, 4));
    // didChange ranges are converted too.
    h.mgr.did_change(doc, 2, &[change(rng(0, 4, 0, 4), "y")], &|| unreachable!());
    eventually(|| !s.notified("textDocument/didChange").is_empty()).await;
    assert_eq!(s.notified("textDocument/didChange")[0]["contentChanges"][0]["range"]["start"]["character"], 7);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_is_propagated() {
    let h = harness(&[("mock", incremental_caps())]);
    let (doc, _) = h.open("a.mk", "x");
    h.wait_status("mock", ServerStatus::Ready).await;
    let token = ide_core::CancellationToken::new();
    token.cancel();
    let err = h.mgr.hover(doc, Pos::new(0, 0), token).await.unwrap_err();
    assert_eq!(err.kind, ide_core::error::ErrorKind::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn code_action_query_reports_advertised_kinds_and_cancels() {
    let mut caps = incremental_caps();
    caps["codeActionProvider"] = json!({ "codeActionKinds": ["quickfix", "refactor.extract", "refactor.inline"] });
    let h = harness(&[("mock", caps)]);
    let (doc, _) = h.open("a.mk", "let x = 1;\n");
    h.wait_status("mock", ServerStatus::Ready).await;
    let s = h.launcher.state("mock");
    s.respond(
        "textDocument/codeAction",
        json!([{ "title": "Extract into variable", "kind": "refactor.extract.variable" }]),
    );

    let q = h
        .mgr
        .code_action_query(doc, rng(0, 8, 0, 9), vec!["refactor".into()], ide_core::CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(q.actions.len(), 1);
    assert_eq!(q.actions[0].kind.as_deref(), Some("refactor.extract.variable"));
    assert_eq!(q.advertised_kinds, Some(vec!["quickfix".into(), "refactor.extract".into(), "refactor.inline".into()]));
    assert_eq!(s.requested("textDocument/codeAction")[0]["context"]["only"], json!(["refactor"]));

    let token = ide_core::CancellationToken::new();
    token.cancel();
    let err = h.mgr.code_action_query(doc, rng(0, 8, 0, 9), vec![], token).await.unwrap_err();
    assert_eq!(err.kind, ide_core::error::ErrorKind::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn code_action_query_without_listed_kinds() {
    let h = harness(&[("mock", incremental_caps())]);
    let (doc, _) = h.open("a.mk", "x");
    h.wait_status("mock", ServerStatus::Ready).await;
    let q = h.mgr.code_action_query(doc, rng(0, 0, 0, 0), vec![], ide_core::CancellationToken::new()).await.unwrap();
    assert!(q.actions.is_empty());
    assert_eq!(q.advertised_kinds, None, "`codeActionProvider` without `codeActionKinds` may return any kind");
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_all_stops_gracefully() {
    let h = harness(&[("mock", incremental_caps())]);
    h.open("a.mk", "x");
    h.wait_status("mock", ServerStatus::Ready).await;
    let s = h.launcher.state("mock");
    tokio::time::timeout(Duration::from_secs(5), h.mgr.shutdown_all()).await.unwrap();
    assert_eq!(s.requested("shutdown").len(), 1);
    assert_eq!(s.notified("exit").len(), 1);
    assert_eq!(h.statuses("mock").last(), Some(&ServerStatus::Stopped));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.launches.load(Ordering::SeqCst), 1, "no restart after shutdown");
    // Opening more documents after shutdown does not start servers.
    h.open("b.mk", "y");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.launches.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn noop_handler_is_usable_for_raw_peers() {
    // Sanity check that the public jsonrpc API composes with the launcher seam.
    let (a, b) = tokio::io::duplex(1024);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let _server = Peer::connect(br, bw, Arc::new(MockHandler(Arc::default())));
    let client = Peer::connect(ar, aw, Arc::new(NoopHandler));
    let v: Value = client.request("shutdown", (), None).await.unwrap();
    assert_eq!(v, Value::Null);
}

// ------------------------------------------------------------------ virtual documents

#[tokio::test(flavor = "multi_thread")]
async fn virtual_locations_and_text_document_content() {
    let mut caps = incremental_caps();
    caps["typeHierarchyProvider"] = json!(true);
    caps["workspace"] = json!({ "textDocumentContent": { "schemes": ["mocklib"] } });
    let h = harness(&[("mock", caps)]);
    let (doc, path) = h.open("a.mk", "use Thing;\n");
    h.wait_status("mock", ServerStatus::Ready).await;
    let s = h.launcher.state("mock");
    let r = |l0: u32, c0: u32, l1: u32, c1: u32| json!({"start":{"line":l0,"character":c0},"end":{"line":l1,"character":c1}});
    let lib = "mocklib://deps/lib-1.0/Thing.mk";

    // Non-file targets are kept (as virtual locations) instead of dropped.
    s.respond("textDocument/definition", json!({ "uri": lib, "range": r(2, 4, 2, 9) }));
    let defs = h.mgr.goto(doc, Pos::new(0, 5), GotoKind::Definition).await.unwrap();
    assert_eq!(defs, vec![ide_core::Location { path: lib.into(), range: rng(2, 4, 2, 9), uri: Some(lib.to_owned()) }]);
    s.respond(
        "textDocument/references",
        json!([{ "uri": uri_of(&path), "range": r(0, 4, 0, 9) }, { "uri": lib, "range": r(2, 4, 2, 9) }]),
    );
    let refs = h.mgr.references(doc, Pos::new(0, 5), true, true, Default::default()).await.unwrap().locations;
    assert_eq!(refs.iter().map(|l| l.uri.is_some()).collect::<Vec<_>>(), vec![false, true]);
    s.respond(
        "textDocument/prepareTypeHierarchy",
        json!([{ "name": "Thing", "kind": 5, "uri": uri_of(&path), "range": r(0,0,0,1), "selectionRange": r(0,0,0,1) }]),
    );
    s.respond(
        "typeHierarchy/supertypes",
        json!([{ "name": "Base", "kind": 5, "uri": lib, "range": r(1,0,3,1), "selectionRange": r(1,6,1,10) }]),
    );
    let supers = h.mgr.type_hierarchy(doc, Pos::new(0, 5), HierarchyDirection::Incoming).await.unwrap();
    assert_eq!(supers[0].uri.as_deref(), Some(lib));
    assert_eq!(supers[0].selection_range, rng(1, 6, 1, 10));

    // Contents come from `workspace/textDocumentContent` and are cached.
    s.respond("workspace/textDocumentContent", json!({ "text": "// lib\npub struct Base;\npub struct Thing;\n" }));
    let vd = h.mgr.virtual_document(lib).await.unwrap();
    assert_eq!(vd.title, "Thing.mk");
    assert_eq!(vd.language_id.as_str(), "mock");
    assert!(!vd.decompiled);
    assert_eq!(h.mgr.virtual_document(lib).await.unwrap(), vd);
    let asked = s.requested("workspace/textDocumentContent");
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["uri"], lib);

    // Opened read-only on the owning server: didOpen with the URI, requests
    // inside it use it, closing sends didClose; never didChange.
    let vdoc = DocId::next();
    h.mgr.did_open_virtual(vdoc, lib, &vd.language_id, 0, &vd.text);
    eventually(|| s.docs.lock().contains_key(lib)).await;
    s.respond("textDocument/hover", json!({ "contents": { "kind": "markdown", "value": "struct Thing" } }));
    let hover = h.mgr.hover(vdoc, Pos::new(2, 12), Default::default()).await.unwrap().unwrap();
    assert_eq!(hover.contents.value, "struct Thing");
    assert_eq!(s.requested("textDocument/hover").last().unwrap()["textDocument"]["uri"], lib);
    h.mgr.did_close(vdoc);
    eventually(|| !s.docs.lock().contains_key(lib)).await;
    assert!(s.notified("textDocument/didChange").iter().all(|p| p["textDocument"]["uri"] != lib));

    // Unknown schemes nobody serves fail cleanly.
    assert!(h.mgr.virtual_document("other://x/y.mk").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn jdt_class_file_contents() {
    let h = harness_for("java", "java", &[("jdtls", incremental_caps())], &[]);
    let (doc, _) = h.open("App.java", "import org.example.Lib;\n");
    h.wait_status("jdtls", ServerStatus::Ready).await;
    let s = h.launcher.state("jdtls");
    let jdt = "jdt://contents/lib-1.0.jar/org.example/Lib.class?=demo/%5C/m2%5C/lib-1.0.jar%3Corg.example%28Lib.class";
    s.respond(
        "textDocument/definition",
        json!([{ "uri": jdt, "range": { "start": { "line": 3, "character": 13 }, "end": { "line": 3, "character": 16 } } }]),
    );
    let defs = h.mgr.goto(doc, Pos::new(0, 20), GotoKind::Definition).await.unwrap();
    assert_eq!(defs[0].uri.as_deref(), Some(jdt));

    s.respond(
        "java/classFileContents",
        json!("// Source code is unavailable, and was generated by the Fernflower decompiler.\npackage org.example;\n\npublic class Lib {}\n"),
    );
    let vd = h.mgr.virtual_document(jdt).await.unwrap();
    assert_eq!(s.requested("java/classFileContents")[0], json!({ "uri": jdt }));
    assert!(vd.decompiled);
    assert_eq!(vd.title, "Lib.class");
    assert_eq!(vd.language_id.as_str(), "java");
    assert_eq!(vd.description.as_deref(), Some("/m2/lib-1.0.jar"));
}

#[tokio::test(flavor = "multi_thread")]
async fn will_and_did_rename_files_honour_filters() {
    let mut caps = incremental_caps();
    caps["workspace"] = json!({ "fileOperations": {
        "willRename": { "filters": [
            { "scheme": "file", "pattern": { "glob": "**/*.mk", "matches": "file" } },
            { "scheme": "file", "pattern": { "glob": "**", "matches": "folder" } }
        ] },
        "didRename": { "filters": [{ "pattern": { "glob": "**/*.mk" } }] }
    }});
    let h = harness(&[("mock", caps)]);
    std::fs::create_dir_all(h.root.join("pkg")).unwrap();
    let util = h.root.join("pkg/util.mk");
    let main = h.root.join("main.mk");
    std::fs::write(&util, "fn util() {}\n").unwrap();
    std::fs::write(&main, "use pkg/util;\n").unwrap();
    let s = h.launcher.state("mock");
    let r = |l0: u32, c0: u32, l1: u32, c1: u32| json!({"start":{"line":l0,"character":c0},"end":{"line":l1,"character":c1}});
    s.respond(
        "workspace/willRenameFiles",
        json!({ "changes": { uri_of(&main): [{ "range": r(0,4,0,7), "newText": "lib" }] } }),
    );

    // Nothing open yet: the server is started for the moved file's language,
    // and the file is opened just for the request (tsserver needs a project).
    let to = h.root.join("lib/util.mk");
    let result =
        h.mgr.will_rename_files(&[(util.clone(), to.clone())], ide_core::CancellationToken::new()).await.unwrap();
    assert_eq!(result.servers, vec!["mock".to_owned()]);
    assert!(result.errors.is_empty());
    match &result.edit.files[..] {
        [FileEdit::Edit { path, edits }] => {
            assert_eq!(path, &main);
            assert_eq!(edits[0].range, rng(0, 4, 0, 7));
            assert_eq!(edits[0].new_text, "lib");
        }
        other => panic!("unexpected edit {other:?}"),
    }
    let req = &s.requested("workspace/willRenameFiles")[0];
    assert_eq!(req["files"], json!([{ "oldUri": uri_of(&util), "newUri": uri_of(&to) }]));
    assert_eq!(s.notified("textDocument/didOpen")[0]["textDocument"]["uri"], uri_of(&util));
    eventually(|| !s.notified("textDocument/didClose").is_empty()).await;
    assert_eq!(s.notified("textDocument/didClose")[0]["textDocument"]["uri"], uri_of(&util));

    // Folders match the folder filter; files of other types match nothing.
    let dir_to = h.root.join("lib");
    h.mgr.will_rename_files(&[(h.root.join("pkg"), dir_to.clone())], ide_core::CancellationToken::new()).await.unwrap();
    assert_eq!(s.requested("workspace/willRenameFiles")[1]["files"][0]["newUri"], uri_of(&dir_to));
    let txt = h.root.join("notes.txt");
    std::fs::write(&txt, "x").unwrap();
    let none = h
        .mgr
        .will_rename_files(&[(txt.clone(), h.root.join("n.txt"))], ide_core::CancellationToken::new())
        .await
        .unwrap();
    assert!(none.servers.is_empty() && none.edit.files.is_empty());
    assert_eq!(s.requested("workspace/willRenameFiles").len(), 2);

    // didRenameFiles after the move, filtered the same way.
    std::fs::create_dir_all(h.root.join("lib")).unwrap();
    std::fs::rename(&util, &to).unwrap();
    std::fs::rename(&txt, h.root.join("n.txt")).unwrap();
    h.mgr.did_rename_files(&[(util.clone(), to.clone()), (txt, h.root.join("n.txt"))]);
    eventually(|| !s.notified("workspace/didRenameFiles").is_empty()).await;
    assert_eq!(
        s.notified("workspace/didRenameFiles")[0]["files"],
        json!([{ "oldUri": uri_of(&util), "newUri": uri_of(&to) }])
    );

    // Servers without fileOperations are never asked.
    let plain = harness(&[("plain", incremental_caps())]);
    let f = plain.root.join("a.mk");
    std::fs::write(&f, "x").unwrap();
    let r = plain
        .mgr
        .will_rename_files(&[(f.clone(), plain.root.join("b.mk"))], ide_core::CancellationToken::new())
        .await
        .unwrap();
    assert!(r.servers.is_empty());
    assert!(plain.launcher.state("plain").requested("workspace/willRenameFiles").is_empty());
    assert_eq!(
        s.requested("initialize")[0]["capabilities"]["workspace"]["fileOperations"],
        json!({ "dynamicRegistration": true, "willRename": true, "didRename": true })
    );
}

// ------------------------------------------------------------------ inspect code

struct InspectTestHost {
    mgr: Arc<LspManager>,
    lang: LanguageId,
}

impl ide_lsp::InspectHost for InspectTestHost {
    fn open(&self, path: &Path) -> ide_core::IdeResult<(DocId, bool)> {
        let text = std::fs::read_to_string(path).map_err(|e| ide_core::IdeError::internal(e.to_string()))?;
        let doc = DocId::next();
        self.mgr.did_open(doc, path, &self.lang, 1, &text);
        Ok((doc, false))
    }
    fn close(&self, doc: DocId) {
        self.mgr.did_close(doc);
    }
}

fn inspect_opts() -> ide_lsp::InspectOptions {
    ide_lsp::InspectOptions { concurrency: 2, timeout: Duration::from_secs(5), settle: Duration::from_millis(50) }
}

async fn inspect(h: &Harness, lang: &str, files: Vec<PathBuf>) -> Vec<(PathBuf, ide_lsp::FileInspection)> {
    let host = Arc::new(InspectTestHost { mgr: h.mgr.clone(), lang: LanguageId::new(lang) });
    let out = Mutex::new(Vec::new());
    h.mgr
        .inspect_files(host, files, inspect_opts(), ide_core::CancellationToken::new(), &|p, r| out.lock().push((p, r)))
        .await;
    let mut out = out.into_inner();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn inspect_waits_for_published_diagnostics_and_closes_files() {
    let h = harness(&[("mock", incremental_caps())]);
    let a = h.root.join("a.mk");
    let b = h.root.join("b.mk");
    std::fs::write(&a, "let unused = 1;\n").unwrap();
    std::fs::write(&b, "ok\n").unwrap();
    let s = h.launcher.state("mock");
    // The "server" publishes for every file it gets opened: one problem in a.mk, none in b.mk.
    let publisher = {
        let s = s.clone();
        let (a, b) = (a.clone(), b.clone());
        tokio::spawn(async move {
            let mut done = Vec::new();
            for _ in 0..500 {
                for open in s.notified("textDocument/didOpen") {
                    let uri = open["textDocument"]["uri"].as_str().unwrap().to_owned();
                    if done.contains(&uri) {
                        continue;
                    }
                    let diags = if uri == uri_of(&a) {
                        json!([{ "range": {"start":{"line":0,"character":4},"end":{"line":0,"character":10}},
                                 "severity": 2, "message": "unused variable" }])
                    } else {
                        assert_eq!(uri, uri_of(&b));
                        json!([])
                    };
                    s.peer()
                        .notify("textDocument/publishDiagnostics", json!({ "uri": uri, "diagnostics": diags }))
                        .unwrap();
                    done.push(uri);
                }
                if done.len() == 2 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };
    let results = inspect(&h, "mock", vec![a.clone(), b.clone()]).await;
    publisher.await.unwrap();
    assert_eq!(results.len(), 2);
    match &results[0].1 {
        ide_lsp::FileInspection::Inspected(items) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].message, "unused variable");
            assert_eq!(items[0].severity, Severity::Warning);
            assert_eq!(items[0].source.as_deref(), Some("mock"));
            assert_eq!(items[0].range, rng(0, 4, 0, 10));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(&results[1].1, ide_lsp::FileInspection::Inspected(items) if items.is_empty()));
    // Transient documents are closed again.
    eventually(|| s.notified("textDocument/didClose").len() == 2).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn inspect_pulls_diagnostics_when_the_server_supports_it() {
    let mut caps = incremental_caps();
    caps["diagnosticProvider"] = json!({ "interFileDependencies": false, "workspaceDiagnostics": false });
    let h = harness(&[("mock", caps)]);
    let a = h.root.join("a.mk");
    std::fs::write(&a, "x\n").unwrap();
    let s = h.launcher.state("mock");
    s.respond(
        "textDocument/diagnostic",
        json!({ "kind": "full", "items": [{ "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                                           "severity": 1, "message": "bad x", "source": "mockc" }] }),
    );
    let results = inspect(&h, "mock", vec![a.clone()]).await;
    match &results[0].1 {
        ide_lsp::FileInspection::Inspected(items) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].severity, Severity::Error);
            assert_eq!(items[0].source.as_deref(), Some("mockc"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(s.requested("textDocument/diagnostic")[0]["textDocument"]["uri"], json!(uri_of(&a)));
}

#[tokio::test(flavor = "multi_thread")]
async fn inspect_reports_files_without_a_server() {
    let h = harness(&[("mock", incremental_caps())]);
    let txt = h.root.join("notes.txt");
    std::fs::write(&txt, "hello\n").unwrap();
    assert!(!h.mgr.serves_path(&txt));
    assert!(h.mgr.serves_path(&h.root.join("a.mk")));
    let results = inspect(&h, "plaintext", vec![txt]).await;
    assert!(matches!(results[0].1, ide_lsp::FileInspection::NoServer));
}

#[tokio::test(flavor = "multi_thread")]
async fn css_family_configuration_returns_default_objects() {
    for language in ["css", "scss", "less"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let registry = Arc::new(LanguageRegistry::load(None).unwrap());
        let launcher = Arc::new(MockLauncher::default());
        let state = launcher.state("vscode-css-language-server");
        *state.caps.lock() = incremental_caps();
        let mgr = LspManager::with_launcher(root.clone(), registry, Arc::new(|_| {}), launcher);
        let path = root.join(format!("a.{language}"));
        let text = ".qa { colr: red; }";
        std::fs::write(&path, text).unwrap();
        mgr.did_open(DocId::next(), &path, &LanguageId::new(language), 1, text);
        eventually(|| !state.notified("textDocument/didOpen").is_empty()).await;
        let reply: Value = state
            .peer()
            .request(
                "workspace/configuration",
                json!({ "items": [
            { "section": "css", "scopeUri": uri_of(&path) },
            { "section": "scss", "scopeUri": uri_of(&path) },
            { "section": "less", "scopeUri": uri_of(&path) },
            { "section": "unknown" }
        ] }),
                None,
            )
            .await
            .unwrap();
        assert_eq!(reply, json!([{}, {}, {}, null]), "{language}");
        assert_eq!(state.requested("initialize")[0]["initializationOptions"]["provideFormatter"], true);
    }
}

// ------------------------------------------------------------------ references beyond the analysis scope

const WORD: &str = "target";

/// Locations of [`WORD`] in `text` (ASCII, so UTF-16 columns are byte offsets).
fn occurrences(uri: &str, text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for (line, l) in text.lines().enumerate() {
        for (col, _) in l.match_indices(WORD) {
            let end = col + WORD.len();
            out.push(json!({ "uri": uri, "range": {
                "start": { "line": line, "character": col }, "end": { "line": line, "character": end } } }));
        }
    }
    out
}

/// A server that only analyses its open documents (minus `blind` URIs), like
/// Pyright for files outside its `include`.
fn open_files_only(blind: Vec<String>) -> Responder {
    Arc::new(move |s, method, _| {
        let docs: Vec<(String, String)> =
            s.docs.lock().iter().filter(|(u, _)| !blind.contains(u)).map(|(u, t)| (u.clone(), t.clone())).collect();
        let locs = || docs.iter().flat_map(|(u, t)| occurrences(u, t));
        match method {
            "textDocument/references" => Some(Value::Array(locs().collect())),
            "textDocument/rename" => {
                let mut changes = serde_json::Map::new();
                for loc in locs() {
                    let edits = changes.entry(loc["uri"].as_str().unwrap()).or_insert_with(|| json!([]));
                    edits.as_array_mut().unwrap().push(json!({ "range": loc["range"], "newText": "renamed" }));
                }
                Some(json!({ "changes": changes }))
            }
            _ => None,
        }
    })
}

/// Candidate files: those directly under the root that contain the word; records the queries.
fn grep_candidates(h: &Harness) -> Arc<Mutex<Vec<ide_lsp::CandidateQuery>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    h.mgr.set_candidates_hook(Box::new(move |q, _| {
        log.lock().push(q.clone());
        let mut files: Vec<PathBuf> = std::fs::read_dir(&q.root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains(&q.word)))
            .collect();
        files.sort();
        ide_lsp::CandidateFiles { files, limit_hit: false }
    }));
    seen
}

fn uris(params: &[Value]) -> Vec<String> {
    params.iter().map(|p| p["textDocument"]["uri"].as_str().unwrap().to_owned()).collect()
}

fn diagnostics_for(h: &Harness, path: &Path) -> usize {
    h.events.lock().iter().filter(|e| matches!(e, IdeEvent::Diagnostics { path: p, .. } if p == path)).count()
}

fn scope_harness() -> (Harness, Arc<MockState>) {
    let h = harness(&[("mock", incremental_caps())]);
    let s = h.launcher.state("mock");
    *s.responder.lock() = Some(open_files_only(vec![]));
    (h, s)
}

#[tokio::test(flavor = "multi_thread")]
async fn references_open_unopened_files_that_mention_the_name() {
    let (h, s) = scope_harness();
    let seen = grep_candidates(&h);
    let b = h.root.join("b.mk");
    let c = h.root.join("c.mk");
    std::fs::write(&b, "call target\n").unwrap();
    std::fs::write(&c, "nothing here\n").unwrap();
    let (doc, a) = h.open("a.mk", "def target\n");
    let (_, d) = h.open("d.mk", "use target\n");
    h.wait_status("mock", ServerStatus::Ready).await;
    eventually(|| s.docs.lock().len() == 2).await;

    let refs = h.mgr.references(doc, Pos::new(0, 5), true, true, Default::default()).await.unwrap();
    let mut paths: Vec<PathBuf> = refs.locations.iter().map(|l| l.path.to_path_buf()).collect();
    paths.sort();
    assert_eq!(paths, vec![a.clone(), b.clone(), d.clone()]);
    assert_eq!(refs.incomplete, None);

    let q = seen.lock()[0].clone();
    assert_eq!((q.word.as_str(), q.root.as_path(), q.limit), (WORD, h.root.as_path(), ide_lsp::MAX_CANDIDATES));
    assert_eq!(q.languages, vec![h.lang.clone()]);

    // Only the unopened candidate was opened (from disk), then closed again.
    let opens = s.notified("textDocument/didOpen");
    let temp: Vec<&Value> = opens.iter().filter(|p| p["textDocument"]["uri"] == uri_of(&b)).collect();
    assert_eq!(temp.len(), 1);
    assert_eq!(temp[0]["textDocument"]["version"], 0);
    assert_eq!(temp[0]["textDocument"]["text"], "call target\n");
    assert!(!uris(&opens).contains(&uri_of(&c)));
    assert_eq!(uris(&opens).iter().filter(|u| **u == uri_of(&d)).count(), 1);
    eventually(|| uris(&s.notified("textDocument/didClose")) == vec![uri_of(&b)]).await;
    assert_eq!(s.requested("textDocument/references").len(), 2);
    assert_eq!(s.docs.lock().len(), 2);

    // When every candidate file is already in the answer, no second request is made.
    std::fs::remove_file(&b).unwrap();
    h.mgr.references(doc, Pos::new(0, 5), true, true, Default::default()).await.unwrap();
    assert_eq!(s.requested("textDocument/references").len(), 3);

    // Without project_wide only the server's scope counts: no project search.
    std::fs::write(&b, "call target\n").unwrap();
    let refs = h.mgr.references(doc, Pos::new(0, 5), true, false, Default::default()).await.unwrap();
    assert_eq!(refs.locations.len(), 2);
    assert_eq!(seen.lock().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn references_never_reopen_or_close_the_users_files() {
    let h = harness(&[("mock", incremental_caps())]);
    let s = h.launcher.state("mock");
    grep_candidates(&h);
    let (doc, _) = h.open("a.mk", "def target\n");
    // Open, mentions the name, but the server does not report it.
    let (_, d) = h.open("d.mk", "use target\n");
    *s.responder.lock() = Some(open_files_only(vec![uri_of(&d)]));
    h.wait_status("mock", ServerStatus::Ready).await;
    eventually(|| s.docs.lock().len() == 2).await;

    let refs = h.mgr.references(doc, Pos::new(0, 5), true, true, Default::default()).await.unwrap();
    assert_eq!(refs.locations.len(), 1);
    assert_eq!(uris(&s.notified("textDocument/didOpen")).iter().filter(|u| **u == uri_of(&d)).count(), 1);
    assert!(s.notified("textDocument/didClose").is_empty());
    assert_eq!(s.requested("textDocument/references").len(), 1);
    assert!(s.docs.lock().contains_key(&uri_of(&d)));
}

#[tokio::test(flavor = "multi_thread")]
async fn references_report_a_capped_candidate_search() {
    let (h, s) = scope_harness();
    h.mgr.set_candidates_hook(Box::new(|q, _| ide_lsp::CandidateFiles {
        files: vec![q.root.join("b.mk")],
        limit_hit: true,
    }));
    std::fs::write(h.root.join("b.mk"), "target\n").unwrap();
    let (doc, _) = h.open("a.mk", "def target\n");
    h.wait_status("mock", ServerStatus::Ready).await;
    eventually(|| s.docs.lock().len() == 1).await;
    let refs = h.mgr.references(doc, Pos::new(0, 5), true, true, Default::default()).await.unwrap();
    assert_eq!(refs.locations.len(), 2);
    assert_eq!(refs.incomplete, Some(ReferencesIncomplete::TooManyFiles));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_references_search_stops_the_candidate_search_and_closes_files() {
    let (h, s) = scope_harness();
    h.mgr.set_candidates_hook(Box::new(|_, cancel| {
        while !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(5));
        }
        ide_lsp::CandidateFiles::default()
    }));
    let (doc, _) = h.open("a.mk", "def target\n");
    h.wait_status("mock", ServerStatus::Ready).await;
    let cancel = ide_core::CancellationToken::new();
    let later = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        later.cancel();
    });
    let err = h.mgr.references(doc, Pos::new(0, 5), true, true, cancel).await.unwrap_err();
    assert_eq!(err.kind, ide_core::error::ErrorKind::Cancelled);

    // Cancelled while the server answers with a file opened for the search.
    let b = h.root.join("b.mk");
    std::fs::write(&b, "call target\n").unwrap();
    grep_candidates(&h);
    let slow = uri_of(&b);
    let inner = open_files_only(vec![]);
    let gate = Arc::new(Gate::default());
    let held = gate.clone();
    *s.responder.lock() = Some(Arc::new(move |s, method, params| {
        if s.docs.lock().contains_key(&slow) {
            held.hold();
        }
        inner(s, method, params)
    }));
    let cancel = ide_core::CancellationToken::new();
    let mgr = h.mgr.clone();
    let search = tokio::spawn({
        let cancel = cancel.clone();
        async move { mgr.references(doc, Pos::new(0, 5), true, true, cancel).await }
    });
    eventually(|| gate.entered()).await;
    cancel.cancel();
    // Returns while the server still holds the answer.
    let err = search.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ide_core::error::ErrorKind::Cancelled);
    eventually(|| uris(&s.notified("textDocument/didClose")) == vec![uri_of(&b)]).await;
    gate.open();
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostics_of_files_opened_for_a_search_stay_out_of_problems() {
    let h = harness(&[("mock", incremental_caps())]);
    let s = h.launcher.state("mock");
    grep_candidates(&h);
    let b = h.root.join("b.mk");
    std::fs::write(&b, "call target\n").unwrap();
    let (doc, a) = h.open("a.mk", "def target\n");
    let diag = json!([{ "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}}, "message": "e" }]);
    let inner = open_files_only(vec![]);
    let (temp, user, d) = (uri_of(&b), uri_of(&a), diag.clone());
    *s.responder.lock() = Some(Arc::new(move |s, method, params| {
        if s.docs.lock().contains_key(&temp) {
            for uri in [&temp, &user] {
                s.peer().notify("textDocument/publishDiagnostics", json!({ "uri": uri, "diagnostics": d })).unwrap();
            }
        }
        inner(s, method, params)
    }));
    h.wait_status("mock", ServerStatus::Ready).await;
    eventually(|| s.docs.lock().len() == 1).await;

    let refs = h.mgr.references(doc, Pos::new(0, 5), true, true, Default::default()).await.unwrap();
    assert_eq!(refs.locations.len(), 2);
    // Published right after the didClose (analysis still running): dropped too.
    s.peer().notify("textDocument/publishDiagnostics", json!({ "uri": uri_of(&b), "diagnostics": diag })).unwrap();
    s.peer().notify("textDocument/publishDiagnostics", json!({ "uri": uri_of(&a), "diagnostics": [] })).unwrap();
    eventually(|| diagnostics_for(&h, &a) == 2).await;
    assert_eq!(diagnostics_for(&h, &b), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn opening_a_file_during_a_search_takes_it_over() {
    let h = harness(&[("mock", incremental_caps())]);
    let s = h.launcher.state("mock");
    grep_candidates(&h);
    let b = h.root.join("b.mk");
    std::fs::write(&b, "call target\n").unwrap();
    let (doc, _) = h.open("a.mk", "def target\n");
    let slow = uri_of(&b);
    let inner = open_files_only(vec![]);
    let gate = Arc::new(Gate::default());
    let held = gate.clone();
    *s.responder.lock() = Some(Arc::new(move |s, method, params| {
        if s.docs.lock().get(&slow).is_some_and(|t| t.starts_with("call")) {
            held.hold();
        }
        inner(s, method, params)
    }));
    h.wait_status("mock", ServerStatus::Ready).await;
    eventually(|| s.docs.lock().len() == 1).await;

    let mgr = h.mgr.clone();
    let search = tokio::spawn(async move { mgr.references(doc, Pos::new(0, 5), true, true, Default::default()).await });
    // The server answers with the copy opened for the search.
    eventually(|| gate.entered()).await;
    let user_doc = DocId::next();
    h.mgr.did_open(user_doc, &b, &h.lang, 7, "edited target\n");
    eventually(|| s.docs.lock().get(&uri_of(&b)).map(String::as_str) == Some("edited target\n")).await;
    gate.open();
    search.await.unwrap().unwrap();

    // The temporary copy was closed before the user's didOpen, and the user's stays open.
    let events: Vec<(String, Value)> = s
        .notifications
        .lock()
        .iter()
        .filter(|(m, p)| m.starts_with("textDocument/did") && p["textDocument"]["uri"] == uri_of(&b))
        .map(|(m, p)| (m.clone(), p["textDocument"]["version"].clone()))
        .collect();
    assert_eq!(
        events,
        vec![
            ("textDocument/didOpen".to_owned(), json!(0)),
            ("textDocument/didClose".to_owned(), Value::Null),
            ("textDocument/didOpen".to_owned(), json!(7)),
        ]
    );
    assert_eq!(s.docs.lock().get(&uri_of(&b)).map(String::as_str), Some("edited target\n"));
    // Its diagnostics are the user's again.
    s.peer().notify("textDocument/publishDiagnostics", json!({ "uri": uri_of(&b), "diagnostics": [] })).unwrap();
    eventually(|| diagnostics_for(&h, &b) == 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_answer_while_indexing_is_retried_when_indexing_ends() {
    let h = harness(&[("mock", incremental_caps())]);
    let s = h.launcher.state("mock");
    let (doc, path) = h.open("a.mk", "def target\n");
    h.wait_status("mock", ServerStatus::Ready).await;
    let peer = s.peer();
    peer.notify("$/progress", json!({ "token": "idx", "value": { "kind": "begin", "title": "Indexing" } })).unwrap();
    h.wait_status("mock", ServerStatus::Busy).await;
    // Nothing found while indexing; indexing ends once that answer is out.
    s.respond("textDocument/references", Value::Array(occurrences(&uri_of(&path), "def target\n")));
    let answered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let first = answered.clone();
    *s.responder.lock() = Some(Arc::new(move |_, method, _| {
        (method == "textDocument/references" && !first.swap(true, Ordering::SeqCst)).then(|| json!([]))
    }));
    tokio::spawn(async move {
        eventually(|| answered.load(Ordering::SeqCst)).await;
        peer.notify("$/progress", json!({ "token": "idx", "value": { "kind": "end" } })).unwrap();
    });
    let refs = h.mgr.references(doc, Pos::new(0, 5), true, true, Default::default()).await.unwrap();
    assert_eq!(refs.locations.len(), 1);
    assert_eq!(refs.incomplete, None);
    assert_eq!(s.requested("textDocument/references").len(), 2);

    // While the server works, results are taken as they are; finding nothing is flagged.
    s.peer()
        .notify("$/progress", json!({ "token": "again", "value": { "kind": "begin", "title": "Indexing" } }))
        .unwrap();
    h.wait_status("mock", ServerStatus::Busy).await;
    let refs = h.mgr.references(doc, Pos::new(0, 5), true, true, Default::default()).await.unwrap();
    assert_eq!((refs.locations.len(), refs.incomplete), (1, None));
    s.respond("textDocument/references", json!([]));
    let refs = h.mgr.references(doc, Pos::new(0, 5), true, false, Default::default()).await.unwrap();
    assert_eq!(refs.locations.len(), 0);
    assert_eq!(refs.incomplete, Some(ReferencesIncomplete::Indexing));
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_edits_files_outside_the_analysis_scope() {
    let (h, s) = scope_harness();
    grep_candidates(&h);
    let b = h.root.join("b.mk");
    std::fs::write(&b, "call target\n").unwrap();
    let (doc, a) = h.open("a.mk", "def target\n");
    h.wait_status("mock", ServerStatus::Ready).await;
    eventually(|| s.docs.lock().len() == 1).await;
    let edit = h.mgr.rename(doc, Pos::new(0, 5), "renamed".into()).await.unwrap();
    let mut paths: Vec<PathBuf> = edit
        .files
        .iter()
        .map(|f| match f {
            FileEdit::Edit { path, .. } => path.to_path_buf(),
            _ => panic!("unexpected {f:?}"),
        })
        .collect();
    paths.sort();
    assert_eq!(paths, vec![a, b.clone()]);
    eventually(|| uris(&s.notified("textDocument/didClose")) == vec![uri_of(&b)]).await;
}

// ------------------------------------------------------------------ intellij-server licensing

#[tokio::test(flavor = "multi_thread")]
async fn licensing_capabilities_go_only_to_intellij_server() {
    let h = harness(&[("intellij-server", incremental_caps()), ("mock", incremental_caps())]);
    h.open("a.mk", "x");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    h.wait_status("mock", ServerStatus::Ready).await;

    let ij = &h.launcher.state("intellij-server").requested("initialize")[0]["capabilities"];
    assert_eq!(ij["experimental"]["licensingUi"], true);
    assert_eq!(ij["window"]["showDocument"]["support"], true);
    assert_eq!(ij["window"]["workDoneProgress"], true);

    let other = &h.launcher.state("mock").requested("initialize")[0]["capabilities"];
    assert_eq!(other["experimental"], json!({}));
    assert_eq!(other["window"]["showDocument"]["support"], false);
}

fn licensing_caps() -> Value {
    let mut caps = incremental_caps();
    caps["experimental"] = json!({ "licensing": true });
    caps
}

fn license_views(h: &Harness) -> Vec<ide_core::event::IntellijLicenseView> {
    h.events
        .lock()
        .iter()
        .filter_map(|e| match e {
            IdeEvent::IntellijLicense { view, .. } => Some((**view).clone()),
            _ => None,
        })
        .collect()
}

fn licensing_state(active: Value, jba: &str, extra: Value) -> Value {
    let mut state = json!({
        "activeLicense": active,
        "jbaLicensingState": { "authenticationStatus": jba },
        "licenseServerFlowState": { "kind": "notConfigured" },
        "featuresDeclined": false,
    });
    for (k, v) in extra.as_object().unwrap() {
        state[k] = v.clone();
    }
    state
}

fn license(status: &str) -> Value {
    json!({ "source": "subscription", "status": status, "product": "IIU" })
}

#[tokio::test(flavor = "multi_thread")]
async fn licensing_state_pushes_become_views() {
    use ide_core::event::IntellijLicenseKind as K;
    let h = harness(&[("intellij-server", licensing_caps())]);
    h.open("a.mk", "x");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    let peer = h.launcher.state("intellij-server").peer();
    let configured = json!({ "licenseServerFlowState": { "kind": "configured", "floatingTicketStatus": "Obtaining" } });
    let cases = [
        (licensing_state(Value::Null, "Authenticating", json!({})), K::Authenticating),
        (licensing_state(license("Expired"), "NotAuthenticated", configured), K::Authenticating),
        (licensing_state(license("Valid"), "Authenticating", json!({})), K::Licensed),
        (licensing_state(license("Valid"), "FailedToProlongateAuthentication", json!({})), K::SessionExpired),
        (licensing_state(license("Valid"), "Authenticated", json!({ "graceDeadline": 1767225600000u64 })), K::Licensed),
        (licensing_state(license("Expired"), "Authenticated", json!({ "graceDeadline": 1767225600000u64 })), K::Grace),
        (
            licensing_state(
                Value::Null,
                "NotAuthenticated",
                json!({ "featuresDeclined": true, "parkedReason": "StartedUnlicensed" }),
            ),
            K::Parked,
        ),
        (
            licensing_state(Value::Null, "NotAuthenticated", json!({ "graceDeadline": null, "parkedReason": null })),
            K::Unlicensed,
        ),
    ];
    for (i, (state, kind)) in cases.iter().enumerate() {
        peer.notify("jetbrains/licensing/state", state).unwrap();
        eventually(|| license_views(&h).len() == i + 1).await;
        assert_eq!(license_views(&h)[i].kind, *kind, "case {i}: {state}");
    }

    peer.notify(
        "jetbrains/licensing/state",
        json!({
            "activeLicense": { "source": "subscription", "status": "Valid", "product": "IIU", "licensedTo": "Ann",
                "licenseId": "L1", "validThrough": "2027-01-31", "daysLeft": 95, "assetId": "A1", "serverAddress": null },
            "jbaLicensingState": {
                "authenticationStatus": "Authenticated",
                "accountInfo": { "displayName": "Ann", "email": "ann@example.com" },
                "availableSubscriptions": [
                    { "kind": "subscription", "assetId": "A1", "product": "IIU", "licensedTo": "Ann", "status": "Valid", "validThrough": "2027-01-31", "licenseId": "L1" },
                    { "kind": "subscription", "assetId": "A2", "product": "IIU", "licensedTo": "Ann", "status": "Expired", "validThrough": "2020-01-31", "licenseId": "L2" }
                ],
                "errorMessage": null
            },
            "licenseServerFlowState": { "kind": "configured", "serverAddress": "https://ls.example.com", "status": "ok", "floatingTicketStatus": "Obtained" },
            "featuresDeclined": false
        }),
    )
    .unwrap();
    eventually(|| license_views(&h).len() == cases.len() + 1).await;
    let view = license_views(&h).pop().unwrap();
    assert_eq!(view.kind, K::Licensed);
    let active = view.license.unwrap();
    assert_eq!(
        (active.source.as_str(), active.licensed_to.as_deref(), active.days_left),
        ("subscription", Some("Ann"), Some(95))
    );
    assert_eq!(active.server_address, None);
    assert_eq!(view.account.display_name.as_deref(), Some("Ann"));
    assert_eq!(view.account.email.as_deref(), Some("ann@example.com"));
    assert_eq!(view.account.authentication_status, "Authenticated");
    assert_eq!(view.subscriptions.len(), 1);
    assert_eq!(
        (view.subscriptions[0].asset_id.as_str(), view.subscriptions[0].license_id.as_deref()),
        ("A1", Some("L1"))
    );
    assert_eq!(view.license_server.server_address.as_deref(), Some("https://ls.example.com"));
    assert_eq!(view.license_server.floating_ticket_status.as_deref(), Some("Obtained"));
    assert!(!view.env_license);
}

fn licensing_requests(s: &MockState) -> Vec<(String, Value)> {
    s.requests.lock().iter().filter(|(m, _)| m.starts_with("jetbrains/licensing/")).cloned().collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_reads_the_license_and_tries_discovery_without_one() {
    use ide_core::event::IntellijLicenseKind as K;
    let h = harness(&[("intellij-server", licensing_caps())]);
    let s = h.launcher.state("intellij-server");
    let activated = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let activated = activated.clone();
        *s.responder.lock() = Some(Arc::new(move |_, method, _| match method {
            "jetbrains/licensing/state/get" if activated.load(Ordering::SeqCst) => {
                Some(licensing_state(license("Valid"), "NotAuthenticated", json!({})))
            }
            "jetbrains/licensing/state/get" => Some(licensing_state(Value::Null, "NotAuthenticated", json!({}))),
            "jetbrains/licensing/discovery/autoActivate" => {
                activated.store(true, Ordering::SeqCst);
                Some(json!({ "activated": true, "source": "licenseFile" }))
            }
            _ => None,
        }));
    }
    h.open("a.mk", "x");
    eventually(|| license_views(&h).len() == 2).await;
    assert_eq!(license_views(&h).iter().map(|v| v.kind).collect::<Vec<_>>(), vec![K::Unlicensed, K::Licensed]);
    let sent = licensing_requests(&s);
    let methods: Vec<&str> = sent.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(
        methods,
        vec![
            "jetbrains/licensing/state/get",
            "jetbrains/licensing/discovery/autoActivate",
            "jetbrains/licensing/state/get"
        ]
    );
    assert!(sent.iter().all(|(_, p)| *p == json!({})));
    let server = h.mgr.servers()[0].id;
    assert_eq!(h.mgr.intellij_license(server).unwrap().kind, K::Licensed);
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_skips_discovery_with_a_license_and_servers_without_licensing() {
    let h = harness(&[("intellij-server", licensing_caps()), ("mock", licensing_caps())]);
    let ij = h.launcher.state("intellij-server");
    ij.respond("jetbrains/licensing/state/get", licensing_state(license("Expired"), "NotAuthenticated", json!({})));
    h.open("a.mk", "x");
    h.wait_status("mock", ServerStatus::Ready).await;
    eventually(|| license_views(&h).len() == 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(licensing_requests(&ij).len(), 1);
    assert!(licensing_requests(&h.launcher.state("mock")).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn licensing_requests_take_their_parameter_and_reject_unknown_names() {
    let h = harness(&[("intellij-server", licensing_caps()), ("mock", incremental_caps())]);
    let s = h.launcher.state("intellij-server");
    s.respond("jetbrains/licensing/state/get", licensing_state(license("Valid"), "Authenticated", json!({})));
    s.respond("jetbrains/licensing/activationCode/use", json!(true));
    h.open("a.mk", "x");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    h.wait_status("mock", ServerStatus::Ready).await;
    let server = |name: &str| h.mgr.servers().into_iter().find(|i| i.name == name).unwrap().id;
    let ij = server("intellij-server");

    let used = h.mgr.intellij_licensing(ij, "activationCode/use", Some("CODE-1".into())).await.unwrap();
    assert_eq!(used, json!(true));
    h.mgr.intellij_licensing(ij, "jba/logout", None).await.unwrap();
    let sent = licensing_requests(&s);
    assert!(sent.contains(&("jetbrains/licensing/activationCode/use".into(), json!({ "activationCode": "CODE-1" }))));
    assert!(sent.contains(&("jetbrains/licensing/jba/logout".into(), json!({}))));

    let invalid = ide_core::error::ErrorKind::InvalidInput;
    assert_eq!(h.mgr.intellij_licensing(ij, "jba/hack", None).await.unwrap_err().kind, invalid);
    assert_eq!(h.mgr.intellij_licensing(ij, "subscriptions/use", None).await.unwrap_err().kind, invalid);
    assert!(h.mgr.intellij_licensing(server("mock"), "state/get", None).await.is_err());
    assert_eq!(licensing_requests(&s).len(), sent.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_state_push_wins_over_an_older_state_reply() {
    use ide_core::event::IntellijLicenseKind as K;
    let h = harness(&[("intellij-server", licensing_caps())]);
    let s = h.launcher.state("intellij-server");
    let gate = Arc::new(Gate::default());
    {
        let gate = gate.clone();
        *s.responder.lock() = Some(Arc::new(move |_, method, _| {
            (method == "jetbrains/licensing/state/get").then(|| {
                gate.hold();
                licensing_state(Value::Null, "NotAuthenticated", json!({ "featuresDeclined": true }))
            })
        }));
    }
    h.open("a.mk", "x");
    eventually(|| gate.entered()).await;
    s.peer()
        .notify("jetbrains/licensing/state", licensing_state(license("Valid"), "Authenticated", json!({})))
        .unwrap();
    eventually(|| license_views(&h).len() == 1).await;
    gate.open();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let kinds: Vec<K> = license_views(&h).iter().map(|v| v.kind).collect();
    assert_eq!(kinds, vec![K::Licensed], "the reply that crossed the push is dropped");
    assert!(s.requested("jetbrains/licensing/discovery/autoActivate").is_empty());
    let server = h.mgr.servers()[0].id;
    assert_eq!(h.mgr.intellij_license(server).unwrap().kind, K::Licensed);
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_in_pages_open_in_the_browser() {
    let h = harness(&[("intellij-server", licensing_caps()), ("mock", incremental_caps())]);
    h.open("a.mk", "x");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    h.wait_status("mock", ServerStatus::Ready).await;
    let ij = h.launcher.state("intellij-server").peer();
    let mock = h.launcher.state("mock").peer();
    let url = "https://account.jetbrains.com/oauth2/auth?x=1";
    let show = |peer: Peer, params: Value| async move {
        peer.request::<_, Value>("window/showDocument", params, None).await.unwrap()
    };

    assert_eq!(show(ij.clone(), json!({ "uri": url, "external": true })).await, json!({ "success": true }));
    assert_eq!(show(ij.clone(), json!({ "uri": url })).await, json!({ "success": false }));
    assert_eq!(show(ij, json!({ "uri": "file:///etc/passwd", "external": true })).await, json!({ "success": false }));
    assert_eq!(show(mock, json!({ "uri": url, "external": true })).await, json!({ "success": false }));

    let ij_id = h.mgr.servers().into_iter().find(|i| i.name == "intellij-server").unwrap().id;
    let opened: Vec<(ServerId, String)> = h
        .events
        .lock()
        .iter()
        .filter_map(|e| match e {
            IdeEvent::OpenExternalUrl { server, url } => Some((*server, url.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(opened, vec![(ij_id, url.to_owned())]);
}

#[tokio::test(flavor = "multi_thread")]
async fn server_cancelled_requests_count_as_cancelled() {
    let h = harness(&[("mock", incremental_caps())]);
    let (doc, _) = h.open("a.mk", "x");
    h.wait_status("mock", ServerStatus::Ready).await;
    let s = h.launcher.state("mock");
    s.errors.lock().insert("textDocument/hover".into(), (-32802, "no license".into()));
    let err = h.mgr.hover(doc, Pos::new(0, 0), Default::default()).await.unwrap_err();
    assert_eq!(err.kind, ide_core::error::ErrorKind::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn getting_a_license_refreshes_the_editor_data() {
    let h = harness(&[("intellij-server", licensing_caps())]);
    let (_, path) = h.open("a.mk", "x");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    let s = h.launcher.state("intellij-server");
    let uri = uri_of(&path);
    eventually(|| s.docs.lock().contains_key(&uri)).await;
    let refreshes = || h.events.lock().iter().filter(|e| matches!(e, IdeEvent::LspRefresh { .. })).count();
    let push = |state: Value| s.peer().notify("jetbrains/licensing/state", state).unwrap();

    push(licensing_state(Value::Null, "NotAuthenticated", json!({ "featuresDeclined": true })));
    push(licensing_state(Value::Null, "Authenticating", json!({ "featuresDeclined": true })));
    eventually(|| license_views(&h).len() == 2).await;
    push(licensing_state(license("Valid"), "Authenticated", json!({})));
    eventually(|| refreshes() == 3).await;
    let server = h.mgr.servers()[0].id;
    let kinds: Vec<ide_core::event::LspRefreshKind> = h
        .events
        .lock()
        .iter()
        .filter_map(|e| match e {
            IdeEvent::LspRefresh { server: id, kind } if *id == server => Some(*kind),
            _ => None,
        })
        .collect();
    use ide_core::event::LspRefreshKind as R;
    assert_eq!(kinds, vec![R::SemanticTokens, R::InlayHints, R::CodeLens]);
    eventually(|| s.notified("textDocument/didOpen").len() == 2).await;
    assert_eq!(s.notified("textDocument/didClose").len(), 1);
    assert_eq!(s.docs.lock().get(&uri).map(String::as_str), Some("x"));

    push(licensing_state(license("Valid"), "Authenticated", json!({ "featuresDeclined": false })));
    eventually(|| license_views(&h).len() == 4).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(refreshes(), 3);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_declined_user_agreement_is_not_restarted() {
    let h = harness(&[("intellij-server", licensing_caps())]);
    let s = h.launcher.state("intellij-server");
    *s.exit_code.lock() = Some(11);
    h.open("a.mk", "x");
    h.wait_status("intellij-server", ServerStatus::Crashed).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(s.launches.load(Ordering::SeqCst), 1);
    let message = h.events.lock().iter().rev().find_map(|e| match e {
        IdeEvent::LanguageServer { status: ServerStatus::Crashed, message, .. } => message.clone(),
        _ => None,
    });
    assert_eq!(message.as_deref(), Some("the JetBrains User Agreement was not accepted"));
}

// ------------------------------------------------------------------ prompts

fn prompts(h: &Harness) -> Vec<(u64, ServerId, LspPrompt)> {
    h.events
        .lock()
        .iter()
        .filter_map(|e| match e {
            IdeEvent::LspPrompt { id, server, prompt } => Some((*id, *server, prompt.clone())),
            _ => None,
        })
        .collect()
}

fn closed_prompts(h: &Harness) -> Vec<u64> {
    h.events
        .lock()
        .iter()
        .filter_map(|e| match e {
            IdeEvent::LspPromptClosed { id } => Some(*id),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn message_requests_with_actions_wait_for_the_user() {
    let h = harness(&[("mock", incremental_caps())]);
    h.open("a.mk", "x");
    h.wait_status("mock", ServerStatus::Ready).await;
    let peer = h.launcher.state("mock").peer();
    let params = json!({ "type": 2, "message": "Import as?", "actions": [{ "title": "Maven" }, { "title": "Gradle", "id": 7 }] });
    let reply = tokio::spawn(async move { peer.request::<_, Value>("window/showMessageRequest", params, None).await });
    eventually(|| !prompts(&h).is_empty()).await;
    let (id, server, prompt) = prompts(&h).remove(0);
    assert_eq!(server, h.mgr.servers()[0].id);
    assert_eq!(
        prompt,
        LspPrompt::MessageRequest {
            level: NotifyLevel::Warning,
            message: "Import as?".into(),
            actions: vec!["Maven".into(), "Gradle".into()],
        }
    );
    assert!(closed_prompts(&h).is_empty());

    h.mgr.prompt_reply(id, Some("Gradle".into()));
    assert_eq!(reply.await.unwrap().unwrap(), json!({ "title": "Gradle", "id": 7 }));
    assert_eq!(closed_prompts(&h), vec![id]);
    h.mgr.prompt_reply(id, Some("Maven".into()));
    assert_eq!(closed_prompts(&h), vec![id]);
    assert!(!h.events.lock().iter().any(|e| matches!(e, IdeEvent::Notify { .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn prompts_close_when_their_server_stops() {
    let h = harness(&[("mock", incremental_caps())]);
    h.open("a.mk", "x");
    h.wait_status("mock", ServerStatus::Ready).await;
    let peer = h.launcher.state("mock").peer();
    let params = json!({ "type": 3, "message": "Import?", "actions": [{ "title": "Yes" }] });
    let reply = tokio::spawn(async move { peer.request::<_, Value>("window/showMessageRequest", params, None).await });
    eventually(|| !prompts(&h).is_empty()).await;
    let id = prompts(&h)[0].0;

    tokio::time::timeout(Duration::from_secs(5), h.mgr.restart(h.mgr.servers()[0].id)).await.unwrap().unwrap();
    assert_eq!(closed_prompts(&h), vec![id]);
    assert!(reply.await.unwrap().is_err());
}

fn show_conflicts(peer: Peer, uri: &str) -> tokio::task::JoinHandle<Result<Value, ide_jsonrpc::RpcError>> {
    let params = json!({
        "title": "Problems Detected",
        "conflicts": [
            { "messages": ["Method <b>run</b> is already defined"],
              "location": { "uri": uri, "range": {"start":{"line":0,"character":6},"end":{"line":0,"character":7}} } },
            { "messages": ["Library class", "is read-only"],
              "location": { "uri": "jrt://java.base/String.class", "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}} } },
            { "messages": ["No location"] }
        ],
        "continueLabel": "Continue",
        "cancelLabel": "Cancel",
        "revealLabel": "Show",
        "documentChangedLabel": "Changed"
    });
    tokio::spawn(async move { peer.request::<_, Value>("intellij/showConflicts", params, None).await })
}

#[tokio::test(flavor = "multi_thread")]
async fn intellij_conflicts_ask_whether_to_continue() {
    let mut caps = incremental_caps();
    caps["positionEncoding"] = json!("utf-8");
    let h = harness(&[("intellij-server", caps), ("mock", incremental_caps())]);
    let (_, path) = h.open("a.mk", "é😀x = 1\n");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    h.wait_status("mock", ServerStatus::Ready).await;
    let ij = h.launcher.state("intellij-server").peer();
    let uri = uri_of(&path);

    let reply = show_conflicts(ij.clone(), &uri);
    eventually(|| prompts(&h).len() == 1).await;
    let (id, server, prompt) = prompts(&h).remove(0);
    assert_eq!(server, h.mgr.servers().into_iter().find(|i| i.name == "intellij-server").unwrap().id);
    assert_eq!(
        prompt,
        LspPrompt::Conflicts {
            title: "Problems Detected".into(),
            conflicts: vec![
                LspConflict {
                    messages: vec!["Method <b>run</b> is already defined".into()],
                    path: Some(path.clone().into()),
                    range: Some(rng(0, 3, 0, 4)),
                },
                LspConflict { messages: vec!["Library class".into(), "is read-only".into()], path: None, range: None },
                LspConflict { messages: vec!["No location".into()], path: None, range: None },
            ],
            continue_label: "Continue".into(),
            cancel_label: "Cancel".into(),
        }
    );
    h.mgr.prompt_reply(id, Some("continue".into()));
    assert_eq!(reply.await.unwrap().unwrap(), json!({ "decision": "continue" }));

    let reply = show_conflicts(ij, &uri);
    eventually(|| prompts(&h).len() == 2).await;
    h.mgr.prompt_reply(prompts(&h)[1].0, None);
    assert_eq!(reply.await.unwrap().unwrap(), json!({ "decision": "cancel" }));
    assert_eq!(closed_prompts(&h).len(), 2);

    let mock = h.launcher.state("mock").peer();
    assert!(show_conflicts(mock, &uri).await.unwrap().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn editing_a_document_cancels_pending_conflicts() {
    let h = harness(&[("intellij-server", incremental_caps())]);
    let (doc, path) = h.open("a.mk", "x\n");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    let peer = h.launcher.state("intellij-server").peer();
    let question = json!({ "type": 3, "message": "Import?", "actions": [{ "title": "Yes" }] });
    let other = peer.clone();
    let asked =
        tokio::spawn(async move { other.request::<_, Value>("window/showMessageRequest", question, None).await });
    let reply = show_conflicts(peer, &uri_of(&path));
    eventually(|| prompts(&h).len() == 2).await;
    let conflicts = prompts(&h).into_iter().find(|p| matches!(p.2, LspPrompt::Conflicts { .. })).unwrap().0;

    h.mgr.did_change(doc, 2, &[change(rng(0, 1, 0, 1), "y")], &|| unreachable!());
    assert_eq!(reply.await.unwrap().unwrap(), json!({ "decision": "cancel" }));
    assert_eq!(closed_prompts(&h), vec![conflicts]);

    let question = prompts(&h).into_iter().find(|p| p.0 != conflicts).unwrap().0;
    h.mgr.prompt_reply(question, Some("Yes".into()));
    assert_eq!(asked.await.unwrap().unwrap(), json!({ "title": "Yes" }));
}

// ------------------------------------------------------------------ intellij-server extensions

fn intellij_caps() -> Value {
    let mut caps = incremental_caps();
    caps["executeCommandProvider"] = json!({ "commands": ["applyModCommand", "decompile"] });
    caps
}

fn bundled_intellij_server(lang: &str) -> LanguageServerConfig {
    let registry = LanguageRegistry::load(None).unwrap();
    let servers = registry.get(&LanguageId::new(lang)).unwrap().language_servers;
    servers.into_iter().find(|s| s.name == "intellij-server").unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn intellij_server_lists_fixes_lazily_and_applies_them_by_command() {
    assert_eq!(bundled_intellij_server("java"), bundled_intellij_server("kotlin"));
    let h = bundled_harness("java", &[("intellij-server", intellij_caps())]);
    let (doc, _) = h.open("App.java", "class App { List l; }\n");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    let s = h.launcher.state("intellij-server");
    let options = &s.requested("initialize")[0]["initializationOptions"];
    assert_eq!((&options["intellijExtensions"], &options["lazyIntentions"]), (&json!(true), &json!(true)));

    let lazy = json!({ "kind": "com.intellij.lsp.LazyAction", "sessionId": "s1", "index": 2 });
    s.respond(
        "textDocument/codeAction",
        json!([{ "title": "Import class", "kind": "quickfix",
                 "command": { "title": "Apply ModCommand", "command": "applyModCommand", "arguments": [lazy] } }]),
    );
    let actions = h.mgr.code_actions(doc, rng(0, 12, 0, 16), vec![]).await.unwrap();
    let command = actions[0].command.clone().unwrap();
    h.mgr.execute_command(doc, command).await.unwrap();
    assert_eq!(
        s.requested("workspace/executeCommand"),
        vec![json!({ "command": "applyModCommand", "arguments": [lazy] })]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn intellij_snippet_edits_apply_as_plain_text() {
    let h = harness(&[("intellij-server", intellij_caps()), ("mock", incremental_caps())]);
    let (doc, path) = h.open("a.mk", "foo(1 + 2);\n");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    h.wait_status("mock", ServerStatus::Ready).await;
    let workspace_edit =
        |s: &MockState| s.requested("initialize")[0]["capabilities"]["workspace"]["workspaceEdit"].clone();
    assert_eq!(workspace_edit(&h.launcher.state("intellij-server"))["snippetEditSupport"], true);
    assert_eq!(workspace_edit(&h.launcher.state("mock"))["snippetEditSupport"], Value::Null);

    let applied: Arc<Mutex<Vec<WorkspaceEdit>>> = Arc::default();
    {
        let applied = applied.clone();
        h.mgr.set_apply_edit_hook(Box::new(move |e| {
            applied.lock().push(e);
            true
        }));
    }
    let edit = json!({ "documentChanges": [{ "textDocument": { "uri": uri_of(&path), "version": 1 }, "edits": [
        { "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":0}}, "newText": "int " },
        { "range": {"start":{"line":0,"character":4},"end":{"line":0,"character":9}},
          "snippet": { "kind": "snippet", "value": "${1:sum}$0" } }
    ] }] });
    let s = h.launcher.state("intellij-server");
    let reply: Value = s.peer().request("workspace/applyEdit", json!({ "edit": edit }), None).await.unwrap();
    assert_eq!(reply, json!({ "applied": true }));
    let texts = |e: &WorkspaceEdit| match &e.files[0] {
        FileEdit::Edit { edits, .. } => edits.iter().map(|e| e.new_text.clone()).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(texts(&applied.lock()[0]), vec!["int ", "sum"]);

    s.respond("textDocument/codeAction", json!([{ "title": "Introduce variable", "edit": edit }]));
    let actions = h.mgr.code_actions(doc, rng(0, 4, 0, 9), vec![]).await.unwrap();
    let ij = actions.iter().find(|a| a.title == "Introduce variable").unwrap();
    assert_eq!(texts(ij.edit.as_ref().unwrap()), vec!["int ", "sum"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn intellij_initialization_progress_is_shown() {
    let h = harness(&[("intellij-server", intellij_caps()), ("mock", incremental_caps())]);
    h.open("a.mk", "x");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    h.wait_status("mock", ServerStatus::Ready).await;
    let s = h.launcher.state("intellij-server");
    assert_eq!(s.requested("initialize")[0]["workDoneToken"], "intellij-server-init");
    assert_eq!(h.launcher.state("mock").requested("initialize")[0].get("workDoneToken"), None);

    let progress = |value: Value| json!({ "token": "intellij-server-init", "value": value });
    s.peer().notify("$/progress", progress(json!({ "kind": "begin", "title": "Initializing server" }))).unwrap();
    s.peer().notify("$/progress", progress(json!({ "kind": "end" }))).unwrap();
    let tasks = || {
        h.events
            .lock()
            .iter()
            .filter_map(|e| match e {
                IdeEvent::Task { title, done, .. } => Some((title.clone(), *done)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    eventually(|| tasks().len() == 2).await;
    assert_eq!(tasks(), vec![("Initializing server".to_owned(), false), ("Initializing server".to_owned(), true)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn intellij_copies_to_the_clipboard() {
    let h = harness(&[("intellij-server", intellij_caps()), ("mock", incremental_caps())]);
    h.open("a.mk", "x");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    h.wait_status("mock", ServerStatus::Ready).await;
    h.launcher.state("mock").peer().notify("intellij/copyToClipboard", json!({ "content": "other" })).unwrap();
    h.launcher
        .state("intellij-server")
        .peer()
        .notify("intellij/copyToClipboard", json!({ "content": "a.b.C" }))
        .unwrap();
    let copied = || {
        h.events
            .lock()
            .iter()
            .filter_map(|e| match e {
                IdeEvent::CopyToClipboard { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    eventually(|| !copied().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(copied(), vec!["a.b.C".to_owned()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn intellij_choose_action_runs_or_reports_the_choice() {
    let h = harness(&[("intellij-server", intellij_caps())]);
    h.open("a.mk", "x");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    let s = h.launcher.state("intellij-server");
    let lazy = |i: u32| json!({ "kind": "LazyAction", "sessionId": "n", "index": i });
    let entry = move |name: &str, i: u32| json!({ "name": name, "command": { "title": "Apply ModCommand", "command": "applyModCommand", "arguments": [lazy(i)] } });
    *s.responder.lock() = Some(Arc::new(move |s, method, params| {
        (method == "workspace/executeCommand" && params["arguments"][0]["index"] == 0).then(|| {
            let again = json!({ "title": "Then", "entries": [entry("Deeper", 9)] });
            s.peer().notify("intellij/chooseAction", again).unwrap();
            Value::Null
        })
    }));
    let executed = || s.requested("workspace/executeCommand");
    let choose = |params: Value| s.peer().notify("intellij/chooseAction", params).unwrap();

    choose(json!({ "title": "Choose variant", "entries": [entry("First", 0), entry("Other", 1)] }));
    choose(json!({ "sessionId": 7, "entries": [{ "index": 3, "name": "A" }, { "index": 5, "name": "B" }] }));
    eventually(|| prompts(&h).len() == 2).await;
    let asked = prompts(&h);
    let new_shape = asked.iter().find(|p| matches!(&p.2, LspPrompt::ChooseAction { title, .. } if !title.is_empty()));
    let old_shape = asked.iter().find(|p| matches!(&p.2, LspPrompt::ChooseAction { title, .. } if title.is_empty()));
    let (new_shape, old_shape) = (new_shape.unwrap(), old_shape.unwrap());
    assert_eq!(
        new_shape.2,
        LspPrompt::ChooseAction { title: "Choose variant".into(), items: vec!["First".into(), "Other".into()] }
    );
    assert_eq!(old_shape.2, LspPrompt::ChooseAction { title: String::new(), items: vec!["A".into(), "B".into()] });

    h.mgr.prompt_reply(old_shape.0, Some("B".into()));
    eventually(|| executed().len() == 1).await;
    assert_eq!(executed()[0], json!({ "command": "chooseModCommandAction", "arguments": [7, 5] }));

    h.mgr.prompt_reply(new_shape.0, Some("First".into()));
    eventually(|| prompts(&h).len() == 3).await;
    assert_eq!(executed()[1], json!({ "command": "applyModCommand", "arguments": [lazy(0)] }));
    assert_eq!(prompts(&h)[2].2, LspPrompt::ChooseAction { title: "Then".into(), items: vec!["Deeper".into()] });
    h.mgr.prompt_reply(prompts(&h)[2].0, None);

    choose(json!({ "sessionId": 8, "title": "Old", "entries": [{ "index": 0, "name": "A" }] }));
    eventually(|| prompts(&h).len() == 4).await;
    h.mgr.prompt_reply(prompts(&h)[3].0, None);
    eventually(|| executed().len() == 3).await;
    assert_eq!(executed()[2], json!({ "command": "chooseModCommandAction", "arguments": [8, null] }));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(executed().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn intellij_editor_commands_reach_the_editor() {
    let h = harness(&[("intellij-server", intellij_caps())]);
    let (doc, path) = h.open("a.mk", "x");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    let s = h.launcher.state("intellij-server");
    let run = |params: Value| s.peer().notify("intellij/runEditorCommand", params).unwrap();
    let editor_commands = || {
        h.events
            .lock()
            .iter()
            .filter_map(|e| match e {
                IdeEvent::RunEditorCommand { command, path } => Some((command.clone(), path.clone())),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    run(json!({ "command": "editor.action.rename", "arguments": [], "uri": uri_of(&path) }));
    run(json!({ "command": "editor.action.organizeImports", "arguments": [] }));
    run(json!({ "command": "editor.action.triggerSuggest", "arguments": [] }));
    eventually(|| editor_commands().len() == 2).await;

    let command =
        Command { title: "".into(), command: "editor.action.triggerParameterHints".into(), arguments: vec![] };
    h.mgr.execute_command(doc, command).await.unwrap();
    assert_eq!(
        editor_commands(),
        vec![
            ("editor.action.rename".to_owned(), Some(path.clone().into())),
            ("editor.action.triggerSuggest".to_owned(), None),
            ("editor.action.triggerParameterHints".to_owned(), Some(path.into())),
        ]
    );
    assert!(s.requested("workspace/executeCommand").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn intellij_shows_project_files_with_a_selection() {
    let mut caps = intellij_caps();
    caps["positionEncoding"] = json!("utf-8");
    let h = harness(&[("intellij-server", caps), ("mock", incremental_caps())]);
    let (_, path) = h.open("a.mk", "é😀x = 1\n");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    h.wait_status("mock", ServerStatus::Ready).await;
    let show = |name: &str, params: Value| {
        let peer = h.launcher.state(name).peer();
        async move { peer.request::<_, Value>("window/showDocument", params, None).await.unwrap() }
    };
    let selection = json!({ "start": { "line": 0, "character": 6 }, "end": { "line": 0, "character": 7 } });

    let params = json!({ "uri": uri_of(&path), "selection": selection, "takeFocus": true });
    assert_eq!(show("intellij-server", params.clone()).await, json!({ "success": true }));
    assert_eq!(
        show("intellij-server", json!({ "uri": uri_of(&path), "external": false })).await,
        json!({ "success": true })
    );
    assert_eq!(show("mock", params).await, json!({ "success": false }));
    let shown: Vec<(PathBuf, Option<Range>, bool)> = h
        .events
        .lock()
        .iter()
        .filter_map(|e| match e {
            IdeEvent::ShowDocument { path, selection, take_focus } => {
                Some((path.to_path_buf(), *selection, *take_focus))
            }
            _ => None,
        })
        .collect();
    assert_eq!(shown, vec![(path.clone(), Some(rng(0, 3, 0, 4)), true), (path, None, false)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn intellij_library_sources_are_decompiled() {
    let h = harness_for("java", "java", &[("intellij-server", intellij_caps())], &[]);
    let (doc, _) = h.open("App.java", "class App { String s; }\n");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    let s = h.launcher.state("intellij-server");
    let jar = "jar:///opt/jdk/lib/src.zip!/java.base/java/lang/String.java";
    let range = json!({ "start": { "line": 172, "character": 19 }, "end": { "line": 172, "character": 25 } });
    s.respond("textDocument/definition", json!([{ "uri": jar, "range": range }]));
    let defs = h.mgr.goto(doc, Pos::new(0, 13), GotoKind::Definition).await.unwrap();
    assert_eq!(defs[0].uri.as_deref(), Some(jar));

    s.respond(
        "workspace/executeCommand",
        json!({ "code": "package java.lang;\n\npublic final class String {}\n", "language": "java" }),
    );
    let vd = h.mgr.virtual_document(jar).await.unwrap();
    assert_eq!(s.requested("workspace/executeCommand"), vec![json!({ "command": "decompile", "arguments": [jar] })]);
    assert_eq!(vd.text, "package java.lang;\n\npublic final class String {}\n");
    assert_eq!(vd.language_id.as_str(), "java");
    assert_eq!(vd.title, "String.java");
    assert_eq!(vd.description.as_deref(), Some("/opt/jdk/lib/src.zip › java.base"));
    assert!(!vd.decompiled);
    assert_eq!(h.mgr.virtual_document(jar).await.unwrap(), vd);
    assert_eq!(s.requested("workspace/executeCommand").len(), 1);

    s.respond("workspace/executeCommand", Value::Null);
    let err = h.mgr.virtual_document("jrt:/java.base/sun/misc/Hidden.class").await.unwrap_err();
    assert!(err.message.contains("could not decompile jrt:/java.base/sun/misc/Hidden.class"), "{}", err.message);
}

#[tokio::test(flavor = "multi_thread")]
async fn intellij_inlay_hint_settings_are_answered() {
    let h = bundled_harness("java", &[("intellij-server", intellij_caps())]);
    h.open("App.java", "class App {}\n");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    let items = json!({ "items": [{ "section": "jetbrains.java" }, { "section": "jetbrains.kotlin" }] });
    let reply: Value =
        h.launcher.state("intellij-server").peer().request("workspace/configuration", items, None).await.unwrap();
    assert_eq!(
        reply,
        json!([
            {
                "hints.collapse complex types": true,
                "hints.settings.method parameter": true,
                "hints.types.local variable": true,
                "settings.types.lambda parameter": true,
                "hints.types.call chain": true
            },
            {
                "hints.settings.types.property": true,
                "hints.settings.types.variable": true,
                "hints.type.function.return": true,
                "hints.type.function.parameter": true,
                "hints.settings.lambda.return": true,
                "hints.lambda.receivers.parameters": true,
                "hints.settings.value.ranges": true,
                "hints.value.kotlin.time": true,
                "hints.parameters": true,
                "hints.parameters.compiled": true,
                "hints.call.chains": false,
                "hints.parameters.excluded": false
            }
        ])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn intellij_failed_imports_are_reported() {
    let h = harness(&[("intellij-server", intellij_caps())]);
    h.open("a.mk", "x");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    let peer = h.launcher.state("intellij-server").peer();
    let log = |extra: Value| {
        let mut line = json!({ "type": 3, "message": "line", "started": false, "failed": false, "succeeded": false });
        for (k, v) in extra.as_object().unwrap() {
            line[k] = v.clone();
        }
        peer.notify("intellij/importLog", line).unwrap();
    };
    log(json!({ "tool": "Gradle", "started": true, "message": "Gradle: importing /q" }));
    log(json!({ "message": "stale" }));
    log(json!({ "tool": "Maven", "started": true, "message": "Maven: importing /p" }));
    for i in 0..25 {
        log(json!({ "type": 2, "message": format!("[ERROR] line {i}") }));
    }
    log(json!({ "type": 1, "tool": "Maven", "failed": true, "message": "Could not resolve dependencies" }));
    log(json!({ "tool": "Gradle", "started": true }));
    log(json!({ "succeeded": true, "message": "Workspace imported successfully" }));
    peer.notify("intellij/copyToClipboard", json!({ "content": "done" })).unwrap();
    eventually(|| h.events.lock().iter().any(|e| matches!(e, IdeEvent::CopyToClipboard { .. }))).await;
    let notified: Vec<(NotifyLevel, Option<String>)> = h
        .events
        .lock()
        .iter()
        .filter_map(|e| match e {
            IdeEvent::Notify { level, title, body } if title == "intellij-server" => Some((*level, body.clone())),
            _ => None,
        })
        .collect();
    let tail: Vec<String> = (6..25).map(|i| format!("[ERROR] line {i}")).collect();
    let body = format!("Maven import failed\n{}\nCould not resolve dependencies", tail.join("\n"));
    assert_eq!(notified, vec![(NotifyLevel::Error, Some(body))]);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_fallback_serves_while_the_preferred_server_has_given_up() {
    let h = harness_with_fallbacks(&[("main", incremental_caps()), ("alt", incremental_caps())], &["alt"]);
    let main = h.launcher.state("main");
    *main.exit_code.lock() = Some(1);
    let (_, path) = h.open("a.mk", "x");
    let uri = uri_of(&path);
    eventually(|| {
        h.statuses("main").last() == Some(&ServerStatus::Crashed) && main.launches.load(Ordering::SeqCst) == 4
    })
    .await;
    h.wait_status("alt", ServerStatus::Ready).await;
    eventually(|| h.launcher.state("alt").docs.lock().contains_key(&uri)).await;

    *main.exit_code.lock() = None;
    let id = h.mgr.servers().into_iter().find(|s| s.name == "main").unwrap().id;
    h.mgr.restart(id).await.unwrap();
    eventually(|| main.docs.lock().contains_key(&uri)).await;
    eventually(|| h.removed("alt")).await;
    assert!(!h.launcher.state("alt").notified("textDocument/didClose").is_empty());
    assert_eq!(h.mgr.servers().iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["main"]);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_fallback_serves_when_intellij_server_refuses_to_run() {
    let h = harness_with_fallbacks(&[("intellij-server", licensing_caps()), ("alt", incremental_caps())], &["alt"]);
    *h.launcher.state("intellij-server").exit_code.lock() = Some(11);
    let (_, path) = h.open("a.mk", "x");
    h.wait_status("alt", ServerStatus::Ready).await;
    eventually(|| h.launcher.state("alt").docs.lock().contains_key(&uri_of(&path))).await;
    assert_eq!(h.launcher.state("intellij-server").launches.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_instance_has_no_license_view() {
    let h = harness(&[("intellij-server", licensing_caps())]);
    h.open("a.mk", "x");
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    let s = h.launcher.state("intellij-server");
    s.peer()
        .notify("jetbrains/licensing/state", licensing_state(license("Valid"), "Authenticated", json!({})))
        .unwrap();
    let server = h.mgr.servers()[0].id;
    eventually(|| h.mgr.intellij_license(server).is_some()).await;
    s.peer().kill();
    eventually(|| h.statuses("intellij-server").contains(&ServerStatus::Crashed)).await;
    assert!(h.mgr.intellij_license(server).is_none());
}

fn server_id(h: &Harness, name: &str) -> ServerId {
    h.mgr.servers().into_iter().find(|s| s.name == name).unwrap().id
}

fn names(h: &Harness) -> Vec<String> {
    let mut names: Vec<String> = h.mgr.servers().into_iter().map(|s| s.name).collect();
    names.sort();
    names
}

async fn hover_text(h: &Harness, doc: DocId) -> Option<String> {
    h.mgr.hover(doc, Pos::new(0, 0), Default::default()).await.ok().flatten().map(|h| h.contents.value)
}

fn answer_hover(s: &MockState, text: &str) {
    s.respond("textDocument/hover", json!({ "contents": { "kind": "markdown", "value": text } }));
}

#[cfg(unix)]
async fn fallback_serving(h: &Harness, preferred: &str, code: i32, crashes: usize) -> (DocId, String) {
    *h.launcher.state(preferred).exit_code.lock() = Some(code);
    let (doc, path) = h.open("a.mk", "x");
    let uri = uri_of(&path);
    eventually(|| {
        h.statuses(preferred).last() == Some(&ServerStatus::Crashed)
            && h.launcher.state(preferred).launches.load(Ordering::SeqCst) == crashes
    })
    .await;
    h.wait_status("alt", ServerStatus::Ready).await;
    eventually(|| h.launcher.state("alt").docs.lock().contains_key(&uri)).await;
    (doc, uri)
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_fallback_stays_until_the_restarted_server_is_ready() {
    let h = harness_with_fallbacks(&[("main", incremental_caps()), ("alt", incremental_caps())], &["alt"]);
    let (_, uri) = fallback_serving(&h, "main", 1, 4).await;
    let main = h.launcher.state("main");
    let alt = h.launcher.state("alt");
    *main.exit_code.lock() = None;
    let gate = Arc::new(Gate::default());
    *main.init_gate.lock() = Some(gate.clone());
    let restart = {
        let mgr = h.mgr.clone();
        let id = server_id(&h, "main");
        tokio::spawn(async move { mgr.restart(id).await })
    };
    eventually(|| gate.entered()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(alt.notified("textDocument/didClose").is_empty());
    assert!(alt.docs.lock().contains_key(&uri));
    assert!(!h.removed("alt"));

    gate.open();
    restart.await.unwrap().unwrap();
    eventually(|| h.removed("alt")).await;
    assert!(!alt.notified("textDocument/didClose").is_empty());
    assert!(main.docs.lock().contains_key(&uri));
    assert_eq!(names(&h), ["main"]);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_fallback_outlives_a_failed_restart_of_intellij_server() {
    let h = harness_with_fallbacks(&[("intellij-server", licensing_caps()), ("alt", incremental_caps())], &["alt"]);
    let (doc, uri) = fallback_serving(&h, "intellij-server", 11, 1).await;
    let alt = h.launcher.state("alt");
    answer_hover(&alt, "alt");

    assert!(h.mgr.restart(server_id(&h, "intellij-server")).await.is_err());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(alt.launches.load(Ordering::SeqCst), 1);
    assert!(alt.notified("textDocument/didClose").is_empty());
    assert!(alt.docs.lock().contains_key(&uri));
    assert_eq!(hover_text(&h, doc).await.as_deref(), Some("alt"));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_retiring_fallback_is_replaced_when_the_preferred_server_dies() {
    let h = harness_with_fallbacks(&[("intellij-server", licensing_caps()), ("alt", incremental_caps())], &["alt"]);
    let (doc, uri) = fallback_serving(&h, "intellij-server", 11, 1).await;
    let ij = h.launcher.state("intellij-server");
    let alt = h.launcher.state("alt");
    let gate = Arc::new(Gate::default());
    *alt.shutdown_gate.lock() = Some(gate.clone());
    *ij.exit_code.lock() = None;
    let restart = {
        let mgr = h.mgr.clone();
        let id = server_id(&h, "intellij-server");
        tokio::spawn(async move { mgr.restart(id).await })
    };
    eventually(|| gate.entered()).await;
    h.wait_status("intellij-server", ServerStatus::Ready).await;
    *ij.exit_code.lock() = Some(11);
    ij.peer().kill();

    eventually(|| alt.launches.load(Ordering::SeqCst) == 2).await;
    eventually(|| h.mgr.servers().iter().any(|s| s.name == "alt" && s.status == ServerStatus::Ready)).await;
    eventually(|| alt.docs.lock().contains_key(&uri)).await;
    answer_hover(&alt, "alt");
    assert_eq!(hover_text(&h, doc).await.as_deref(), Some("alt"));
    gate.open();
    let _ = restart.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn enabling_a_missing_preferred_server_keeps_the_fallback_until_it_runs() {
    let h = harness_with_fallbacks(&[("main", incremental_caps()), ("alt", incremental_caps())], &["alt"]);
    let disabled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec!["main".into()]));
    {
        let disabled = disabled.clone();
        h.mgr.set_servers_hook(Box::new(move |_, defined| {
            defined.iter().filter(|c| !disabled.lock().contains(&c.name)).cloned().collect()
        }));
    }
    let version = Arc::new(AtomicUsize::new(0));
    {
        let version = version.clone();
        h.mgr.set_config_hook(Box::new(move |cfg, _| {
            if cfg.name == "main" {
                cfg.args = vec![version.load(Ordering::SeqCst).to_string()];
            }
        }));
    }
    let (doc, path) = h.open("a.mk", "x");
    let uri = uri_of(&path);
    let main = h.launcher.state("main");
    let alt = h.launcher.state("alt");
    h.wait_status("alt", ServerStatus::Ready).await;

    main.missing.store(true, Ordering::SeqCst);
    disabled.lock().clear();
    h.mgr.reconfigure().await;
    h.wait_status("main", ServerStatus::Missing).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(alt.notified("textDocument/didClose").is_empty());
    assert!(!h.statuses("alt").contains(&ServerStatus::Stopped));
    assert!(alt.docs.lock().contains_key(&uri));

    main.missing.store(false, Ordering::SeqCst);
    version.fetch_add(1, Ordering::SeqCst);
    h.mgr.reconfigure().await;
    h.wait_status("main", ServerStatus::Ready).await;
    eventually(|| h.removed("alt")).await;
    assert!(main.docs.lock().contains_key(&uri));
    assert_eq!(names(&h), ["main"]);
    answer_hover(&main, "main");
    assert_eq!(hover_text(&h, doc).await.as_deref(), Some("main"));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_reconfigures_leave_the_document_served() {
    let h = harness_with_fallbacks(&[("main", incremental_caps()), ("alt", incremental_caps())], &["alt"]);
    let disabled: Arc<Mutex<Vec<String>>> = Default::default();
    {
        let disabled = disabled.clone();
        h.mgr.set_servers_hook(Box::new(move |_, defined| {
            defined.iter().filter(|c| !disabled.lock().contains(&c.name)).cloned().collect()
        }));
    }
    let (doc, _) = h.open("a.mk", "x");
    h.wait_status("main", ServerStatus::Ready).await;
    for i in 0..5 {
        if i % 2 == 0 {
            disabled.lock().push("main".into());
        } else {
            disabled.lock().clear();
        }
        tokio::join!(h.mgr.reconfigure(), h.mgr.reconfigure());
    }
    answer_hover(&h.launcher.state("alt"), "alt");
    eventually(|| names(&h) == ["alt"] && h.mgr.servers()[0].status == ServerStatus::Ready).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while hover_text(&h, doc).await.as_deref() != Some("alt") {
        assert!(std::time::Instant::now() < deadline, "the document has no working server");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    disabled.lock().clear();
    tokio::join!(h.mgr.reconfigure(), h.mgr.reconfigure());
    h.wait_status("main", ServerStatus::Ready).await;
    eventually(|| names(&h) == ["main"]).await;
    answer_hover(&h.launcher.state("main"), "main");
    assert_eq!(hover_text(&h, doc).await.as_deref(), Some("main"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_virtual_document_keeps_the_fallback_it_was_opened_on() {
    let h = harness_with_fallbacks(&[("main", incremental_caps()), ("alt", incremental_caps())], &["alt"]);
    let main = h.launcher.state("main");
    let alt = h.launcher.state("alt");
    main.missing.store(true, Ordering::SeqCst);
    let (_, path) = h.open("a.mk", "x");
    h.wait_status("alt", ServerStatus::Ready).await;
    let class = "jdt://contents/x.class";
    h.mgr.did_open_virtual(DocId::next(), class, &h.lang, 1, "class X {}");
    eventually(|| alt.docs.lock().contains_key(class)).await;

    main.missing.store(false, Ordering::SeqCst);
    h.mgr.restart(server_id(&h, "main")).await.unwrap();
    eventually(|| !alt.docs.lock().contains_key(&uri_of(&path))).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(alt.docs.lock().contains_key(class));
    assert!(!h.removed("alt"));
}
