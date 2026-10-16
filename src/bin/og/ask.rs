//! Questions for the open Pack, answered on a thread of their own so a slow
//! answer never holds up a key press. Only the newest question counts: one
//! asked while another waits takes its place, a long one stops once a newer
//! one is asked, and the answer to an older one is never taken.

use std::{
    collections::HashMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

use crate::{
    batch,
    engine::{Answer, Engine, Hit},
};

pub enum Question {
    /// What search answers for a query, and how the parser reads it.
    Search(String),
    Reverse {
        lat: f64,
        lon: f64,
    },
    /// Every address of pasted text, as a batch run answers it.
    Pasted(String),
    /// What a batch run answers for each of these addresses.
    Addresses(Vec<String>),
}

pub enum Reply {
    Search {
        answer: Result<Answer, String>,
        reading: Vec<(String, &'static str)>,
    },
    Reverse(Result<Answer, String>),
    Pasted {
        rows: Vec<PastedRow>,
        /// How the rows were read from the text.
        note: String,
    },
    Addresses(Vec<Option<Hit>>),
}

/// One pasted address and the answer for it.
pub struct PastedRow {
    pub input: String,
    pub answer: Option<Arc<Answer>>,
}

impl PastedRow {
    pub fn hit(&self) -> Option<&Hit> {
        self.answer.as_ref().and_then(|answer| answer.hits.first())
    }
}

pub struct Asker {
    shared: Arc<Shared>,
    /// The id of the newest question.
    asked: u64,
}

struct Shared {
    waiting: Mutex<Option<(u64, Question)>>,
    wake: Condvar,
    /// The id of the newest question: one being answered stops once it is
    /// no longer this.
    newest: AtomicU64,
    /// The id of the newest question answered or given up.
    settled: AtomicU64,
    answered: Mutex<Option<(u64, Reply)>>,
    closed: AtomicBool,
}

impl Asker {
    pub fn new(engine: Arc<Engine>) -> Self {
        let shared = Arc::new(Shared {
            waiting: Mutex::new(None),
            wake: Condvar::new(),
            newest: AtomicU64::new(0),
            settled: AtomicU64::new(0),
            answered: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        {
            let shared = Arc::clone(&shared);
            thread::spawn(move || answer(&engine, &shared));
        }
        Self { shared, asked: 0 }
    }

    pub fn ask(&mut self, question: Question) {
        self.asked += 1;
        self.shared.newest.store(self.asked, Ordering::Relaxed);
        *self.shared.waiting.lock().expect("question lock") = Some((self.asked, question));
        self.shared.wake.notify_one();
    }

    /// Drops the questions asked so far: their answers are not taken.
    pub fn forget(&mut self) {
        self.asked += 1;
        self.shared.newest.store(self.asked, Ordering::Relaxed);
        self.shared.settled.fetch_max(self.asked, Ordering::Relaxed);
        *self.shared.waiting.lock().expect("question lock") = None;
    }

    /// The answer to the newest question, once it is there.
    pub fn take(&mut self) -> Option<Reply> {
        match self.shared.answered.lock().expect("answer lock").take() {
            Some((id, reply)) if id == self.asked => Some(reply),
            _ => None,
        }
    }

    /// Whether the newest question is still being answered.
    pub fn busy(&self) -> bool {
        self.shared.settled.load(Ordering::Relaxed) < self.asked
    }
}

impl Drop for Asker {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        // Under the lock, so the thread cannot miss the wake between
        // looking at `closed` and waiting.
        let _waiting = self.shared.waiting.lock().expect("question lock");
        self.shared.wake.notify_one();
    }
}

/// Answers questions until the asker is dropped. Answers to pasted rows are
/// kept for as long as the Pack is open, so text pasted again, or with a
/// row more, is answered at once.
fn answer(engine: &Engine, shared: &Shared) {
    let mut known: HashMap<String, Option<Arc<Answer>>> = HashMap::new();
    loop {
        let (id, question) = {
            let mut waiting = shared.waiting.lock().expect("question lock");
            loop {
                if shared.closed.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(next) = waiting.take() {
                    break next;
                }
                waiting = shared.wake.wait(waiting).expect("question lock");
            }
        };
        let newest = || shared.newest.load(Ordering::Relaxed) == id;
        let reply = match question {
            Question::Search(query) => Some(Reply::Search {
                reading: engine.read(&query).parts,
                answer: engine.search(&query).map_err(|error| format!("{error:#}")),
            }),
            Question::Reverse { lat, lon } => Some(Reply::Reverse(
                engine
                    .reverse(lat, lon)
                    .map_err(|error| format!("{error:#}")),
            )),
            Question::Pasted(text) => {
                let (addresses, note) = batch::pasted_addresses(&text);
                // A last line without its line break may still be typed:
                // it is answered, but not kept.
                let open = (!text.ends_with('\n')).then(|| addresses.len().saturating_sub(1));
                let mut rows = Vec::with_capacity(addresses.len());
                for (index, input) in addresses.into_iter().enumerate() {
                    if !newest() {
                        break;
                    }
                    let answer = match known.get(&input) {
                        Some(answer) => answer.clone(),
                        None => {
                            let answer = engine.geocode_address(&input).ok().map(Arc::new);
                            if open != Some(index) {
                                known.insert(input.clone(), answer.clone());
                            }
                            answer
                        }
                    };
                    rows.push(PastedRow { input, answer });
                }
                newest().then_some(Reply::Pasted { rows, note })
            }
            Question::Addresses(addresses) => Some(Reply::Addresses(
                addresses
                    .iter()
                    .map(|address| {
                        engine
                            .geocode_address(address)
                            .ok()
                            .and_then(|answer| answer.hits.into_iter().next())
                    })
                    .collect(),
            )),
        };
        if let Some(reply) = reply {
            *shared.answered.lock().expect("answer lock") = Some((id, reply));
        }
        shared.settled.fetch_max(id, Ordering::Relaxed);
    }
}
