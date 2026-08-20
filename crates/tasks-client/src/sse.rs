//! Blocking SSE readers with reconnection built in.
//!
//! The server emits bare single-line `data: <json>` frames (no `event:`
//! names, no `id:` fields, no `Last-Event-ID` replay) plus keep-alive
//! comments. Each iterator here owns one connection, blocks its thread
//! between frames, and on a transport drop sleeps and reconnects on the
//! next `next()` call. HTTP-level errors are terminal: the server answered
//! and said no, so the iterator yields the error once and then ends.

use std::io::{BufRead, BufReader, Read};
use std::thread;
use std::time::Duration;

use tasks_api::events::Event;
use tasks_api::models::{OrchestratorFeedEvent, TranscriptLine, TranscriptOwner};

use crate::{Client, ClientError, Result, map_ureq};

/// Pause before each reconnection attempt (the first connect is immediate).
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

type Body = BufReader<Box<dyn Read + Send + Sync + 'static>>;

/// One SSE connection's frame reader.
struct Frames {
    body: Body,
}

impl Frames {
    fn open(client: &Client, path_and_query: &str) -> Result<Self> {
        let response = client
            .streams
            .get(&client.url(path_and_query))
            .call()
            .map_err(map_ureq)?;
        Ok(Self {
            body: BufReader::new(Box::new(response.into_reader())),
        })
    }

    /// The next frame's `data` payload. Accumulates multi-line `data:`
    /// fields per the SSE spec (the server sends single lines today);
    /// comments and other fields are skipped. `None` means EOF.
    fn next_data(&mut self) -> std::io::Result<Option<String>> {
        let mut data: Option<String> = None;
        let mut line = String::new();
        loop {
            line.clear();
            if self.body.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                // Frame boundary. Dataless frames (keep-alives) don't count.
                if data.is_some() {
                    return Ok(data);
                }
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                match &mut data {
                    Some(data) => {
                        data.push('\n');
                        data.push_str(rest);
                    }
                    None => data = Some(rest.to_string()),
                }
            }
        }
    }
}

/// Connection state shared by the three stream iterators.
enum Link {
    /// Never connected, or told to reconnect immediately (first attempt).
    Start,
    Up(Frames),
    /// Dropped; the next attempt sleeps [`RECONNECT_DELAY`] first.
    Down,
    /// A terminal error was yielded; the iterator is over.
    Ended,
}

impl Link {
    /// Drive toward `Up`, sleeping first when reconnecting. `Ok(frames)` to
    /// read from; `Err` when this attempt failed (state already updated).
    fn connect(&mut self, client: &Client, path_and_query: &str) -> Result<&mut Frames> {
        if matches!(self, Link::Down) {
            thread::sleep(RECONNECT_DELAY);
        }
        match Frames::open(client, path_and_query) {
            Ok(frames) => {
                *self = Link::Up(frames);
                match self {
                    Link::Up(frames) => Ok(frames),
                    _ => unreachable!(),
                }
            }
            Err(err) => {
                *self = if err.is_terminal() {
                    Link::Ended
                } else {
                    Link::Down
                };
                Err(err)
            }
        }
    }
}

/// What [`EventStream`] yields.
#[derive(Debug)]
pub enum EventStreamItem {
    /// The stream (re)connected. Snapshot state now: this fires before any
    /// event from the new connection, so everything that happened while
    /// disconnected is reflected in fetches made after it.
    Connected,
    /// Something happened. Events are invalidation signals — refetch the
    /// entity, don't fold the payload into state.
    Event(Event),
    /// The connection dropped; the iterator sleeps and retries on the next
    /// `next()`. Terminal errors end the iterator after this item.
    Disconnected(ClientError),
}

/// Reconnecting iterator over `GET /events/stream`. Never ends except on a
/// terminal (HTTP-level) error.
pub struct EventStream {
    client: Client,
    link: Link,
    last_seq: i64,
}

impl EventStream {
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client,
            link: Link::Start,
            last_seq: 0,
        }
    }

    /// Highest event seq delivered so far (0 before the first event) — the
    /// cursor for a `GET /events?since=` catch-up read, should the caller
    /// want gapless history rather than snapshot-on-`Connected`.
    pub fn last_seq(&self) -> i64 {
        self.last_seq
    }
}

impl Iterator for EventStream {
    type Item = EventStreamItem;

    fn next(&mut self) -> Option<EventStreamItem> {
        loop {
            match &mut self.link {
                Link::Ended => return None,
                Link::Start | Link::Down => {
                    match self.link.connect(&self.client, "/events/stream") {
                        Ok(_) => return Some(EventStreamItem::Connected),
                        Err(err) => return Some(EventStreamItem::Disconnected(err)),
                    }
                }
                Link::Up(frames) => match frames.next_data() {
                    Ok(Some(data)) => match serde_json::from_str::<Event>(&data) {
                        Ok(event) => {
                            // The live broadcast never replays, so a seq at
                            // or below the high water is a reconnect echo.
                            if event.seq <= self.last_seq {
                                continue;
                            }
                            self.last_seq = event.seq;
                            return Some(EventStreamItem::Event(event));
                        }
                        Err(err) => {
                            self.link = Link::Down;
                            return Some(EventStreamItem::Disconnected(err.into()));
                        }
                    },
                    Ok(None) => {
                        self.link = Link::Down;
                        return Some(EventStreamItem::Disconnected(ClientError::Transport(
                            "event stream closed".into(),
                        )));
                    }
                    Err(err) => {
                        self.link = Link::Down;
                        return Some(EventStreamItem::Disconnected(err.into()));
                    }
                },
            }
        }
    }
}

/// Gapless live tail of one run's transcript — a scout session or a build:
/// the server replays from `since` before going live, and every reconnect
/// resumes from the last delivered seq, so nothing is skipped. Yields `Err` on
/// drops (the caller's "connection lost" signal) and keeps going; ends on
/// terminal errors.
pub struct TranscriptTail {
    client: Client,
    owner: TranscriptOwner,
    /// Next seq to ask for — last delivered + 1 (`since` is inclusive).
    next_since: i64,
    link: Link,
}

impl TranscriptTail {
    pub(crate) fn new(client: Client, owner: TranscriptOwner, since: i64) -> Self {
        Self {
            client,
            owner,
            next_since: since,
            link: Link::Start,
        }
    }

    /// The route this tail reconnects to, derived from the owner rather than
    /// remembered separately: two resources, two routes, one cursor.
    fn stream_path(&self) -> String {
        let collection = match self.owner {
            TranscriptOwner::Session { .. } => "sessions",
            TranscriptOwner::Build { .. } => "builds",
            TranscriptOwner::Worker { .. } => "workers",
        };
        format!(
            "/{collection}/{}/transcript/stream?since={}",
            self.owner.id(),
            self.next_since
        )
    }
}

impl Iterator for TranscriptTail {
    type Item = Result<TranscriptLine>;

    fn next(&mut self) -> Option<Result<TranscriptLine>> {
        loop {
            match &mut self.link {
                Link::Ended => return None,
                Link::Start | Link::Down => {
                    let path = self.stream_path();
                    if let Err(err) = self.link.connect(&self.client, &path) {
                        return Some(Err(err));
                    }
                    // Connected: the replay arrives as ordinary frames.
                    continue;
                }
                Link::Up(frames) => match frames.next_data() {
                    Ok(Some(data)) => match serde_json::from_str::<TranscriptLine>(&data) {
                        Ok(line) => {
                            self.next_since = line.seq + 1;
                            return Some(Ok(line));
                        }
                        Err(err) => {
                            self.link = Link::Down;
                            return Some(Err(err.into()));
                        }
                    },
                    Ok(None) => {
                        self.link = Link::Down;
                        return Some(Err(ClientError::Transport(
                            "transcript stream closed".into(),
                        )));
                    }
                    Err(err) => {
                        self.link = Link::Down;
                        return Some(Err(err.into()));
                    }
                },
            }
        }
    }
}

/// Reconnecting iterator over the ephemeral orchestrator feed. There is no
/// backfill: after an `Err`, resync durable state via
/// `GET /orchestrator/messages` — whatever deltas were missed are already
/// part of a finished message there.
pub struct OrchestratorFeed {
    client: Client,
    link: Link,
}

impl OrchestratorFeed {
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client,
            link: Link::Start,
        }
    }
}

impl Iterator for OrchestratorFeed {
    type Item = Result<OrchestratorFeedEvent>;

    fn next(&mut self) -> Option<Result<OrchestratorFeedEvent>> {
        loop {
            match &mut self.link {
                Link::Ended => return None,
                Link::Start | Link::Down => {
                    if let Err(err) = self.link.connect(&self.client, "/orchestrator/stream") {
                        return Some(Err(err));
                    }
                    continue;
                }
                Link::Up(frames) => match frames.next_data() {
                    Ok(Some(data)) => match serde_json::from_str::<OrchestratorFeedEvent>(&data) {
                        Ok(event) => return Some(Ok(event)),
                        Err(err) => {
                            self.link = Link::Down;
                            return Some(Err(err.into()));
                        }
                    },
                    Ok(None) => {
                        self.link = Link::Down;
                        return Some(Err(ClientError::Transport(
                            "orchestrator stream closed".into(),
                        )));
                    }
                    Err(err) => {
                        self.link = Link::Down;
                        return Some(Err(err.into()));
                    }
                },
            }
        }
    }
}
