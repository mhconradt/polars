use crate::logical_plan::iterator::{ArenaExprIter, ArenaLpIter};
use crate::logical_plan::Context;
use crate::prelude::*;
use ahash::RandomState;
use polars_core::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) trait PushNode {
    fn push_node(&mut self, value: Node);
}

impl PushNode for Vec<Node> {
    fn push_node(&mut self, value: Node) {
        self.push(value)
    }
}

impl PushNode for [Option<Node>; 2] {
    fn push_node(&mut self, value: Node) {
        match self {
            [None, None] => self[0] = Some(value),
            [Some(_), None] => self[1] = Some(value),
            _ => panic!("cannot push more than 2 nodes"),
        }
    }
}

impl PushNode for [Option<Node>; 1] {
    fn push_node(&mut self, value: Node) {
        match self {
            [Some(_)] => self[0] = Some(value),
            _ => panic!("cannot push more than 2 nodes"),
        }
    }
}

pub(crate) fn is_scan(plan: &ALogicalPlan) -> bool {
    match plan {
        #[cfg(feature = "csv-file")]
        ALogicalPlan::CsvScan { .. } => true,
        ALogicalPlan::DataFrameScan { .. } => true,
        #[cfg(feature = "parquet")]
        ALogicalPlan::ParquetScan { .. } => true,
        _ => false,
    }
}

impl PushNode for &mut [Option<Node>] {
    fn push_node(&mut self, value: Node) {
        if self[0].is_some() {
            self[1] = Some(value)
        } else {
            self[0] = Some(value)
        }
    }
}

pub(crate) fn has_aexpr<F>(current_node: Node, arena: &Arena<AExpr>, matches: F) -> bool
where
    F: Fn(&AExpr) -> bool,
{
    arena.iter(current_node).any(|(_node, e)| matches(e))
}

pub(crate) fn has_window_aexpr(current_node: Node, arena: &Arena<AExpr>) -> bool {
    has_aexpr(current_node, arena, |e| matches!(e, AExpr::Window { .. }))
}

/// Can check if an expression tree has a matching_expr. This
/// requires a dummy expression to be created that will be used to patter match against.
pub(crate) fn has_expr<F>(current_expr: &Expr, matches: F) -> bool
where
    F: Fn(&Expr) -> bool,
{
    current_expr.into_iter().any(matches)
}

/// Check if root expression is a literal
pub(crate) fn has_root_literal_expr(e: &Expr) -> bool {
    matches!(e.into_iter().last(), Some(Expr::Literal(_)))
}

// this one is used so much that it has its own function, to reduce inlining
pub(crate) fn has_wildcard(current_expr: &Expr) -> bool {
    has_expr(current_expr, |e| matches!(e, Expr::Wildcard))
}

/// output name of expr
pub(crate) fn output_name(expr: &Expr) -> Result<Arc<str>> {
    for e in expr {
        match e {
            // don't follow the partition by branch
            Expr::Window { function, .. } => return output_name(function),
            Expr::Column(name) => return Ok(name.clone()),
            Expr::Alias(_, name) => return Ok(name.clone()),
            _ => {}
        }
    }
    Err(PolarsError::ComputeError(
        format!(
            "No root column name could be found for expr {:?} in output name utillity",
            expr
        )
        .into(),
    ))
}

pub(crate) fn rename_field(field: &Field, name: &str) -> Field {
    Field::new(name, field.data_type().clone())
}

/// This function should be used to find the name of the start of an expression
/// Normal iteration would just return the first root column it found
pub(crate) fn get_single_root(expr: &Expr) -> Result<Arc<str>> {
    for e in expr {
        match e {
            Expr::Filter { input, .. } => return get_single_root(input),
            Expr::Take { expr, .. } => return get_single_root(expr),
            Expr::SortBy { expr, .. } => return get_single_root(expr),
            Expr::Window { function, .. } => return get_single_root(function),
            Expr::Column(name) => return Ok(name.clone()),
            _ => {}
        }
    }
    Err(PolarsError::ComputeError(
        format!("no root column found in {:?}", expr).into(),
    ))
}

/// This should gradually replace expr_to_root_column as this will get all names in the tree.
pub(crate) fn expr_to_root_column_names(expr: &Expr) -> Vec<Arc<str>> {
    expr_to_root_column_exprs(expr)
        .into_iter()
        .map(|e| expr_to_root_column_name(&e).unwrap())
        .collect()
}

/// unpack alias(col) to name of the root column name
pub(crate) fn expr_to_root_column_name(expr: &Expr) -> Result<Arc<str>> {
    let mut roots = expr_to_root_column_exprs(expr);
    match roots.len() {
        0 => Err(PolarsError::ComputeError(
            "no root column name found".into(),
        )),
        1 => match roots.pop().unwrap() {
            Expr::Wildcard => Err(PolarsError::ComputeError(
                "wildcard has not root column name".into(),
            )),
            Expr::Column(name) => Ok(name),
            _ => {
                unreachable!();
            }
        },
        _ => Err(PolarsError::ComputeError(
            "found more than one root column name".into(),
        )),
    }
}

pub(crate) fn aexpr_to_root_nodes(root: Node, arena: &Arena<AExpr>) -> Vec<Node> {
    let mut out = vec![];
    arena.iter(root).for_each(|(node, e)| match e {
        AExpr::Column(_) | AExpr::Wildcard => {
            out.push(node);
        }
        _ => {}
    });
    out
}

pub(crate) fn rename_aexpr_root_name(
    node: Node,
    arena: &mut Arena<AExpr>,
    new_name: Arc<str>,
) -> Result<()> {
    let roots = aexpr_to_root_nodes(node, arena);
    match roots.len() {
        1 => {
            let node = roots[0];
            arena.replace_with(node, |ae| match ae {
                AExpr::Column(_) => AExpr::Column(new_name),
                _ => panic!("should be only a column"),
            });
            Ok(())
        }
        _ => {
            panic!("had more than one root columns");
        }
    }
}

/// Get all root column expressions in the expression tree.
pub(crate) fn expr_to_root_column_exprs(expr: &Expr) -> Vec<Expr> {
    let mut out = vec![];
    expr.into_iter().for_each(|e| match e {
        Expr::Column(_) | Expr::Wildcard => {
            out.push(e.clone());
        }
        _ => {}
    });
    out
}

/// Take a list of expressions and a schema and determine the output schema.
pub(crate) fn expressions_to_schema(expr: &[Expr], schema: &Schema, ctxt: Context) -> Schema {
    let fields = expr
        .iter()
        .map(|expr| expr.to_field(schema, ctxt))
        .collect::<Result<Vec<_>>>()
        .unwrap();
    Schema::new(fields)
}

/// Get a set of the data source paths in this LogicalPlan
pub(crate) fn agg_source_paths(
    root_lp: Node,
    paths: &mut HashSet<PathBuf, RandomState>,
    lp_arena: &Arena<ALogicalPlan>,
) {
    lp_arena.iter(root_lp).for_each(|(_, lp)| {
        use ALogicalPlan::*;
        match lp {
            #[cfg(feature = "csv-file")]
            CsvScan { path, .. } => {
                paths.insert(path.clone());
            }
            #[cfg(feature = "parquet")]
            ParquetScan { path, .. } => {
                paths.insert(path.clone());
            }
            _ => {}
        }
    })
}

pub(crate) fn try_path_to_str(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        PolarsError::ComputeError(format!("Non-UTF8 file path: {}", path.to_string_lossy()).into())
    })
}

pub(crate) fn aexpr_to_root_names(node: Node, arena: &Arena<AExpr>) -> Vec<Arc<str>> {
    aexpr_to_root_nodes(node, arena)
        .into_iter()
        .map(|node| aexpr_to_root_column_name(node, arena).unwrap())
        .collect()
}

/// unpack alias(col) to name of the root column name
pub(crate) fn aexpr_to_root_column_name(root: Node, arena: &Arena<AExpr>) -> Result<Arc<str>> {
    let mut roots = aexpr_to_root_nodes(root, arena);
    match roots.len() {
        0 => Err(PolarsError::ComputeError(
            "no root column name found".into(),
        )),
        1 => match arena.get(roots.pop().unwrap()) {
            AExpr::Wildcard => Err(PolarsError::ComputeError(
                "wildcard has not root column name".into(),
            )),
            AExpr::Column(name) => Ok(name.clone()),
            _ => {
                unreachable!();
            }
        },
        _ => Err(PolarsError::ComputeError(
            "found more than one root column name".into(),
        )),
    }
}

/// check if a selection/projection can be done on the downwards schema
pub(crate) fn check_input_node(
    node: Node,
    input_schema: &Schema,
    expr_arena: &Arena<AExpr>,
) -> bool {
    // first determine output field, and then check if that output field could be selected
    // on the input schema.
    match expr_arena
        .get(node)
        .to_field(input_schema, Context::Default, expr_arena)
    {
        Ok(output_expr) => input_schema.field_with_name(output_expr.name()).is_ok(),
        Err(_) => false,
    }
}

pub(crate) fn aexprs_to_schema(
    expr: &[Node],
    schema: &Schema,
    ctxt: Context,
    arena: &Arena<AExpr>,
) -> Schema {
    let fields = expr
        .iter()
        .map(|expr| arena.get(*expr).to_field(schema, ctxt, arena))
        .collect::<Result<Vec<_>>>()
        .unwrap();
    Schema::new(fields)
}

pub(crate) fn combine_predicates_expr<I>(iter: I) -> Expr
where
    I: Iterator<Item = Expr>,
{
    let mut single_pred = None;
    for expr in iter {
        single_pred = match single_pred {
            None => Some(expr),
            Some(e) => Some(e.and(expr)),
        };
    }
    single_pred.expect("an empty iterator was passed")
}

#[cfg(test)]
pub(crate) mod test {
    use crate::prelude::stack_opt::{OptimizationRule, StackOptimizer};
    use crate::prelude::*;
    use polars_core::prelude::*;

    pub fn optimize_lp(lp: LogicalPlan, rules: &mut [Box<dyn OptimizationRule>]) -> LogicalPlan {
        // initialize arena's
        let mut expr_arena = Arena::with_capacity(64);
        let mut lp_arena = Arena::with_capacity(32);
        let root = to_alp(lp, &mut expr_arena, &mut lp_arena);

        let opt = StackOptimizer {};
        let lp_top = opt.optimize_loop(rules, &mut expr_arena, &mut lp_arena, root);
        node_to_lp(lp_top, &mut expr_arena, &mut lp_arena)
    }

    pub fn optimize_expr(
        expr: Expr,
        schema: Schema,
        rules: &mut [Box<dyn OptimizationRule>],
    ) -> Expr {
        // initialize arena's
        let mut expr_arena = Arena::with_capacity(64);
        let mut lp_arena = Arena::with_capacity(32);
        let schema = Arc::new(schema);

        // dummy input needed to put the schema
        let input = Box::new(LogicalPlan::Projection {
            expr: vec![],
            input: Box::new(Default::default()),
            schema: schema.clone(),
        });

        let lp = LogicalPlan::Projection {
            expr: vec![expr],
            input,
            schema,
        };

        let root = to_alp(lp, &mut expr_arena, &mut lp_arena);

        let opt = StackOptimizer {};
        let lp_top = opt.optimize_loop(rules, &mut expr_arena, &mut lp_arena, root);
        if let LogicalPlan::Projection { mut expr, .. } =
            node_to_lp(lp_top, &mut expr_arena, &mut lp_arena)
        {
            expr.pop().unwrap()
        } else {
            unreachable!()
        }
    }
}
