pub mod argue;
pub mod block;
pub mod block_eval;
pub mod block_update;
pub mod builder;
pub mod cli;
pub mod document;
pub mod edges;
mod eval;
pub mod execute;
pub mod filter;
mod graph_match;
pub mod project;
pub mod schemas;
pub mod scores;
pub mod search;
pub mod sort;
pub mod update;
pub mod via;
pub mod wire;

pub use argue::{
    argue, diagnose, render_diagnosis_text, render_text as render_argument_text, Argument,
    Diagnosis, Status as ArgueStatus,
};
pub use builder::{
    ParseError, build_filter_value, build_projection, build_update_doc, check_path_segments, parse_expect, parse_filter_expression, parse_filter_mapping, parse_operation,
};
pub use document::{
    BlockUpdate, BlockUpdateOp, CountCmp, CountOp, CountPred, DeleteOp, Expect, FieldOp, FieldPath, Filter, FindOp, InclusionAnchor, KeyOp, Limit, Operation, OperationKind, Projection, ProjectionBase, ProjectionField, ProjectionSource, PseudoField, ReferenceAnchor, Sort, SortDir, StandingOp, Update, UpdateOp, UpdateOperator, YamlType, is_operator_segment,
};
pub use eval::{evaluate, evaluate_within};
pub use execute::{execute, execute_with_scores, strict_guard_violations, FindMatch, Outcome};
pub use schemas::{
    current_query_schema, query_schema, query_schema_uri, CURRENT_QUERY_SCHEMA_DRAFT,
    QUERY_SCHEMA_DRAFTS,
};
pub use scores::QueryScores;
pub use search::SearchSpec;
pub use via::ViaWalk;
