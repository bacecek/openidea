use crate::caps::ServerCaps;
use crate::server::{Running, Server};
use crate::text::Encoding;
use crate::uri::{uri_target, uri_to_path};
use crate::{LspManager, Target};
use ide_core::event::{
    IntellijAccount, IntellijActiveLicense, IntellijLicenseKind, IntellijLicenseServer, IntellijLicenseView,
    IntellijSubscription, LspPrompt, LspRefreshKind, NotifyLevel,
};
use ide_core::{IdeError, IdeEvent, IdeResult, ServerId};
use ide_language::LanguageServerConfig;
use lsp_types as lsp;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub(crate) const SERVER: &str = "intellij-server";

const ENV_LICENSE: [&str; 3] = ["INTELLIJ_SERVER_LICENSE", "INTELLIJ_SERVER_LICENSE_FILE", "JETBRAINS_LICENSE_SERVER"];

const IMPORT_LOG_LINES: usize = 200;
const FAILED_IMPORT_LINES: usize = 20;

pub(crate) const EDITOR_COMMANDS: [&str; 3] =
    ["editor.action.rename", "editor.action.triggerSuggest", "editor.action.triggerParameterHints"];

const REQUESTS: [(&str, Option<&str>, u64); 12] = [
    ("state/get", None, 10_000),
    ("discovery/autoActivate", None, 60_000),
    ("trial/startAnonymous", None, 60_000),
    ("jba/login", None, 310_000),
    ("jba/logout", None, 30_000),
    ("subscriptions/update", None, 60_000),
    ("subscriptions/use", Some("assetId"), 30_000),
    ("subscriptions/unuseAll", None, 30_000),
    ("activationCode/use", Some("activationCode"), 60_000),
    ("activationCode/disable", None, 60_000),
    ("licenseServer/add", Some("serverAddress"), 360_000),
    ("licenseServer/remove", None, 30_000),
];

#[derive(Default)]
pub(crate) struct License {
    view: Option<IntellijLicenseView>,
    pushes: u64,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LicensingState {
    active_license: Option<IntellijActiveLicense>,
    jba_licensing_state: JbaState,
    license_server_flow_state: IntellijLicenseServer,
    features_declined: bool,
    parked_reason: Option<String>,
    grace_deadline: Option<f64>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct JbaState {
    authentication_status: String,
    account_info: Option<AccountInfo>,
    available_subscriptions: Vec<Subscription>,
    error_message: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct AccountInfo {
    display_name: Option<String>,
    email: Option<String>,
}

#[derive(Deserialize)]
struct Subscription {
    #[serde(default)]
    status: String,
    #[serde(flatten)]
    info: IntellijSubscription,
}

fn without_nulls(value: Value) -> Value {
    match value {
        Value::Object(o) => {
            Value::Object(o.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, without_nulls(v))).collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(without_nulls).collect()),
        other => other,
    }
}

fn with_required(value: Option<&mut Value>, keys: &[&str]) {
    if let Some(Value::Object(o)) = value {
        for key in keys {
            o.entry(*key).or_insert_with(|| json!(""));
        }
    }
}

fn parse_state(state: Value) -> serde_json::Result<LicensingState> {
    let mut state = without_nulls(state);
    with_required(state.get_mut("activeLicense"), &["source", "status", "product"]);
    with_required(state.get_mut("licenseServerFlowState"), &["kind"]);
    if let Some(Value::Array(subscriptions)) = state.pointer_mut("/jbaLicensingState/availableSubscriptions") {
        for s in subscriptions {
            with_required(Some(s), &["assetId", "product"]);
        }
    }
    serde_json::from_value(state)
}

fn kind(state: &LicensingState) -> IntellijLicenseKind {
    let valid = state.active_license.as_ref().is_some_and(|l| l.status == "Valid");
    let jba = state.jba_licensing_state.authentication_status.as_str();
    let flow = &state.license_server_flow_state;
    let obtaining =
        flow.kind == "configured" && matches!(flow.floating_ticket_status.as_deref(), Some("Obtaining" | "Refreshing"));
    if !valid && (jba == "Authenticating" || obtaining) {
        IntellijLicenseKind::Authenticating
    } else if valid && jba == "FailedToProlongateAuthentication" {
        IntellijLicenseKind::SessionExpired
    } else if valid {
        IntellijLicenseKind::Licensed
    } else if state.grace_deadline.is_some() {
        IntellijLicenseKind::Grace
    } else if state.features_declined {
        IntellijLicenseKind::Parked
    } else {
        IntellijLicenseKind::Unlicensed
    }
}

fn view(state: LicensingState, env_license: bool) -> IntellijLicenseView {
    let kind = kind(&state);
    let jba = state.jba_licensing_state;
    let account = jba.account_info.unwrap_or_default();
    IntellijLicenseView {
        kind,
        license: state.active_license,
        account: IntellijAccount {
            authentication_status: jba.authentication_status,
            display_name: account.display_name,
            email: account.email,
            error_message: jba.error_message,
        },
        subscriptions: jba
            .available_subscriptions
            .into_iter()
            .filter(|s| s.status == "Valid")
            .map(|s| s.info)
            .collect(),
        license_server: state.license_server_flow_state,
        parked_reason: state.parked_reason,
        grace_deadline: state.grace_deadline.filter(|_| kind != IntellijLicenseKind::Licensed),
        env_license,
    }
}

fn env_license(cfg: &LanguageServerConfig) -> bool {
    ENV_LICENSE.iter().any(|key| {
        let value = cfg.env.get(*key).cloned().or_else(|| std::env::var(key).ok()).or_else(|| {
            ide_core::process::shell_env().find(|(k, _)| *k == *key).map(|(_, v)| v.to_string_lossy().into_owned())
        });
        value.is_some_and(|v| !v.trim().is_empty())
    })
}

pub(crate) fn add_client_capabilities(caps: &mut Value) {
    caps["experimental"]["licensingUi"] = json!(true);
    caps["window"]["showDocument"]["support"] = json!(true);
    caps["workspace"]["workspaceEdit"]["snippetEditSupport"] = json!(true);
}

fn supports_licensing(caps: &ServerCaps) -> bool {
    caps.raw.pointer("/experimental/licensing") == Some(&Value::Bool(true))
}

pub(crate) fn external_url<'a>(server: &str, params: &'a Value) -> Option<&'a str> {
    if server != SERVER || params.get("external") != Some(&Value::Bool(true)) {
        return None;
    }
    let uri = params.get("uri")?.as_str()?;
    let lower = uri.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")).then_some(uri)
}

pub(crate) fn request_timeout(server: &str, method: &str) -> Duration {
    match method {
        "workspace/executeCommand" | "textDocument/rename" | "workspace/willRenameFiles" if server == SERVER => {
            crate::prompt::PROMPT_TIMEOUT
        }
        _ => crate::LONG_TIMEOUT,
    }
}

pub(crate) fn exit_message(server: &str, code: Option<i32>, tail: &str) -> Option<String> {
    if server != SERVER {
        return None;
    }
    Some(match code? {
        7 if tail.is_empty() => "license check failed".to_owned(),
        7 => format!("license check failed: {tail}"),
        11 => "the JetBrains User Agreement was not accepted".to_owned(),
        12 => "the intellij-server bundle is damaged; reinstall it in Settings › Languages & Frameworks › Language Servers"
            .to_owned(),
        _ => return None,
    })
}

impl LspManager {
    pub(crate) fn on_intellij_notification(
        self: &Arc<Self>,
        server: &Arc<Server>,
        generation: u64,
        method: &str,
        params: Value,
    ) {
        match method {
            "intellij/copyToClipboard" => {
                if let Some(text) = params.get("content").and_then(Value::as_str) {
                    self.sink.emit(IdeEvent::CopyToClipboard { text: text.to_owned() });
                }
            }
            "intellij/chooseAction" => self.choose_action(server.clone(), generation, params),
            "intellij/runEditorCommand" => {
                let Some(command) = params.get("command").and_then(Value::as_str) else { return };
                if EDITOR_COMMANDS.contains(&command) {
                    let path = params.get("uri").and_then(Value::as_str).map(|uri| uri_target(uri).0.into());
                    self.sink.emit(IdeEvent::RunEditorCommand { command: command.to_owned(), path });
                }
            }
            "intellij/importLog" => {
                let message = params.get("message").and_then(Value::as_str).unwrap_or_default();
                match params.get("type").and_then(Value::as_u64) {
                    Some(1) => tracing::warn!("[{}] {message}", server.name),
                    Some(2 | 3) => tracing::info!("[{}] {message}", server.name),
                    _ => tracing::debug!("[{}] {message}", server.name),
                }
                let tail: Option<Vec<String>> = {
                    let mut logs = self.intellij_import_logs.lock();
                    let log = logs.entry(server.id).or_default();
                    if params.get("started") == Some(&Value::Bool(true)) {
                        log.clear();
                    }
                    if !message.is_empty() {
                        if log.len() == IMPORT_LOG_LINES {
                            log.pop_front();
                        }
                        log.push_back(message.to_owned());
                    }
                    let failed = params.get("failed") == Some(&Value::Bool(true));
                    failed.then(|| log.iter().skip(log.len().saturating_sub(FAILED_IMPORT_LINES)).cloned().collect())
                };
                if let Some(tail) = tail {
                    let tool = params.get("tool").and_then(Value::as_str).unwrap_or("Build");
                    let body = [format!("{tool} import failed")].into_iter().chain(tail).collect::<Vec<_>>().join("\n");
                    let title = server.name.clone();
                    self.sink.emit(IdeEvent::Notify { level: NotifyLevel::Error, title, body: Some(body) });
                }
            }
            "intellij/workspaceImportState" | "intellij/workspaceImportStatus" | "intellij/ready-for-test" => {}
            other => tracing::trace!("[{}] unhandled notification {other}", server.name),
        }
    }

    fn choose_action(self: &Arc<Self>, server: Arc<Server>, generation: u64, params: Value) {
        let Some(entries) = params.get("entries").and_then(Value::as_array).filter(|e| !e.is_empty()).cloned() else {
            return;
        };
        let name = |e: &Value| e.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
        let prompt = LspPrompt::ChooseAction {
            title: params.get("title").and_then(Value::as_str).unwrap_or_default().to_owned(),
            items: entries.iter().map(name).collect(),
        };
        let me = self.clone();
        self.runtime.spawn_blocking(move || {
            let chosen = me.ask(server.id, prompt).and_then(|c| entries.into_iter().find(|e| name(e) == c));
            let command = match (chosen.as_ref().and_then(|e| e.get("command")), params.get("sessionId")) {
                (Some(command), _) => {
                    let mut command = command.clone();
                    if let Some(c) = command.as_object_mut() {
                        c.remove("title");
                    }
                    command
                }
                (None, Some(session)) => {
                    let index = chosen.as_ref().and_then(|e| e.get("index")).cloned().unwrap_or(Value::Null);
                    json!({ "command": "chooseModCommandAction", "arguments": [session, index] })
                }
                (None, None) => return,
            };
            let Some(run) = server.running_gen(generation) else { return };
            let t = Target { server, run };
            let mgr = me.clone();
            me.runtime.spawn(async move {
                let timeout = request_timeout(&t.server.name, "workspace/executeCommand");
                let sent = mgr.call::<_, Value>(&t, "workspace/executeCommand", command, None, timeout);
                if let Err(e) = sent.await {
                    tracing::warn!("{}: chosen action: {e}", t.server.name);
                }
            });
        });
    }

    pub(crate) fn show_file(&self, server: &Server, enc: Encoding, params: &Value) -> bool {
        if server.name != SERVER || params.get("external") == Some(&Value::Bool(true)) {
            return false;
        }
        let Some(path) = params.get("uri").and_then(Value::as_str).and_then(uri_to_path) else { return false };
        let selection = params.get("selection").and_then(|r| serde_json::from_value::<lsp::Range>(r.clone()).ok());
        let selection = selection.map(|r| self.conv(enc).range(&path, r));
        let take_focus = params.get("takeFocus") == Some(&Value::Bool(true));
        self.sink.emit(IdeEvent::ShowDocument { path: path.into(), selection, take_focus });
        true
    }

    pub(crate) fn on_licensing_state(&self, server: &Server, params: Value) {
        self.store_licensing_state(server, params, None);
    }

    fn store_licensing_state(&self, server: &Server, state: Value, after_pushes: Option<u64>) {
        let state: LicensingState = match parse_state(state) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("{}: bad licensing state: {e}", server.name);
                return;
            }
        };
        let view = view(state, env_license(&server.effective_config()));
        let previous = {
            let mut licenses = self.intellij_licenses.lock();
            let license = licenses.entry(server.id).or_default();
            match after_pushes {
                Some(n) if n != license.pushes => return,
                Some(_) => {}
                None => license.pushes += 1,
            }
            license.view.replace(view.clone()).map(|v| v.kind)
        };
        let kind = view.kind;
        self.sink.emit(IdeEvent::IntellijLicense { server: server.id, view: Box::new(view) });
        if kind == IntellijLicenseKind::Licensed
            && previous
                .is_some_and(|k| !matches!(k, IntellijLicenseKind::Licensed | IntellijLicenseKind::SessionExpired))
        {
            self.refresh_licensed(server);
        }
    }

    fn refresh_licensed(&self, server: &Server) {
        for kind in [LspRefreshKind::SemanticTokens, LspRefreshKind::InlayHints, LspRefreshKind::CodeLens] {
            self.sink.emit(IdeEvent::LspRefresh { server: server.id, kind });
        }
        let Some(run) = server.running() else { return };
        if !run.caps.read().open_close {
            return;
        }
        let docs = self.docs.lock();
        for d in docs.values() {
            if !d.servers.iter().any(|a| a.server.id == server.id && a.opened_gen == Some(run.generation)) {
                continue;
            }
            if let Err(e) = run
                .peer
                .notify("textDocument/didClose", json!({ "textDocument": { "uri": d.uri } }))
                .and_then(|_| run.peer.notify("textDocument/didOpen", d.did_open_params()))
            {
                tracing::debug!("{}: reopen: {e}", server.name);
            }
        }
    }

    pub(crate) fn start_licensing(&self, server: &Arc<Server>, run: &Running) {
        if server.name != SERVER || !supports_licensing(&run.caps.read()) {
            return;
        }
        let Some(me) = self.me.upgrade() else { return };
        let t = Target { server: server.clone(), run: run.clone() };
        self.runtime.spawn(async move {
            if let Err(e) = me.licensing_request(&t, "state/get", json!({}), 10_000).await {
                tracing::warn!("{}: licensing state: {e}", t.server.name);
                return;
            }
            if me.intellij_license(t.server.id).is_some_and(|v| v.license.is_some()) {
                return;
            }
            if let Err(e) = me.licensing_request(&t, "discovery/autoActivate", json!({}), 60_000).await {
                tracing::warn!("{}: license discovery: {e}", t.server.name);
            }
            if let Err(e) = me.licensing_request(&t, "state/get", json!({}), 10_000).await {
                tracing::warn!("{}: licensing state: {e}", t.server.name);
            }
        });
    }

    async fn licensing_request(&self, t: &Target, request: &str, params: Value, timeout: u64) -> IdeResult<Value> {
        let pushes = self.intellij_licenses.lock().entry(t.server.id).or_default().pushes;
        let method = format!("jetbrains/licensing/{request}");
        let result: Value = self.call(t, &method, params, None, Duration::from_millis(timeout)).await?;
        if request == "state/get" {
            self.store_licensing_state(&t.server, result.clone(), Some(pushes));
        }
        Ok(result)
    }

    pub async fn intellij_licensing(&self, server: ServerId, request: &str, value: Option<String>) -> IdeResult<Value> {
        let Some(&(_, param, timeout)) = REQUESTS.iter().find(|(name, ..)| *name == request) else {
            return Err(IdeError::invalid(format!("unknown licensing request {request}")));
        };
        let params = match (param, value) {
            (None, _) => json!({}),
            (Some(param), Some(value)) => json!({ param: value }),
            (Some(param), None) => return Err(IdeError::invalid(format!("{request} needs {param}"))),
        };
        let s = self
            .servers
            .lock()
            .iter()
            .find(|s| s.id == server && s.name == SERVER)
            .cloned()
            .ok_or_else(|| IdeError::not_found(format!("unknown {SERVER} {server}")))?;
        let run = s
            .wait_ready(crate::START_WAIT)
            .await
            .ok_or_else(|| IdeError::unavailable(format!("{SERVER} is not running")))?;
        if !supports_licensing(&run.caps.read()) {
            return Err(IdeError::unavailable(format!("{SERVER} does not support licensing")));
        }
        self.licensing_request(&Target { server: s, run }, request, params, timeout).await
    }

    pub fn intellij_license(&self, server: ServerId) -> Option<IntellijLicenseView> {
        self.intellij_licenses.lock().get(&server).and_then(|l| l.view.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refactoring_requests_wait_for_the_user() {
        let long = Duration::from_secs(30);
        for method in ["workspace/executeCommand", "textDocument/rename", "workspace/willRenameFiles"] {
            assert_eq!(request_timeout(SERVER, method), Duration::from_secs(600), "{method}");
            assert_eq!(request_timeout("jdtls", method), long, "{method}");
        }
        assert_eq!(request_timeout(SERVER, "textDocument/references"), long);
    }

    #[test]
    fn grace_deadline_is_dropped_while_licensed() {
        let state = parse_state(json!({
            "activeLicense": { "source": "trial", "status": "Valid", "product": "IIU" },
            "jbaLicensingState": { "authenticationStatus": "NotAuthenticated", "errorMessage": null },
            "graceDeadline": 1767225600000u64,
        }))
        .unwrap();
        let view = view(state, false);
        assert_eq!(view.kind, IntellijLicenseKind::Licensed);
        assert_eq!(view.grace_deadline, None);
    }

    #[test]
    fn a_missing_field_keeps_the_rest_of_the_state() {
        let state = parse_state(json!({
            "activeLicense": { "source": "subscription", "status": "Valid" },
            "jbaLicensingState": {
                "authenticationStatus": "Authenticated",
                "availableSubscriptions": [{ "assetId": "A1", "status": "Valid" }]
            },
            "licenseServerFlowState": { "serverAddress": "https://ls.example.com" },
        }))
        .unwrap();
        let view = view(state, false);
        assert_eq!(view.kind, IntellijLicenseKind::Licensed);
        assert_eq!(view.license.unwrap().product, "");
        assert_eq!(view.subscriptions[0].asset_id, "A1");
        assert_eq!(view.license_server.kind, "");
        assert_eq!(view.license_server.server_address.as_deref(), Some("https://ls.example.com"));
    }
}
