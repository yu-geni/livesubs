//! The subtitles on show: confirmed lines, and at most one line still being spoken.

/// `time` is the line's HH:mm; `show_time` is false while it is the same minute as the line
/// above, so the transcript gets one stamp per minute. Interim lines carry no stamp at all.
pub struct Item { pub uid: u64, pub conn: u32, pub id: i32, pub ja: String, pub en: String, pub dim: bool,
                  pub time: String, pub show_time: bool }

/// One row of the 目次 outline. `seg` is the line it starts at, `k` its number on this connection.
pub struct Topic { pub conn: u32, pub k: i32, pub seg: i32, pub title: String, pub summary: String, pub time: String }

pub const MAX_ITEMS: usize = 5000;

#[derive(Default)]
pub struct Model {
    pub items: Vec<Item>,
    pub topics: Vec<Topic>,
    /// Counts every change to the 目次, so that a view knows a row it already has was rewritten.
    pub topics_rev: u64,
    pub last_lag_ms: i32,
    pub ever_had_subtitle: bool,
    pub models: String,
    interim: Option<u64>,
    conn: u32,
    max_id: i32, // highest segment id seen on this connection
    min_id: i32, // ids below this belong to a stream the user has switched away from
    next_uid: u64,
}

impl Model {
    pub fn new() -> Model { Model { last_lag_ms: -1, max_id: -1, ..Default::default() } }

    fn drop_interim(&mut self) {
        if let Some(uid) = self.interim.take() { self.items.retain(|i| i.uid != uid); }
    }

    fn add(&mut self, id: i32, ja: &str, en: &str, dim: bool) -> u64 {
        self.next_uid += 1;
        // The stamp is the one the 目次 points at, so it is kept on every confirmed line; only
        // showing it is left out while the minute has not changed.
        let (time, show_time) = if dim {
            (String::new(), false)
        } else {
            let hm = crate::core::now().hm;
            let show = self.items.last().map(|i| i.time != hm).unwrap_or(true);
            (hm, show)
        };
        self.items.push(Item { uid: self.next_uid, conn: self.conn, id, ja: ja.into(), en: en.into(), dim, time, show_time });
        if self.items.len() > MAX_ITEMS { self.items.remove(0); }
        self.next_uid
    }

    /// The user switched to another stream: empty the screen, and ignore whatever the engine
    /// still sends about segments that had already started.
    pub fn clear(&mut self) {
        self.items.clear();
        self.topics.clear();
        self.interim = None;
        self.last_lag_ms = -1;
        self.min_id = self.max_id + 1;
    }

    /// Segment ids restart at 0 on every connection; what is on show stays.
    pub fn on_ready(&mut self, asr: &str, mt: &str, device: &str) {
        self.conn += 1;
        self.max_id = -1;
        self.min_id = 0;
        self.models = format!("{asr} / {mt} ({device})");
        self.drop_interim();
    }

    pub fn on_interim(&mut self, id: i32, ja: &str, en: &str) {
        if id < self.min_id { return; }
        self.max_id = self.max_id.max(id);
        let conn = self.conn;
        // the line being spoken is the newest one: look from the end
        let current = self.interim.and_then(|uid| self.items.iter_mut().rev().find(|i| i.uid == uid && i.conn == conn && i.id == id));
        match current {
            Some(item) => {
                if !ja.is_empty() { item.ja = ja.into(); } // an empty translation keeps the one on show
                item.en = en.into();
            }
            None => {
                self.drop_interim();
                self.interim = Some(self.add(id, ja, en, true));
            }
        }
        if !ja.is_empty() { self.ever_had_subtitle = true; }
    }

    /// False when the line belongs to a stream that was switched away from.
    pub fn on_final(&mut self, id: i32, ja: &str, en: &str, lag_ms: i32) -> bool {
        if id < self.min_id { return false; }
        self.max_id = self.max_id.max(id);
        if lag_ms >= 0 { self.last_lag_ms = lag_ms; }
        self.drop_interim();
        let conn = self.conn;
        match self.items.iter_mut().find(|i| i.conn == conn && i.id == id) {
            Some(item) => { item.ja = ja.into(); item.en = en.into(); item.dim = false; }
            None => { self.add(id, ja, en, false); }
        }
        self.ever_had_subtitle = true;
        true
    }

    /// A 目次 entry. The engine sends the same `k` again when it rewrites one.
    pub fn on_topic(&mut self, k: i32, seg: i32, title: &str, summary: &str) -> bool {
        if seg < self.min_id { return false; }
        let conn = self.conn;
        self.topics_rev += 1;
        if let Some(t) = self.topics.iter_mut().find(|t| t.conn == conn && t.k == k) {
            (t.title, t.summary) = (title.into(), summary.into());
            return true;
        }
        let time = self.find(conn, seg).map(|i| self.items[i].time.clone()).unwrap_or_else(|| crate::core::now().hm);
        self.topics.push(Topic { conn, k, seg, title: title.into(), summary: summary.into(), time });
        if self.topics.len() > MAX_ITEMS { self.topics.remove(0); }
        true
    }

    /// The first transcript line of a topic, for the click-to-scroll behaviour.
    pub fn topic_target(&self, t: &Topic) -> Option<usize> {
        self.find(t.conn, t.seg).or_else(|| self.items.iter().position(|i| i.conn == t.conn && i.id >= t.seg))
    }

    /// The confirmed line (conn, id), newest first; never the line still being spoken.
    fn find(&self, conn: u32, id: i32) -> Option<usize> {
        self.items.iter().rposition(|i| i.conn == conn && i.id == id && Some(i.uid) != self.interim)
    }
}
