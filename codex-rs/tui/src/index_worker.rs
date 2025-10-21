use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_agentic_core::index::builder::BuildOptions;
use codex_agentic_core::index::builder::build_with_progress;
use codex_agentic_core::index::events::IndexEvent;
use tokio::task;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::index_status::IndexStatusSnapshot;

#[derive(Clone)]
pub(crate) struct IndexWorker {
    root: PathBuf,
    sender: AppEventSender,
    running: Arc<AtomicBool>,
}

impl IndexWorker {
    pub(crate) fn new(root: PathBuf, sender: AppEventSender) -> Self {
        Self {
            root,
            sender,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn spawn_build(&self, mut options: BuildOptions) {
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let root = self.root.clone();
        options.project_root = root.clone();
        let sender = self.sender.clone();
        let running = self.running.clone();
        task::spawn(async move {
            let progress_sender = sender.clone();
            let result = task::spawn_blocking(move || {
                build_with_progress(options, |event| {
                    progress_sender.send(AppEvent::IndexProgress(event))
                })
            })
            .await;

            match result {
                Ok(Ok(_summary)) => {
                    if let Ok(Some(snapshot)) = IndexStatusSnapshot::load(root.as_path()) {
                        sender.send(AppEvent::IndexStatusUpdated(Some(snapshot)));
                    } else {
                        sender.send(AppEvent::IndexStatusUpdated(None));
                    }
                }
                Ok(Err(err)) => {
                    sender.send(AppEvent::IndexProgress(IndexEvent::Error {
                        message: err.to_string(),
                    }));
                    sender.send(AppEvent::IndexStatusUpdated(None));
                }
                Err(join_err) => {
                    sender.send(AppEvent::IndexProgress(IndexEvent::Error {
                        message: format!("index task join error: {join_err}"),
                    }));
                    sender.send(AppEvent::IndexStatusUpdated(None));
                }
            }

            running.store(false, Ordering::SeqCst);
        });
    }
}
