use crate::LspManager;
use ide_core::event::LspPrompt;
use ide_core::{IdeEvent, ServerId};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

const PROMPT_TIMEOUT: Duration = Duration::from_secs(600);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct Pending {
    server: ServerId,
    conflicts: bool,
    reply: mpsc::SyncSender<Option<String>>,
}

impl LspManager {
    pub(crate) fn ask(&self, server: ServerId, prompt: LspPrompt) -> Option<String> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = mpsc::sync_channel(1);
        let conflicts = matches!(prompt, LspPrompt::Conflicts { .. });
        self.prompts.lock().insert(id, Pending { server, conflicts, reply });
        self.sink.emit(IdeEvent::LspPrompt { id, server, prompt });
        answer.recv_timeout(PROMPT_TIMEOUT).unwrap_or_else(|_| {
            self.close_prompts(None, |p, _| p == id);
            answer.try_recv().ok().flatten()
        })
    }

    pub fn prompt_reply(&self, id: u64, answer: Option<String>) {
        self.close_prompts(answer, |p, _| p == id);
    }

    fn close_prompts(&self, answer: Option<String>, which: impl Fn(u64, &Pending) -> bool) {
        let closed: Vec<(u64, Pending)> = self.prompts.lock().extract_if(|id, p| which(*id, p)).collect();
        for (id, pending) in closed {
            let _ = pending.reply.send(answer.clone());
            self.sink.emit(IdeEvent::LspPromptClosed { id });
        }
    }

    pub(crate) fn close_conflict_prompts(&self, servers: &[ServerId]) {
        self.close_prompts(None, |_, p| p.conflicts && servers.contains(&p.server));
    }

    pub(crate) fn close_server_prompts(&self, server: ServerId) {
        self.close_prompts(None, |_, p| p.server == server);
    }
}
