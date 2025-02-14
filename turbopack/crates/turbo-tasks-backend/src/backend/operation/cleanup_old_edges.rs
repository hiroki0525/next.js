use std::mem::take;

use serde::{Deserialize, Serialize};
use turbo_tasks::TaskId;

use crate::{
    backend::{
        operation::{
            aggregation_update::{
                get_aggregation_number, get_uppers, is_aggregating_node, AggregationUpdateJob,
                AggregationUpdateQueue,
            },
            invalidate::make_task_dirty,
            AggregatedDataUpdate, ExecuteContext, Operation,
        },
        storage::update_count,
        TaskDataCategory,
    },
    data::{CachedDataItemKey, CellRef, CollectibleRef, CollectiblesRef},
};

#[derive(Serialize, Deserialize, Clone, Default)]
pub enum CleanupOldEdgesOperation {
    RemoveEdges {
        task_id: TaskId,
        outdated: Vec<OutdatedEdge>,
        queue: AggregationUpdateQueue,
    },
    AggregationUpdate {
        queue: AggregationUpdateQueue,
    },
    #[default]
    Done,
    // TODO Add aggregated edge
}

#[derive(Serialize, Deserialize, Clone)]
pub enum OutdatedEdge {
    Child(TaskId),
    Collectible(CollectibleRef, i32),
    CellDependency(CellRef),
    OutputDependency(TaskId),
    CollectiblesDependency(CollectiblesRef),
    RemovedCellDependent(TaskId),
}

impl CleanupOldEdgesOperation {
    pub fn run(task_id: TaskId, outdated: Vec<OutdatedEdge>, ctx: &mut ExecuteContext<'_>) {
        let queue = AggregationUpdateQueue::new();
        CleanupOldEdgesOperation::RemoveEdges {
            task_id,
            outdated,
            queue,
        }
        .execute(ctx);
    }
}

impl Operation for CleanupOldEdgesOperation {
    fn execute(mut self, ctx: &mut ExecuteContext<'_>) {
        loop {
            ctx.operation_suspend_point(&self);
            match self {
                CleanupOldEdgesOperation::RemoveEdges {
                    task_id,
                    ref mut outdated,
                    ref mut queue,
                } => {
                    if let Some(edge) = outdated.pop() {
                        match edge {
                            OutdatedEdge::Child(child_id) => {
                                let mut children = Vec::new();
                                children.push(child_id);
                                outdated.retain(|e| match e {
                                    OutdatedEdge::Child(id) => {
                                        children.push(*id);
                                        false
                                    }
                                    _ => true,
                                });
                                let mut task = ctx.task(task_id, TaskDataCategory::All);
                                for &child_id in children.iter() {
                                    task.remove(&CachedDataItemKey::Child { task: child_id });
                                }
                                if is_aggregating_node(get_aggregation_number(&task)) {
                                    queue.push(AggregationUpdateJob::InnerLostFollowers {
                                        upper_ids: vec![task_id],
                                        lost_follower_ids: children,
                                    });
                                } else {
                                    let upper_ids = get_uppers(&task);
                                    queue.push(AggregationUpdateJob::InnerLostFollowers {
                                        upper_ids,
                                        lost_follower_ids: children,
                                    });
                                }
                            }
                            OutdatedEdge::Collectible(collectible, count) => {
                                let mut collectibles = Vec::new();
                                collectibles.push((collectible, count));
                                outdated.retain(|e| match e {
                                    OutdatedEdge::Collectible(collectible, count) => {
                                        collectibles.push((*collectible, -*count));
                                        false
                                    }
                                    _ => true,
                                });
                                let mut task = ctx.task(task_id, TaskDataCategory::All);
                                for &(collectible, count) in collectibles.iter() {
                                    update_count!(task, Collectible { collectible }, -count);
                                }
                                queue.extend(AggregationUpdateJob::data_update(
                                    &mut task,
                                    AggregatedDataUpdate::new().collectibles_update(collectibles),
                                ));
                            }
                            OutdatedEdge::CellDependency(CellRef {
                                task: cell_task_id,
                                cell,
                            }) => {
                                {
                                    let mut task = ctx.task(cell_task_id, TaskDataCategory::Data);
                                    task.remove(&CachedDataItemKey::CellDependent {
                                        cell,
                                        task: task_id,
                                    });
                                }
                                {
                                    let mut task = ctx.task(task_id, TaskDataCategory::Data);
                                    task.remove(&CachedDataItemKey::CellDependency {
                                        target: CellRef {
                                            task: cell_task_id,
                                            cell,
                                        },
                                    });
                                }
                            }
                            OutdatedEdge::OutputDependency(output_task_id) => {
                                {
                                    let mut task = ctx.task(output_task_id, TaskDataCategory::Data);
                                    task.remove(&CachedDataItemKey::OutputDependent {
                                        task: task_id,
                                    });
                                }
                                {
                                    let mut task = ctx.task(task_id, TaskDataCategory::Data);
                                    task.remove(&CachedDataItemKey::OutputDependency {
                                        target: output_task_id,
                                    });
                                }
                            }
                            OutdatedEdge::CollectiblesDependency(CollectiblesRef {
                                collectible_type,
                                task: dependent_task_id,
                            }) => {
                                {
                                    let mut task =
                                        ctx.task(dependent_task_id, TaskDataCategory::Data);
                                    task.remove(&CachedDataItemKey::CollectiblesDependent {
                                        collectible_type,
                                        task: task_id,
                                    });
                                }
                                {
                                    let mut task = ctx.task(task_id, TaskDataCategory::Data);
                                    task.remove(&CachedDataItemKey::CollectiblesDependency {
                                        target: CollectiblesRef {
                                            collectible_type,
                                            task: dependent_task_id,
                                        },
                                    });
                                }
                            }
                            OutdatedEdge::RemovedCellDependent(task_id) => {
                                make_task_dirty(task_id, queue, ctx);
                            }
                        }
                    }

                    if outdated.is_empty() {
                        self = CleanupOldEdgesOperation::AggregationUpdate { queue: take(queue) };
                    }
                }
                CleanupOldEdgesOperation::AggregationUpdate { ref mut queue } => {
                    if queue.process(ctx) {
                        self = CleanupOldEdgesOperation::Done;
                    }
                }
                CleanupOldEdgesOperation::Done => {
                    return;
                }
            }
        }
    }
}
