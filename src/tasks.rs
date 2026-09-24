//! Cancellable preparation, keeping parsing and layout off the UI thread.

use crate::{
    export::AssemblyStats,
    filter::FilterParams,
    gfa::GfaGraph,
    graph::ViewGraph,
    layout::{Layout, LayoutBackend, LayoutParams, LayoutRunner},
    session::Session,
};
use anyhow::{Result, ensure};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use std::time::Duration;

pub enum LoadRequest {
    File(PathBuf),
    Session(PathBuf),
    Rebuild { gfa: Arc<GfaGraph>, source: PathBuf },
}

pub struct PreparedGraph {
    pub filter: FilterParams,
    pub source: PathBuf,
    pub gfa: Arc<GfaGraph>,
    pub stats: AssemblyStats,
    pub view: Arc<ViewGraph>,
    pub runner: LayoutRunner,
    pub snapshot: Layout,
    pub session: Option<Session>,
}

pub struct LoadJob {
    handle: Option<std::thread::JoinHandle<Result<PreparedGraph>>>,
    cancel: Arc<AtomicBool>,
    stage: Arc<AtomicU8>,
}

impl LoadJob {
    pub fn spawn(
        request: LoadRequest,
        filter: FilterParams,
        backend: LayoutBackend,
        interval: Duration,
        strict: bool,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let stage = Arc::new(AtomicU8::new(0));
        let worker_cancel = cancel.clone();
        let worker_stage = stage.clone();
        let handle = std::thread::spawn(move || {
            let cancel = &worker_cancel;
            let stage = &worker_stage;
            let check = || -> Result<()> {
                ensure!(!cancel.load(Ordering::Relaxed), "Loading cancelled");
                Ok(())
            };
            let strict = strict && !matches!(&request, LoadRequest::Rebuild { .. });
            let (gfa, source, session) = match request {
                LoadRequest::File(path) => {
                    let source = path.canonicalize()?;
                    let gfa = Arc::new(crate::gfa::parse_gfa_with_control(&source, cancel)?);
                    (gfa, source, None)
                }
                LoadRequest::Session(path) => {
                    let session = Session::read(&path)?;
                    let source = session.source.canonicalize()?;
                    let gfa = Arc::new(crate::gfa::parse_gfa_with_control(&source, cancel)?);
                    (gfa, source, Some(session))
                }
                LoadRequest::Rebuild { gfa, source } => (gfa, source, None),
            };
            check()?;
            ensure!(
                !strict || gfa.diagnostics.is_empty(),
                "Strict parsing rejected {} warning(s). Open with strict parsing disabled to inspect diagnostics.",
                gfa.diagnostics.len()
            );
            let filter = session.as_ref().map(|s| s.filter.clone()).unwrap_or(filter);
            stage.store(1, Ordering::Relaxed);
            let stats = AssemblyStats::compute(&gfa);
            check()?;
            stage.store(2, Ordering::Relaxed);
            let view = Arc::new(ViewGraph::from_gfa(&gfa, &filter));
            check()?;
            stage.store(3, Ordering::Relaxed);
            let mut snapshot = if let Some(session) = &session {
                Layout::try_from_positions(&view, &session.positions, cancel)?
            } else {
                Layout::try_new_with_graph_backend(&view, backend, cancel)?
            };
            if let Some(session) = &session {
                stage.store(4, Ordering::Relaxed);
                session.validate_graph(&gfa, &view, &snapshot)?;
                snapshot.restore_positions(&session.positions)?;
            }
            check()?;
            let runner = LayoutRunner::from_layout(
                view.clone(),
                LayoutParams::default(),
                snapshot.clone(),
                interval,
            );
            Ok(PreparedGraph {
                filter,
                source,
                gfa,
                stats,
                view,
                runner,
                snapshot,
                session,
            })
        });
        Self {
            handle: Some(handle),
            cancel,
            stage,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }
    pub fn finish(mut self) -> Result<PreparedGraph> {
        self.handle
            .take()
            .expect("load job handle")
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("Graph loading worker panicked")))
    }
    pub fn stage(&self) -> &'static str {
        match self.stage.load(Ordering::Relaxed) {
            0 => "Reading and parsing graph…",
            1 => "Calculating assembly statistics…",
            2 => "Filtering and finding components…",
            3 => "Computing layout and packing components…",
            _ => "Verifying saved session…",
        }
    }
}

impl Drop for LoadJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
