//! Reads Windows toast notifications (Action Center) through
//! UserNotificationListener and reports the ones that arrive after start.

use crate::system;
use crate::worker::decode_square;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tiny_skia::Pixmap;
use windows::core::Result;
use windows::Foundation::Size;
use windows::UI::Notifications::Management::{UserNotificationListener, UserNotificationListenerAccessStatus};
use windows::UI::Notifications::{KnownNotificationBindings, NotificationKinds, UserNotification};

pub struct Notif {
    pub app: String,
    pub title: String,
    pub body: String,
    pub time: String,
    pub icon: Option<Arc<Pixmap>>,
}

#[derive(Default)]
pub struct Notifier {
    listener: Option<UserNotificationListener>,
    denied: bool,
    seen: HashSet<u32>,
    primed: bool,
    icons: HashMap<String, Option<Arc<Pixmap>>>,
}

impl Notifier {
    /// New notifications since the previous call (oldest first).
    pub fn poll(&mut self) -> Vec<Notif> {
        if self.denied {
            return Vec::new();
        }
        if self.listener.is_none() {
            let Ok(l) = UserNotificationListener::Current() else {
                self.denied = true;
                return Vec::new();
            };
            let allowed = match l.GetAccessStatus() {
                Ok(s) if s == UserNotificationListenerAccessStatus::Allowed => true,
                _ => l
                    .RequestAccessAsync()
                    .and_then(|op| op.join())
                    .map(|s| s == UserNotificationListenerAccessStatus::Allowed)
                    .unwrap_or(false),
            };
            if !allowed {
                self.denied = true;
                return Vec::new();
            }
            self.listener = Some(l);
        }
        let Some(l) = &self.listener else { return Vec::new() };
        let Ok(list) = l.GetNotificationsAsync(NotificationKinds::Toast).and_then(|op| op.join()) else {
            return Vec::new();
        };
        let mut current = HashSet::new();
        let mut fresh: Vec<(i64, UserNotification)> = Vec::new();
        for i in 0..list.Size().unwrap_or(0) {
            let Ok(n) = list.GetAt(i) else { continue };
            let Ok(id) = n.Id() else { continue };
            current.insert(id);
            // First poll: take what is already in Action Center too, so the
            // Notif tab is not empty at start (the UI shows no HUD for it).
            if !self.primed || !self.seen.contains(&id) {
                let t = n.CreationTime().map(|d| d.UniversalTime).unwrap_or(0);
                fresh.push((t, n));
            }
        }
        self.seen = current;
        self.primed = true;
        fresh.sort_by_key(|(t, _)| *t);
        let skip = fresh.len().saturating_sub(10);
        fresh.drain(..skip);
        fresh
            .into_iter()
            .filter_map(|(_, n)| self.parse(&n).ok())
            .collect()
    }

    fn parse(&mut self, n: &UserNotification) -> Result<Notif> {
        let binding = n
            .Notification()?
            .Visual()?
            .GetBinding(&KnownNotificationBindings::ToastGeneric()?)?;
        let texts = binding.GetTextElements()?;
        let mut lines = Vec::new();
        for i in 0..texts.Size()? {
            let t = texts.GetAt(i)?.Text()?.to_string_lossy();
            if !t.trim().is_empty() {
                lines.push(t.replace(['\r', '\n'], " "));
            }
        }
        let (app, icon) = match n.AppInfo().and_then(|a| a.DisplayInfo()) {
            Ok(info) => {
                let name = info.DisplayName().map(|h| h.to_string_lossy()).unwrap_or_default();
                let icon = self
                    .icons
                    .entry(name.clone())
                    .or_insert_with(|| {
                        info.GetLogo(Size { Width: 64.0, Height: 64.0 })
                            .and_then(|r| r.OpenReadAsync())
                            .and_then(|op| op.join())
                            .and_then(|s| decode_square(&s, 64))
                            .ok()
                            .map(Arc::new)
                    })
                    .clone();
                (name, icon)
            }
            Err(_) => (String::new(), None),
        };
        let app = if app.is_empty() { "Notifikasi".to_string() } else { app };
        let title = lines.first().cloned().unwrap_or_else(|| app.clone());
        let body = lines.iter().skip(1).cloned().collect::<Vec<_>>().join(" · ");
        let clk = system::local_time();
        Ok(Notif {
            app,
            title,
            body,
            time: format!("{:02}:{:02}", clk.wHour, clk.wMinute),
            icon,
        })
    }
}
