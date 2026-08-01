//! Workflows — durable, multi-step orchestration. A workflow is an ordered list
//! of steps (each invokes a function); a run executes them sequentially with
//! per-step state persisted so progress is observable and resumable.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub name: String,
    pub deployment: String,
    /// Function route to invoke for this step.
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowDef {
    pub id: String,
    pub name: String,
    /// Project this workflow belongs to (drives multi-tenant scoping).
    #[serde(default)]
    pub project: String,
    pub steps: Vec<WorkflowStep>,
    /// When ingested from a Vercel WDK `manifest.json`, the workflow's React-Flow
    /// graph (`{ nodes, edges }`) so the dashboard can draw it on the canvas
    /// exactly as the WDK defined it. `None` for engine-defined workflows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Queued but not started.
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepRun {
    pub name: String,
    pub status: RunStatus,
    pub output: String,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: String,
    pub def_id: String,
    /// Workflow name + project, denormalized so listing a run is self-contained.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub project: String,
    pub status: RunStatus,
    pub steps: Vec<StepRun>,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
}

/// Invokes one step, returning its output (or an error to fail the run).
pub type StepInvoker = Arc<
    dyn Fn(WorkflowStep) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>>
        + Send
        + Sync,
>;

pub struct WorkflowEngine {
    defs: RwLock<HashMap<String, WorkflowDef>>,
    runs: RwLock<HashMap<String, WorkflowRun>>,
}

impl WorkflowEngine {
    pub fn new() -> Arc<WorkflowEngine> {
        Arc::new(WorkflowEngine {
            defs: RwLock::new(HashMap::new()),
            runs: RwLock::new(HashMap::new()),
        })
    }

    pub fn define(&self, def: WorkflowDef) {
        self.defs.write().insert(def.id.clone(), def);
    }

    pub fn defs(&self) -> Vec<WorkflowDef> {
        self.defs.read().values().cloned().collect()
    }

    pub fn runs(&self) -> Vec<WorkflowRun> {
        let mut v: Vec<WorkflowRun> = self.runs.read().values().cloned().collect();
        v.sort_by(|a, b| b.started_ms.cmp(&a.started_ms));
        v
    }

    pub fn run(&self, id: &str) -> Option<WorkflowRun> {
        self.runs.read().get(id).cloned()
    }

    /// Start a run; steps execute sequentially in a background task. Returns the
    /// run id immediately.
    pub fn start(self: &Arc<Self>, def_id: &str, invoke: StepInvoker) -> anyhow::Result<String> {
        let def = self
            .defs
            .read()
            .get(def_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such workflow '{def_id}'"))?;
        let run_id = format!(
            "wrun_{}",
            Uuid::new_v4().simple().to_string()[..26].to_uppercase()
        );
        let run = WorkflowRun {
            id: run_id.clone(),
            def_id: def.id.clone(),
            name: def.name.clone(),
            project: def.project.clone(),
            status: RunStatus::Running,
            steps: Vec::new(),
            started_ms: now_ms(),
            finished_ms: None,
        };
        self.runs.write().insert(run_id.clone(), run);

        let engine = self.clone();
        let rid = run_id.clone();
        tokio::spawn(async move {
            let mut all_ok = true;
            for step in &def.steps {
                {
                    let mut runs = engine.runs.write();
                    if let Some(r) = runs.get_mut(&rid) {
                        r.steps.push(StepRun {
                            name: step.name.clone(),
                            status: RunStatus::Running,
                            output: String::new(),
                            started_ms: now_ms(),
                            finished_ms: None,
                        });
                    }
                }
                let result = invoke(step.clone()).await;
                let mut runs = engine.runs.write();
                if let Some(r) = runs.get_mut(&rid) {
                    if let Some(sr) = r.steps.last_mut() {
                        sr.finished_ms = Some(now_ms());
                        match &result {
                            Ok(out) => {
                                sr.status = RunStatus::Succeeded;
                                sr.output = out.chars().take(2000).collect();
                            }
                            Err(e) => {
                                sr.status = RunStatus::Failed;
                                sr.output = e.to_string();
                            }
                        }
                    }
                }
                if result.is_err() {
                    all_ok = false;
                    break;
                }
            }
            let mut runs = engine.runs.write();
            if let Some(r) = runs.get_mut(&rid) {
                r.status = if all_ok {
                    RunStatus::Succeeded
                } else {
                    RunStatus::Failed
                };
                r.finished_ms = Some(now_ms());
            }
        });
        Ok(run_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_steps_in_order() {
        let eng = WorkflowEngine::new();
        eng.define(WorkflowDef {
            id: "wf".into(),
            name: "demo".into(),
            project: "demo-project".into(),
            steps: vec![
                WorkflowStep {
                    name: "a".into(),
                    deployment: "d".into(),
                    path: "/a".into(),
                },
                WorkflowStep {
                    name: "b".into(),
                    deployment: "d".into(),
                    path: "/b".into(),
                },
            ],
            graph: None,
        });
        let invoker: StepInvoker = Arc::new(|step: WorkflowStep| {
            Box::pin(async move { Ok(format!("ran {}", step.name)) })
        });
        let rid = eng.start("wf", invoker).unwrap();
        // Let the background task finish.
        for _ in 0..50 {
            if eng.run(&rid).map(|r| r.status) == Some(RunStatus::Succeeded) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let run = eng.run(&rid).unwrap();
        assert_eq!(run.status, RunStatus::Succeeded);
        assert_eq!(run.steps.len(), 2);
        assert_eq!(run.steps[0].output, "ran a");
    }
}
