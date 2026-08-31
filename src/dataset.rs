//! `dataset('sales.tdy.sql')` — the members, as one relation.
//!
//! Conformance is what makes this simple. Every member has been proved to
//! produce the target's schema exactly, so the union is a concatenation:
//! there is nothing to coerce, nothing to widen, and no chance of DataFusion
//! quietly turning an Int64 and a Utf8 branch into Utf8 the way an ordinary
//! `UNION ALL` would.
//!
//! # One partition, in lock order
//!
//! Members are read sequentially in the order the lock records them, in a
//! single partition. Splitting them across partitions would let DataFusion
//! emit whichever finished first, so `SELECT *` over the same twelve files
//! would return its rows in a different order on every run and `--frozen`
//! would stop meaning "same files, same answer". Streaming makes the single
//! partition cheap: peak memory follows the batch, not the dataset.
//!
//! # What it refuses to do
//!
//! It never plans, never expands a glob, and never writes. A query is a
//! question, not a job that can spend money or mutate committed files. If the
//! lock is stale or a member has drifted, `dataset()` fails at planning time
//! with the file named — and `tdy fit` is what settles it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::TableProvider;
use datafusion::common::{DataFusionError, Result as DfResult};
use datafusion::datasource::TableType;
use datafusion::execution::TaskContext;
use datafusion::logical_expr::{Expr, TableProviderFilterPushDown};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::streaming::{PartitionStream, StreamingTableExec};
use datafusion::physical_plan::{ExecutionPlan, SendableRecordBatchStream};

use crate::config::Limits;
use crate::conform::conforms;
use crate::lockfile::{self, Lock};
use crate::spec::ParseSpec;
use crate::target::Target;

/// One member, resolved and proved, ready to read.
#[derive(Debug, Clone)]
pub struct ResolvedMember {
    pub path: PathBuf,
    /// Relative, for messages.
    pub rel: String,
    pub spec: Arc<ParseSpec>,
}

/// Everything `dataset()` needs, with every check already done.
pub struct Resolved {
    pub target: Target,
    pub schema: SchemaRef,
    pub members: Vec<ResolvedMember>,
}

/// Load a dataset and prove it is ready to query.
///
/// Every failure here is one a query must not paper over, so each is an error
/// naming the file and the fix rather than a member quietly dropped.
pub fn resolve(target_file: &Path, limits: Limits) -> Result<Resolved> {
    let target = Target::load(target_file)?;
    let schema: SchemaRef = Arc::new(target.arrow_schema());

    let lock = Lock::load(target_file)?.ok_or_else(|| {
        anyhow::anyhow!(
            "{} has no lock — run `tdy fit {}` to plan its members",
            target_file.display(),
            target_file.display()
        )
    })?;

    let drifts = lockfile::drift(&lock, &target, target_file)?;
    if !drifts.is_empty() {
        let mut msg = format!("dataset `{}` is out of date:", target.name);
        for d in &drifts {
            msg.push_str(&format!("\n  {}", d.message()));
        }
        anyhow::bail!("{msg}");
    }

    if lock.members.is_empty() {
        anyhow::bail!(
            "dataset `{}` has no members; its globs matched nothing",
            target.name
        );
    }

    let dir = lockfile::target_dir(target_file);
    let mut members = Vec::with_capacity(lock.members.len());
    for m in &lock.members {
        let path = dir.join(&m.path);
        // The sidecar must be present *and* fresh: a stale one is a spec no
        // query would use, and this is the one place that cannot re-plan.
        let spec = match crate::sidecar::load(&path)? {
            crate::sidecar::SidecarStatus::Fresh(sc) => sc.spec,
            crate::sidecar::SidecarStatus::Stale(_) => anyhow::bail!(
                "{} has changed since it was fitted — run `tdy fit {}`",
                m.path,
                target_file.display()
            ),
            crate::sidecar::SidecarStatus::Absent => anyhow::bail!(
                "{} is a member of `{}` but has no spec — run `tdy fit {}`",
                m.path,
                target.name,
                target_file.display()
            ),
        };
        // Re-proved on every load, because a sidecar is hand-editable and
        // therefore untrusted input. The check costs no I/O.
        if let Err(mismatches) = conforms(&spec, &target) {
            let mut msg = format!("{} no longer produces `{}`:", m.path, target.name);
            for x in &mismatches {
                msg.push_str(&format!("\n  {}", x.message()));
            }
            anyhow::bail!("{msg}");
        }
        members.push(ResolvedMember {
            path,
            rel: m.path.clone(),
            spec: Arc::new(spec),
        });
    }

    let _ = limits;
    Ok(Resolved { target, schema, members })
}

/// A `TableProvider` over the members, read in lock order.
#[derive(Debug)]
pub struct DatasetTable {
    schema: SchemaRef,
    partition: Arc<MembersPartition>,
}

impl DatasetTable {
    pub fn new(resolved: Resolved, limits: Limits) -> Self {
        let schema = resolved.schema.clone();
        DatasetTable {
            partition: Arc::new(MembersPartition {
                schema: schema.clone(),
                members: resolved.members,
                limits,
            }),
            schema,
        }
    }
}

#[async_trait]
impl TableProvider for DatasetTable {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
    fn table_type(&self) -> TableType {
        TableType::Base
    }
    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> DfResult<Vec<TableProviderFilterPushDown>> {
        Ok(vec![TableProviderFilterPushDown::Unsupported; filters.len()])
    }

    async fn scan(
        &self,
        _state: &dyn datafusion::catalog::Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        limit: Option<usize>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(StreamingTableExec::try_new(
            self.schema.clone(),
            vec![self.partition.clone() as Arc<dyn PartitionStream>],
            projection,
            None,
            false,
            limit,
        )?))
    }
}

/// The members, streamed one after another.
#[derive(Debug)]
struct MembersPartition {
    schema: SchemaRef,
    members: Vec<ResolvedMember>,
    limits: Limits,
}

impl PartitionStream for MembersPartition {
    fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    fn execute(&self, _ctx: Arc<TaskContext>) -> SendableRecordBatchStream {
        let (tx, rx) = tokio::sync::mpsc::channel::<DfResult<RecordBatch>>(2);
        let members = self.members.clone();
        let limits = self.limits;
        let schema = self.schema.clone();

        tokio::task::spawn_blocking(move || {
            for m in &members {
                let send = |msg: DfResult<RecordBatch>| tx.blocking_send(msg).is_ok();
                let mut alive = true;
                let result = crate::stream::enabled()
                    .then(|| crate::stream::can_stream(&m.spec))
                    .unwrap_or(false)
                    .then(|| {
                        crate::stream::execute_with(&m.spec, &m.path, limits, |b| {
                            if send(Ok(b)) {
                                Ok(())
                            } else {
                                alive = false;
                                anyhow::bail!("__tdy_receiver_closed")
                            }
                        })
                    })
                    .unwrap_or_else(|| {
                        crate::engine::execute_batches(&m.spec, &m.path, limits).map(|bs| {
                            for b in bs {
                                if !send(Ok(b)) {
                                    alive = false;
                                    break;
                                }
                            }
                        })
                    });

                if let Err(e) = result {
                    let msg = format!("{e:#}");
                    if !msg.contains("__tdy_receiver_closed") {
                        // A member that fails mid-dataset must fail the query.
                        // Swallowing it would return the members read so far —
                        // a total quietly short by a month.
                        let _ = tx.blocking_send(Err(DataFusionError::External(
                            format!("reading member {}: {msg}", m.rel).into(),
                        )));
                    }
                    return;
                }
                if !alive {
                    return;
                }
            }
        });

        let body = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        Box::pin(RecordBatchStreamAdapter::new(schema, body))
    }
}

/// Load a dataset for `dataset('path')` in SQL.
pub fn provider(target_file: &Path, limits: Limits) -> Result<Arc<dyn TableProvider>> {
    let resolved = resolve(target_file, limits)
        .with_context(|| format!("dataset({})", target_file.display()))?;
    Ok(Arc::new(DatasetTable::new(resolved, limits)))
}
