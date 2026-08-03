//! Bounded background scheduling for document diagnostics.
//! The scheduler serializes diagnostic computation and keeps only the newest pending snapshot for
//! each URI so rapid edits do not create an unbounded task or memory backlog.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tower_lsp::Client;
use tower_lsp::lsp_types::Url;
use tracing::{debug, instrument, warn};

use crate::diagnostics::{self, DiagnosticSnapshot};
use crate::schema::SchemaDoc;

pub struct DiagnosticScheduler {
    state: Arc<Mutex<SchedulerState>>,
    notify: Arc<Notify>,
}

pub struct DiagnosticTicket {
    uri: Url,
    generation: u64,
}

impl DiagnosticScheduler {
    pub fn new(client: Client) -> Self {
        let state = Arc::new(Mutex::new(SchedulerState::default()));
        let notify = Arc::new(Notify::new());

        tokio::spawn(run_worker(client, Arc::clone(&state), Arc::clone(&notify)));

        Self { state, notify }
    }

    #[instrument(skip(self), fields(uri = %uri))]
    pub async fn begin(&self, uri: Url) -> DiagnosticTicket {
        let generation = self.state.lock().await.begin(uri.clone());
        debug!(generation, "started document diagnostics generation");
        DiagnosticTicket { uri, generation }
    }

    #[instrument(skip(self, ticket, snapshot, schema), fields(uri = %ticket.uri, generation = ticket.generation))]
    pub async fn schedule(
        &self,
        ticket: DiagnosticTicket,
        snapshot: DiagnosticSnapshot,
        schema: Option<(String, Arc<SchemaDoc>)>,
    ) {
        let scheduled = self.state.lock().await.schedule(&ticket, snapshot, schema);
        if scheduled {
            self.notify.notify_one();
            debug!("scheduled document diagnostics");
        } else {
            debug!("discarding snapshot for stale diagnostics generation");
        }
    }

    #[instrument(skip(self), fields(uri = %uri))]
    pub async fn invalidate(&self, uri: &Url) {
        self.state.lock().await.invalidate(uri);
        debug!("invalidated document diagnostics");
    }
}

#[derive(Default)]
struct SchedulerState {
    next_generation: u64,
    current_generations: HashMap<Url, u64>,
    pending_jobs: HashMap<Url, DiagnosticJob>,
    pending_order: VecDeque<Url>,
}

impl SchedulerState {
    fn begin(&mut self, uri: Url) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = self.next_generation;
        self.current_generations.insert(uri.clone(), generation);
        self.pending_jobs.remove(&uri);
        self.pending_order.retain(|pending_uri| pending_uri != &uri);
        generation
    }

    fn schedule(
        &mut self,
        ticket: &DiagnosticTicket,
        snapshot: DiagnosticSnapshot,
        schema: Option<(String, Arc<SchemaDoc>)>,
    ) -> bool {
        if !self.is_current(&ticket.uri, ticket.generation) {
            return false;
        }

        let job = DiagnosticJob {
            uri: ticket.uri.clone(),
            generation: ticket.generation,
            snapshot,
            schema,
        };
        if self.pending_jobs.insert(ticket.uri.clone(), job).is_none() {
            self.pending_order.push_back(ticket.uri.clone());
        }

        true
    }

    fn take_next_job(&mut self) -> Option<DiagnosticJob> {
        while let Some(uri) = self.pending_order.pop_front() {
            if let Some(job) = self.pending_jobs.remove(&uri) {
                return Some(job);
            }
        }
        None
    }

    fn is_current(&self, uri: &Url, generation: u64) -> bool {
        self.current_generations.get(uri) == Some(&generation)
    }

    fn invalidate(&mut self, uri: &Url) {
        self.current_generations.remove(uri);
        self.pending_jobs.remove(uri);
        self.pending_order.retain(|pending_uri| pending_uri != uri);
    }
}

struct DiagnosticJob {
    uri: Url,
    generation: u64,
    snapshot: DiagnosticSnapshot,
    schema: Option<(String, Arc<SchemaDoc>)>,
}

async fn run_worker(client: Client, state: Arc<Mutex<SchedulerState>>, notify: Arc<Notify>) {
    loop {
        let next_job = state.lock().await.take_next_job();
        let Some(job) = next_job else {
            notify.notified().await;
            continue;
        };

        if !state.lock().await.is_current(&job.uri, job.generation) {
            debug!(uri = %job.uri, generation = job.generation, "skipping stale diagnostics job");
            continue;
        }

        let uri = job.uri.clone();
        let generation = job.generation;
        let diagnostics = match tokio::task::spawn_blocking(move || collect(job)).await {
            Ok(diagnostics) => diagnostics,
            Err(error) => {
                warn!(uri = %uri, generation, %error, "diagnostics job failed");
                continue;
            }
        };

        let state = state.lock().await;
        if !state.is_current(&uri, generation) {
            debug!(uri = %uri, generation, "discarding stale diagnostics result");
            continue;
        }

        debug!(uri = %uri, generation, diagnostic_count = diagnostics.len(), "publishing background diagnostics");
        client.publish_diagnostics(uri, diagnostics, None).await;
    }
}

fn collect(job: DiagnosticJob) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let Some((schema_name, schema)) = job.schema else {
        return Vec::new();
    };

    diagnostics::collect_with_schema_name(&job.snapshot, &schema, Some(&schema_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    fn uri(name: &str) -> Url {
        Url::parse(&format!("file:///tmp/{name}.ifc")).expect("test URI should be valid")
    }

    fn snapshot() -> DiagnosticSnapshot {
        DiagnosticSnapshot::from_document(&Document::new_unloaded(String::new()))
    }

    fn begin(state: &mut SchedulerState, uri: Url) -> DiagnosticTicket {
        let generation = state.begin(uri.clone());
        DiagnosticTicket { uri, generation }
    }

    fn schedule(state: &mut SchedulerState, ticket: &DiagnosticTicket) -> bool {
        state.schedule(ticket, snapshot(), None)
    }

    #[test]
    fn replaces_pending_job_for_same_uri() {
        let mut state = SchedulerState::default();
        let uri = uri("model");

        let first = begin(&mut state, uri.clone());
        assert!(schedule(&mut state, &first));
        let second = begin(&mut state, uri.clone());
        assert!(schedule(&mut state, &second));
        let job = state.take_next_job().expect("latest job should be pending");

        assert!(second.generation > first.generation);
        assert_eq!(job.generation, second.generation);
        assert!(state.take_next_job().is_none());
    }

    #[test]
    fn keeps_fifo_order_across_documents() {
        let mut state = SchedulerState::default();
        let first_uri = uri("first");
        let second_uri = uri("second");

        let first = begin(&mut state, first_uri.clone());
        let second = begin(&mut state, second_uri.clone());
        assert!(schedule(&mut state, &first));
        assert!(schedule(&mut state, &second));

        assert_eq!(state.take_next_job().map(|job| job.uri), Some(first_uri));
        assert_eq!(state.take_next_job().map(|job| job.uri), Some(second_uri));
    }

    #[test]
    fn requeues_document_behind_existing_pending_work() {
        let mut state = SchedulerState::default();
        let first_uri = uri("first");
        let second_uri = uri("second");

        let first = begin(&mut state, first_uri.clone());
        assert!(schedule(&mut state, &first));
        let first_job = state.take_next_job().expect("first job should be pending");
        let second = begin(&mut state, second_uri.clone());
        let first_again = begin(&mut state, first_uri.clone());
        assert!(schedule(&mut state, &second));
        assert!(schedule(&mut state, &first_again));

        assert_eq!(first_job.uri, first_uri);
        assert_eq!(state.take_next_job().map(|job| job.uri), Some(second_uri));
        assert_eq!(state.take_next_job().map(|job| job.uri), Some(first_uri));
    }

    #[test]
    fn invalidation_discards_pending_and_running_generations() {
        let mut state = SchedulerState::default();
        let uri = uri("model");
        let ticket = begin(&mut state, uri.clone());
        assert!(schedule(&mut state, &ticket));

        state.invalidate(&uri);

        assert!(!state.is_current(&uri, ticket.generation));
        assert!(state.take_next_job().is_none());
    }

    #[test]
    fn reopened_document_gets_a_new_generation() {
        let mut state = SchedulerState::default();
        let uri = uri("model");
        let previous = begin(&mut state, uri.clone());
        assert!(schedule(&mut state, &previous));
        state.invalidate(&uri);
        let reopened = begin(&mut state, uri.clone());
        assert!(schedule(&mut state, &reopened));

        assert_ne!(previous.generation, reopened.generation);
        assert!(!state.is_current(&uri, previous.generation));
        assert!(state.is_current(&uri, reopened.generation));
        assert_eq!(
            state.take_next_job().map(|job| job.generation),
            Some(reopened.generation)
        );
    }

    #[test]
    fn rejects_snapshot_from_overlapped_older_handler() {
        let mut state = SchedulerState::default();
        let uri = uri("model");
        let older = begin(&mut state, uri.clone());
        let newer = begin(&mut state, uri);

        assert!(!schedule(&mut state, &older));
        assert!(schedule(&mut state, &newer));
        assert_eq!(
            state.take_next_job().map(|job| job.generation),
            Some(newer.generation)
        );
    }
}
