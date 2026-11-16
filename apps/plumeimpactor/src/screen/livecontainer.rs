use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use futures::{SinkExt, channel::mpsc};
use iced::{
    Element, Task,
    widget::{button, column, pick_list, progress_bar, row, text},
};
use plume_utils::{Device, LiveContainerInstallResult, LiveContainerProgress, LiveContainerTarget};

use crate::appearance;

#[derive(Debug, Clone)]
pub enum Message {
    Refresh,
    Loaded(Result<Vec<LiveContainerTarget>, String>),
    SelectTarget(LiveContainerTarget),
    ChooseIpa,
    IpaChosen(Option<PathBuf>),
    Start,
    Progress(LiveContainerProgress),
    Finished(Result<LiveContainerInstallResult, String>),
    Cancel,
}

#[derive(Debug, Clone)]
pub struct LiveContainerPanel {
    device: Option<Device>,
    targets: Vec<LiveContainerTarget>,
    target: Option<LiveContainerTarget>,
    ipa: Option<PathBuf>,
    loading: bool,
    choosing: bool,
    transferring: bool,
    cancel: Option<Arc<AtomicBool>>,
    progress: f32,
    status: String,
}

impl LiveContainerPanel {
    pub fn new(device: Option<Device>) -> Self {
        Self {
            device,
            targets: Vec::new(),
            target: None,
            ipa: None,
            loading: false,
            choosing: false,
            transferring: false,
            cancel: None,
            progress: 0.0,
            status: String::new(),
        }
    }

    pub fn busy(&self) -> bool {
        self.loading || self.choosing || self.transferring
    }

    pub fn sync_device(&mut self, device: Option<Device>) {
        if !self.busy() && self.device.as_ref().map(|d| &d.udid) != device.as_ref().map(|d| &d.udid)
        {
            self.device = device;
            self.targets.clear();
            self.target = None;
        }
    }

    pub fn cancel(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
            self.status = "Cancelling and removing temporary transfer files…".into();
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Refresh if !self.busy() => {
                let Some(device) = self.device.clone().filter(|d| !d.is_mac) else {
                    self.status = "Connect and trust your iPhone first.".into();
                    return Task::none();
                };
                self.loading = true;
                self.targets.clear();
                self.target = None;
                self.status = "Looking for LiveContainer…".into();
                let (tx, rx) = futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let result = tokio::runtime::Runtime::new()
                        .map_err(|e| e.to_string())
                        .and_then(|rt| {
                            rt.block_on(device.livecontainer_targets())
                                .map_err(|e| e.to_string())
                        });
                    let _ = tx.send(result);
                });
                Task::perform(
                    async move {
                        rx.await
                            .unwrap_or_else(|_| Err("Device lookup stopped unexpectedly.".into()))
                    },
                    Message::Loaded,
                )
            }
            Message::Loaded(result) => {
                self.loading = false;
                match result {
                    Ok(targets) => {
                        self.status = if targets.is_empty() {
                            "No LiveContainer found. Install and open it on this iPhone first."
                                .into()
                        } else {
                            String::new()
                        };
                        self.target = (targets.len() == 1).then(|| targets[0].clone());
                        self.targets = targets;
                    }
                    Err(error) => self.status = format!("Could not find LiveContainer: {error}"),
                }
                Task::none()
            }
            Message::SelectTarget(target) if !self.busy() => {
                self.target = Some(target);
                Task::none()
            }
            Message::ChooseIpa if !self.busy() => {
                self.choosing = true;
                Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .set_title("Choose IPA for LiveContainer")
                            .add_filter("iOS application", &["ipa", "tipa"])
                            .pick_file()
                            .await
                            .map(|f| f.path().to_path_buf())
                    },
                    Message::IpaChosen,
                )
            }
            Message::IpaChosen(path) => {
                self.choosing = false;
                if path.is_some() {
                    self.ipa = path;
                }
                Task::none()
            }
            Message::Start if !self.busy() => {
                let (Some(device), Some(target), Some(ipa)) =
                    (self.device.clone(), self.target.clone(), self.ipa.clone())
                else {
                    return Task::none();
                };
                self.transferring = true;
                self.progress = 0.0;
                self.status = "Checking the IPA… Keep LiveContainer closed.".into();
                let cancel = Arc::new(AtomicBool::new(false));
                self.cancel = Some(cancel.clone());
                let (mut tx, rx) = mpsc::channel(8);
                std::thread::spawn(move || {
                    let result = match tokio::runtime::Runtime::new() {
                        Ok(rt) => {
                            let mut updates = tx.clone();
                            rt.block_on(device.install_to_livecontainer(
                                &ipa,
                                &target.bundle_id,
                                Some(cancel),
                                move |p| {
                                    // A slow UI must not retain an unbounded progress queue.
                                    let _ = updates.try_send(Message::Progress(p));
                                },
                            ))
                            .map_err(|e| e.to_string())
                        }
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = futures::executor::block_on(tx.send(Message::Finished(result)));
                });
                Task::run(rx, |message| message)
            }
            Message::Progress(p) => {
                self.progress = if p.total_bytes == 0 {
                    0.0
                } else {
                    (p.transferred_bytes as f64 / p.total_bytes as f64 * 100.0) as f32
                };
                if !self
                    .cancel
                    .as_ref()
                    .is_some_and(|c| c.load(Ordering::Relaxed))
                {
                    self.status = format!(
                        "{:.1}% — {:.2} / {:.2} GB\n{}",
                        self.progress,
                        p.transferred_bytes as f64 / 1e9,
                        p.total_bytes as f64 / 1e9,
                        p.current_file
                    );
                }
                Task::none()
            }
            Message::Finished(result) => {
                self.transferring = false;
                self.cancel = None;
                self.status = match result {
                    Ok(result) => {
                        self.progress = 100.0;
                        format!(
                            "{} was added. Fully close and reopen LiveContainer, then tap the new app. LiveContainer handles signing on first launch.",
                            result.app_name
                        )
                    }
                    Err(error) => format!("Transfer did not finish: {error}"),
                };
                Task::none()
            }
            Message::Cancel => {
                self.cancel();
                Task::none()
            }
            _ => Task::none(),
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let ready = !self.busy();
        let connected = self.device.as_ref().is_some_and(|d| !d.is_mac);
        let mut content = column![
            text("Install into LiveContainer").size(20),
            text("Keep LiveContainer closed during transfer. Each install adds a new guest copy. Set up signing or JIT in LiveContainer before first launch.").size(13),
            button(text(if self.loading { "Finding LiveContainer…" } else { "Find LiveContainer" }))
                .on_press_maybe((ready && connected).then_some(Message::Refresh)).style(appearance::s_button),
        ].spacing(8);
        if !self.targets.is_empty() {
            if ready {
                content = content.push(
                    pick_list(
                        self.targets.clone(),
                        self.target.clone(),
                        Message::SelectTarget,
                    )
                    .placeholder("Choose LiveContainer"),
                );
            } else if let Some(target) = &self.target {
                content = content.push(text(target.to_string()));
            }
        }
        content = content.push(
            row![
                button("Choose IPA")
                    .on_press_maybe(ready.then_some(Message::ChooseIpa))
                    .style(appearance::s_button),
                button("Install into LiveContainer")
                    .on_press_maybe(
                        (ready && connected && self.target.is_some() && self.ipa.is_some())
                            .then_some(Message::Start)
                    )
                    .style(appearance::p_button),
            ]
            .spacing(8),
        );
        if let Some(ipa) = &self.ipa {
            content = content.push(text(ipa.display().to_string()).size(13));
        }
        if self.transferring {
            content = content.push(progress_bar(0.0..=100.0, self.progress)).push(
                button("Cancel transfer")
                    .on_press(Message::Cancel)
                    .style(appearance::s_button),
            );
        }
        if !self.status.is_empty() {
            content = content.push(text(&self.status).size(13));
        }
        content.into()
    }
}
