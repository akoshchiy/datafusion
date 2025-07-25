// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

mod kernels;

use crate::expressions::binary::kernels::concat_elements_utf8view;
use crate::intervals::cp_solver::{propagate_arithmetic, propagate_comparison};
use crate::PhysicalExpr;
use std::hash::Hash;
use std::{any::Any, sync::Arc};

use arrow::array::*;
use arrow::compute::kernels::boolean::{and_kleene, not, or_kleene};
use arrow::compute::kernels::cmp::*;
use arrow::compute::kernels::comparison::{regexp_is_match, regexp_is_match_scalar};
use arrow::compute::kernels::concat_elements::concat_elements_utf8;
use arrow::compute::{
    cast, filter_record_batch, ilike, like, nilike, nlike, SlicesIterator,
};
use arrow::datatypes::*;
use arrow::error::ArrowError;
use datafusion_common::cast::as_boolean_array;
use datafusion_common::{internal_err, not_impl_err, Result, ScalarValue};
use datafusion_expr::binary::BinaryTypeCoercer;
use datafusion_expr::interval_arithmetic::{apply_operator, Interval};
use datafusion_expr::sort_properties::ExprProperties;
use datafusion_expr::statistics::Distribution::{Bernoulli, Gaussian};
use datafusion_expr::statistics::{
    combine_bernoullis, combine_gaussians, create_bernoulli_from_comparison,
    new_generic_from_binary_op, Distribution,
};
use datafusion_expr::{ColumnarValue, Operator};
use datafusion_physical_expr_common::datum::{apply, apply_cmp, apply_cmp_for_nested};

use kernels::{
    bitwise_and_dyn, bitwise_and_dyn_scalar, bitwise_or_dyn, bitwise_or_dyn_scalar,
    bitwise_shift_left_dyn, bitwise_shift_left_dyn_scalar, bitwise_shift_right_dyn,
    bitwise_shift_right_dyn_scalar, bitwise_xor_dyn, bitwise_xor_dyn_scalar,
};

/// Binary expression
#[derive(Debug, Clone, Eq)]
pub struct BinaryExpr {
    left: Arc<dyn PhysicalExpr>,
    op: Operator,
    right: Arc<dyn PhysicalExpr>,
    /// Specifies whether an error is returned on overflow or not
    fail_on_overflow: bool,
}

// Manually derive PartialEq and Hash to work around https://github.com/rust-lang/rust/issues/78808
impl PartialEq for BinaryExpr {
    fn eq(&self, other: &Self) -> bool {
        self.left.eq(&other.left)
            && self.op.eq(&other.op)
            && self.right.eq(&other.right)
            && self.fail_on_overflow.eq(&other.fail_on_overflow)
    }
}
impl Hash for BinaryExpr {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.left.hash(state);
        self.op.hash(state);
        self.right.hash(state);
        self.fail_on_overflow.hash(state);
    }
}

impl BinaryExpr {
    /// Create new binary expression
    pub fn new(
        left: Arc<dyn PhysicalExpr>,
        op: Operator,
        right: Arc<dyn PhysicalExpr>,
    ) -> Self {
        Self {
            left,
            op,
            right,
            fail_on_overflow: false,
        }
    }

    /// Create new binary expression with explicit fail_on_overflow value
    pub fn with_fail_on_overflow(self, fail_on_overflow: bool) -> Self {
        Self {
            left: self.left,
            op: self.op,
            right: self.right,
            fail_on_overflow,
        }
    }

    /// Get the left side of the binary expression
    pub fn left(&self) -> &Arc<dyn PhysicalExpr> {
        &self.left
    }

    /// Get the right side of the binary expression
    pub fn right(&self) -> &Arc<dyn PhysicalExpr> {
        &self.right
    }

    /// Get the operator for this binary expression
    pub fn op(&self) -> &Operator {
        &self.op
    }
}

impl std::fmt::Display for BinaryExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // Put parentheses around child binary expressions so that we can see the difference
        // between `(a OR b) AND c` and `a OR (b AND c)`. We only insert parentheses when needed,
        // based on operator precedence. For example, `(a AND b) OR c` and `a AND b OR c` are
        // equivalent and the parentheses are not necessary.

        fn write_child(
            f: &mut std::fmt::Formatter,
            expr: &dyn PhysicalExpr,
            precedence: u8,
        ) -> std::fmt::Result {
            if let Some(child) = expr.as_any().downcast_ref::<BinaryExpr>() {
                let p = child.op.precedence();
                if p == 0 || p < precedence {
                    write!(f, "({child})")?;
                } else {
                    write!(f, "{child}")?;
                }
            } else {
                write!(f, "{expr}")?;
            }

            Ok(())
        }

        let precedence = self.op.precedence();
        write_child(f, self.left.as_ref(), precedence)?;
        write!(f, " {} ", self.op)?;
        write_child(f, self.right.as_ref(), precedence)
    }
}

/// Invoke a boolean kernel on a pair of arrays
#[inline]
fn boolean_op(
    left: &dyn Array,
    right: &dyn Array,
    op: impl FnOnce(&BooleanArray, &BooleanArray) -> Result<BooleanArray, ArrowError>,
) -> Result<Arc<(dyn Array + 'static)>, ArrowError> {
    let ll = as_boolean_array(left).expect("boolean_op failed to downcast left array");
    let rr = as_boolean_array(right).expect("boolean_op failed to downcast right array");
    op(ll, rr).map(|t| Arc::new(t) as _)
}

macro_rules! binary_string_array_flag_op {
    ($LEFT:expr, $RIGHT:expr, $OP:ident, $NOT:expr, $FLAG:expr) => {{
        match $LEFT.data_type() {
            DataType::Utf8 => {
                compute_utf8_flag_op!($LEFT, $RIGHT, $OP, StringArray, $NOT, $FLAG)
            },
            DataType::Utf8View => {
                compute_utf8view_flag_op!($LEFT, $RIGHT, $OP, StringViewArray, $NOT, $FLAG)
            }
            DataType::LargeUtf8 => {
                compute_utf8_flag_op!($LEFT, $RIGHT, $OP, LargeStringArray, $NOT, $FLAG)
            },
            other => internal_err!(
                "Data type {:?} not supported for binary_string_array_flag_op operation '{}' on string array",
                other, stringify!($OP)
            ),
        }
    }};
}

/// Invoke a compute kernel on a pair of binary data arrays with flags
macro_rules! compute_utf8_flag_op {
    ($LEFT:expr, $RIGHT:expr, $OP:ident, $ARRAYTYPE:ident, $NOT:expr, $FLAG:expr) => {{
        let ll = $LEFT
            .as_any()
            .downcast_ref::<$ARRAYTYPE>()
            .expect("compute_utf8_flag_op failed to downcast array");
        let rr = $RIGHT
            .as_any()
            .downcast_ref::<$ARRAYTYPE>()
            .expect("compute_utf8_flag_op failed to downcast array");

        let flag = if $FLAG {
            Some($ARRAYTYPE::from(vec!["i"; ll.len()]))
        } else {
            None
        };
        let mut array = $OP(ll, rr, flag.as_ref())?;
        if $NOT {
            array = not(&array).unwrap();
        }
        Ok(Arc::new(array))
    }};
}

/// Invoke a compute kernel on a pair of binary data arrays with flags
macro_rules! compute_utf8view_flag_op {
    ($LEFT:expr, $RIGHT:expr, $OP:ident, $ARRAYTYPE:ident, $NOT:expr, $FLAG:expr) => {{
        let ll = $LEFT
            .as_any()
            .downcast_ref::<$ARRAYTYPE>()
            .expect("compute_utf8view_flag_op failed to downcast array");
        let rr = $RIGHT
            .as_any()
            .downcast_ref::<$ARRAYTYPE>()
            .expect("compute_utf8view_flag_op failed to downcast array");

        let flag = if $FLAG {
            Some($ARRAYTYPE::from(vec!["i"; ll.len()]))
        } else {
            None
        };
        let mut array = $OP(ll, rr, flag.as_ref())?;
        if $NOT {
            array = not(&array).unwrap();
        }
        Ok(Arc::new(array))
    }};
}

macro_rules! binary_string_array_flag_op_scalar {
    ($LEFT:ident, $RIGHT:expr, $OP:ident, $NOT:expr, $FLAG:expr) => {{
        // This macro is slightly different from binary_string_array_flag_op because, when comparing with a scalar value,
        // the query can be optimized in such a way that operands will be dicts, so we need to support it here
        let result: Result<Arc<dyn Array>> = match $LEFT.data_type() {
            DataType::Utf8 => {
                compute_utf8_flag_op_scalar!($LEFT, $RIGHT, $OP, StringArray, $NOT, $FLAG)
            },
            DataType::Utf8View => {
                compute_utf8view_flag_op_scalar!($LEFT, $RIGHT, $OP, StringViewArray, $NOT, $FLAG)
            }
            DataType::LargeUtf8 => {
                compute_utf8_flag_op_scalar!($LEFT, $RIGHT, $OP, LargeStringArray, $NOT, $FLAG)
            },
            DataType::Dictionary(_, _) => {
                let values = $LEFT.as_any_dictionary().values();

                match values.data_type() {
                    DataType::Utf8 => compute_utf8_flag_op_scalar!(values, $RIGHT, $OP, StringArray, $NOT, $FLAG),
                    DataType::Utf8View => compute_utf8view_flag_op_scalar!(values, $RIGHT, $OP, StringViewArray, $NOT, $FLAG),
                    DataType::LargeUtf8 => compute_utf8_flag_op_scalar!(values, $RIGHT, $OP, LargeStringArray, $NOT, $FLAG),
                    other => internal_err!(
                        "Data type {:?} not supported as a dictionary value type for binary_string_array_flag_op_scalar operation '{}' on string array",
                        other, stringify!($OP)
                    ),
                }.map(
                    // downcast_dictionary_array duplicates code per possible key type, so we aim to do all prep work before
                    |evaluated_values| downcast_dictionary_array! {
                        $LEFT => {
                            let unpacked_dict = evaluated_values.take_iter($LEFT.keys().iter().map(|opt| opt.map(|v| v as _))).collect::<BooleanArray>();
                            Arc::new(unpacked_dict) as _
                        },
                        _ => unreachable!(),
                    }
                )
            },
            other => internal_err!(
                "Data type {:?} not supported for binary_string_array_flag_op_scalar operation '{}' on string array",
                other, stringify!($OP)
            ),
        };
        Some(result)
    }};
}

/// Invoke a compute kernel on a data array and a scalar value with flag
macro_rules! compute_utf8_flag_op_scalar {
    ($LEFT:expr, $RIGHT:expr, $OP:ident, $ARRAYTYPE:ident, $NOT:expr, $FLAG:expr) => {{
        let ll = $LEFT
            .as_any()
            .downcast_ref::<$ARRAYTYPE>()
            .expect("compute_utf8_flag_op_scalar failed to downcast array");

        let string_value = match $RIGHT.try_as_str() {
            Some(Some(string_value)) => string_value,
            // null literal or non string
            _ => return internal_err!(
                        "compute_utf8_flag_op_scalar failed to cast literal value {} for operation '{}'",
                        $RIGHT, stringify!($OP)
                    )
        };

        let flag = $FLAG.then_some("i");
        let mut array =
            paste::expr! {[<$OP _scalar>]}(ll, &string_value, flag)?;
        if $NOT {
            array = not(&array).unwrap();
        }

        Ok(Arc::new(array))
    }};
}

/// Invoke a compute kernel on a data array and a scalar value with flag
macro_rules! compute_utf8view_flag_op_scalar {
    ($LEFT:expr, $RIGHT:expr, $OP:ident, $ARRAYTYPE:ident, $NOT:expr, $FLAG:expr) => {{
        let ll = $LEFT
            .as_any()
            .downcast_ref::<$ARRAYTYPE>()
            .expect("compute_utf8view_flag_op_scalar failed to downcast array");

        let string_value = match $RIGHT.try_as_str() {
            Some(Some(string_value)) => string_value,
            // null literal or non string
            _ => return internal_err!(
                        "compute_utf8view_flag_op_scalar failed to cast literal value {} for operation '{}'",
                        $RIGHT, stringify!($OP)
                    )
        };

        let flag = $FLAG.then_some("i");
        let mut array =
            paste::expr! {[<$OP _scalar>]}(ll, &string_value, flag)?;
        if $NOT {
            array = not(&array).unwrap();
        }

        Ok(Arc::new(array))
    }};
}

impl PhysicalExpr for BinaryExpr {
    /// Return a reference to Any that can be used for downcasting
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self, input_schema: &Schema) -> Result<DataType> {
        BinaryTypeCoercer::new(
            &self.left.data_type(input_schema)?,
            &self.op,
            &self.right.data_type(input_schema)?,
        )
        .get_result_type()
    }

    fn nullable(&self, input_schema: &Schema) -> Result<bool> {
        Ok(self.left.nullable(input_schema)? || self.right.nullable(input_schema)?)
    }

    fn evaluate(&self, batch: &RecordBatch) -> Result<ColumnarValue> {
        use arrow::compute::kernels::numeric::*;

        // Evaluate left-hand side expression.
        let lhs = self.left.evaluate(batch)?;

        // Check if we can apply short-circuit evaluation.
        match check_short_circuit(&lhs, &self.op) {
            ShortCircuitStrategy::None => {}
            ShortCircuitStrategy::ReturnLeft => return Ok(lhs),
            ShortCircuitStrategy::ReturnRight => {
                let rhs = self.right.evaluate(batch)?;
                return Ok(rhs);
            }
            ShortCircuitStrategy::PreSelection(selection) => {
                // The function `evaluate_selection` was not called for filtering and calculation,
                // as it takes into account cases where the selection contains null values.
                let batch = filter_record_batch(batch, selection)?;
                let right_ret = self.right.evaluate(&batch)?;
                return pre_selection_scatter(selection, right_ret);
            }
        }

        let rhs = self.right.evaluate(batch)?;
        let left_data_type = lhs.data_type();
        let right_data_type = rhs.data_type();

        let schema = batch.schema();
        let input_schema = schema.as_ref();

        if left_data_type.is_nested() {
            if !left_data_type.equals_datatype(&right_data_type) {
                return internal_err!("Cannot evaluate binary expression because of type mismatch: left {}, right {} ", left_data_type, right_data_type);
            }
            return apply_cmp_for_nested(self.op, &lhs, &rhs);
        }

        match self.op {
            Operator::Plus if self.fail_on_overflow => return apply(&lhs, &rhs, add),
            Operator::Plus => return apply(&lhs, &rhs, add_wrapping),
            Operator::Minus if self.fail_on_overflow => return apply(&lhs, &rhs, sub),
            Operator::Minus => return apply(&lhs, &rhs, sub_wrapping),
            Operator::Multiply if self.fail_on_overflow => return apply(&lhs, &rhs, mul),
            Operator::Multiply => return apply(&lhs, &rhs, mul_wrapping),
            Operator::Divide => return apply(&lhs, &rhs, div),
            Operator::Modulo => return apply(&lhs, &rhs, rem),
            Operator::Eq => return apply_cmp(&lhs, &rhs, eq),
            Operator::NotEq => return apply_cmp(&lhs, &rhs, neq),
            Operator::Lt => return apply_cmp(&lhs, &rhs, lt),
            Operator::Gt => return apply_cmp(&lhs, &rhs, gt),
            Operator::LtEq => return apply_cmp(&lhs, &rhs, lt_eq),
            Operator::GtEq => return apply_cmp(&lhs, &rhs, gt_eq),
            Operator::IsDistinctFrom => return apply_cmp(&lhs, &rhs, distinct),
            Operator::IsNotDistinctFrom => return apply_cmp(&lhs, &rhs, not_distinct),
            Operator::LikeMatch => return apply_cmp(&lhs, &rhs, like),
            Operator::ILikeMatch => return apply_cmp(&lhs, &rhs, ilike),
            Operator::NotLikeMatch => return apply_cmp(&lhs, &rhs, nlike),
            Operator::NotILikeMatch => return apply_cmp(&lhs, &rhs, nilike),
            _ => {}
        }

        let result_type = self.data_type(input_schema)?;

        // If the left-hand side is an array and the right-hand side is a non-null scalar, try the optimized kernel.
        if let (ColumnarValue::Array(array), ColumnarValue::Scalar(ref scalar)) =
            (&lhs, &rhs)
        {
            if !scalar.is_null() {
                if let Some(result_array) =
                    self.evaluate_array_scalar(array, scalar.clone())?
                {
                    let final_array = result_array
                        .and_then(|a| to_result_type_array(&self.op, a, &result_type));
                    return final_array.map(ColumnarValue::Array);
                }
            }
        }

        // if both arrays or both literals - extract arrays and continue execution
        let (left, right) = (
            lhs.into_array(batch.num_rows())?,
            rhs.into_array(batch.num_rows())?,
        );
        self.evaluate_with_resolved_args(left, &left_data_type, right, &right_data_type)
            .map(ColumnarValue::Array)
    }

    fn children(&self) -> Vec<&Arc<dyn PhysicalExpr>> {
        vec![&self.left, &self.right]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn PhysicalExpr>>,
    ) -> Result<Arc<dyn PhysicalExpr>> {
        Ok(Arc::new(
            BinaryExpr::new(Arc::clone(&children[0]), self.op, Arc::clone(&children[1]))
                .with_fail_on_overflow(self.fail_on_overflow),
        ))
    }

    fn evaluate_bounds(&self, children: &[&Interval]) -> Result<Interval> {
        // Get children intervals:
        let left_interval = children[0];
        let right_interval = children[1];
        // Calculate current node's interval:
        apply_operator(&self.op, left_interval, right_interval)
    }

    fn propagate_constraints(
        &self,
        interval: &Interval,
        children: &[&Interval],
    ) -> Result<Option<Vec<Interval>>> {
        // Get children intervals.
        let left_interval = children[0];
        let right_interval = children[1];

        if self.op.eq(&Operator::And) {
            if interval.eq(&Interval::CERTAINLY_TRUE) {
                // A certainly true logical conjunction can only derive from possibly
                // true operands. Otherwise, we prove infeasibility.
                Ok((!left_interval.eq(&Interval::CERTAINLY_FALSE)
                    && !right_interval.eq(&Interval::CERTAINLY_FALSE))
                .then(|| vec![Interval::CERTAINLY_TRUE, Interval::CERTAINLY_TRUE]))
            } else if interval.eq(&Interval::CERTAINLY_FALSE) {
                // If the logical conjunction is certainly false, one of the
                // operands must be false. However, it's not always possible to
                // determine which operand is false, leading to different scenarios.

                // If one operand is certainly true and the other one is uncertain,
                // then the latter must be certainly false.
                if left_interval.eq(&Interval::CERTAINLY_TRUE)
                    && right_interval.eq(&Interval::UNCERTAIN)
                {
                    Ok(Some(vec![
                        Interval::CERTAINLY_TRUE,
                        Interval::CERTAINLY_FALSE,
                    ]))
                } else if right_interval.eq(&Interval::CERTAINLY_TRUE)
                    && left_interval.eq(&Interval::UNCERTAIN)
                {
                    Ok(Some(vec![
                        Interval::CERTAINLY_FALSE,
                        Interval::CERTAINLY_TRUE,
                    ]))
                }
                // If both children are uncertain, or if one is certainly false,
                // we cannot conclusively refine their intervals. In this case,
                // propagation does not result in any interval changes.
                else {
                    Ok(Some(vec![]))
                }
            } else {
                // An uncertain logical conjunction result can not shrink the
                // end-points of its children.
                Ok(Some(vec![]))
            }
        } else if self.op.eq(&Operator::Or) {
            if interval.eq(&Interval::CERTAINLY_FALSE) {
                // A certainly false logical disjunction can only derive from certainly
                // false operands. Otherwise, we prove infeasibility.
                Ok((!left_interval.eq(&Interval::CERTAINLY_TRUE)
                    && !right_interval.eq(&Interval::CERTAINLY_TRUE))
                .then(|| vec![Interval::CERTAINLY_FALSE, Interval::CERTAINLY_FALSE]))
            } else if interval.eq(&Interval::CERTAINLY_TRUE) {
                // If the logical disjunction is certainly true, one of the
                // operands must be true. However, it's not always possible to
                // determine which operand is true, leading to different scenarios.

                // If one operand is certainly false and the other one is uncertain,
                // then the latter must be certainly true.
                if left_interval.eq(&Interval::CERTAINLY_FALSE)
                    && right_interval.eq(&Interval::UNCERTAIN)
                {
                    Ok(Some(vec![
                        Interval::CERTAINLY_FALSE,
                        Interval::CERTAINLY_TRUE,
                    ]))
                } else if right_interval.eq(&Interval::CERTAINLY_FALSE)
                    && left_interval.eq(&Interval::UNCERTAIN)
                {
                    Ok(Some(vec![
                        Interval::CERTAINLY_TRUE,
                        Interval::CERTAINLY_FALSE,
                    ]))
                }
                // If both children are uncertain, or if one is certainly true,
                // we cannot conclusively refine their intervals. In this case,
                // propagation does not result in any interval changes.
                else {
                    Ok(Some(vec![]))
                }
            } else {
                // An uncertain logical disjunction result can not shrink the
                // end-points of its children.
                Ok(Some(vec![]))
            }
        } else if self.op.supports_propagation() {
            Ok(
                propagate_comparison(&self.op, interval, left_interval, right_interval)?
                    .map(|(left, right)| vec![left, right]),
            )
        } else {
            Ok(
                propagate_arithmetic(&self.op, interval, left_interval, right_interval)?
                    .map(|(left, right)| vec![left, right]),
            )
        }
    }

    fn evaluate_statistics(&self, children: &[&Distribution]) -> Result<Distribution> {
        let (left, right) = (children[0], children[1]);

        if self.op.is_numerical_operators() {
            // We might be able to construct the output statistics more accurately,
            // without falling back to an unknown distribution, if we are dealing
            // with Gaussian distributions and numerical operations.
            if let (Gaussian(left), Gaussian(right)) = (left, right) {
                if let Some(result) = combine_gaussians(&self.op, left, right)? {
                    return Ok(Gaussian(result));
                }
            }
        } else if self.op.is_logic_operator() {
            // If we are dealing with logical operators, we expect (and can only
            // operate on) Bernoulli distributions.
            return if let (Bernoulli(left), Bernoulli(right)) = (left, right) {
                combine_bernoullis(&self.op, left, right).map(Bernoulli)
            } else {
                internal_err!(
                    "Logical operators are only compatible with `Bernoulli` distributions"
                )
            };
        } else if self.op.supports_propagation() {
            // If we are handling comparison operators, we expect (and can only
            // operate on) numeric distributions.
            return create_bernoulli_from_comparison(&self.op, left, right);
        }
        // Fall back to an unknown distribution with only summary statistics:
        new_generic_from_binary_op(&self.op, left, right)
    }

    /// For each operator, [`BinaryExpr`] has distinct rules.
    /// TODO: There may be rules specific to some data types and expression ranges.
    fn get_properties(&self, children: &[ExprProperties]) -> Result<ExprProperties> {
        let (l_order, l_range) = (children[0].sort_properties, &children[0].range);
        let (r_order, r_range) = (children[1].sort_properties, &children[1].range);
        match self.op() {
            Operator::Plus => Ok(ExprProperties {
                sort_properties: l_order.add(&r_order),
                range: l_range.add(r_range)?,
                preserves_lex_ordering: false,
            }),
            Operator::Minus => Ok(ExprProperties {
                sort_properties: l_order.sub(&r_order),
                range: l_range.sub(r_range)?,
                preserves_lex_ordering: false,
            }),
            Operator::Gt => Ok(ExprProperties {
                sort_properties: l_order.gt_or_gteq(&r_order),
                range: l_range.gt(r_range)?,
                preserves_lex_ordering: false,
            }),
            Operator::GtEq => Ok(ExprProperties {
                sort_properties: l_order.gt_or_gteq(&r_order),
                range: l_range.gt_eq(r_range)?,
                preserves_lex_ordering: false,
            }),
            Operator::Lt => Ok(ExprProperties {
                sort_properties: r_order.gt_or_gteq(&l_order),
                range: l_range.lt(r_range)?,
                preserves_lex_ordering: false,
            }),
            Operator::LtEq => Ok(ExprProperties {
                sort_properties: r_order.gt_or_gteq(&l_order),
                range: l_range.lt_eq(r_range)?,
                preserves_lex_ordering: false,
            }),
            Operator::And => Ok(ExprProperties {
                sort_properties: r_order.and_or(&l_order),
                range: l_range.and(r_range)?,
                preserves_lex_ordering: false,
            }),
            Operator::Or => Ok(ExprProperties {
                sort_properties: r_order.and_or(&l_order),
                range: l_range.or(r_range)?,
                preserves_lex_ordering: false,
            }),
            _ => Ok(ExprProperties::new_unknown()),
        }
    }

    fn fmt_sql(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn write_child(
            f: &mut std::fmt::Formatter,
            expr: &dyn PhysicalExpr,
            precedence: u8,
        ) -> std::fmt::Result {
            if let Some(child) = expr.as_any().downcast_ref::<BinaryExpr>() {
                let p = child.op.precedence();
                if p == 0 || p < precedence {
                    write!(f, "(")?;
                    child.fmt_sql(f)?;
                    write!(f, ")")
                } else {
                    child.fmt_sql(f)
                }
            } else {
                expr.fmt_sql(f)
            }
        }

        let precedence = self.op.precedence();
        write_child(f, self.left.as_ref(), precedence)?;
        write!(f, " {} ", self.op)?;
        write_child(f, self.right.as_ref(), precedence)
    }
}

/// Casts dictionary array to result type for binary numerical operators. Such operators
/// between array and scalar produce a dictionary array other than primitive array of the
/// same operators between array and array. This leads to inconsistent result types causing
/// errors in the following query execution. For such operators between array and scalar,
/// we cast the dictionary array to primitive array.
fn to_result_type_array(
    op: &Operator,
    array: ArrayRef,
    result_type: &DataType,
) -> Result<ArrayRef> {
    if array.data_type() == result_type {
        Ok(array)
    } else if op.is_numerical_operators() {
        match array.data_type() {
            DataType::Dictionary(_, value_type) => {
                if value_type.as_ref() == result_type {
                    Ok(cast(&array, result_type)?)
                } else {
                    internal_err!(
                            "Incompatible Dictionary value type {value_type:?} with result type {result_type:?} of Binary operator {op:?}"
                        )
                }
            }
            _ => Ok(array),
        }
    } else {
        Ok(array)
    }
}

impl BinaryExpr {
    /// Evaluate the expression of the left input is an array and
    /// right is literal - use scalar operations
    fn evaluate_array_scalar(
        &self,
        array: &dyn Array,
        scalar: ScalarValue,
    ) -> Result<Option<Result<ArrayRef>>> {
        use Operator::*;
        let scalar_result = match &self.op {
            RegexMatch => binary_string_array_flag_op_scalar!(
                array,
                scalar,
                regexp_is_match,
                false,
                false
            ),
            RegexIMatch => binary_string_array_flag_op_scalar!(
                array,
                scalar,
                regexp_is_match,
                false,
                true
            ),
            RegexNotMatch => binary_string_array_flag_op_scalar!(
                array,
                scalar,
                regexp_is_match,
                true,
                false
            ),
            RegexNotIMatch => binary_string_array_flag_op_scalar!(
                array,
                scalar,
                regexp_is_match,
                true,
                true
            ),
            BitwiseAnd => bitwise_and_dyn_scalar(array, scalar),
            BitwiseOr => bitwise_or_dyn_scalar(array, scalar),
            BitwiseXor => bitwise_xor_dyn_scalar(array, scalar),
            BitwiseShiftRight => bitwise_shift_right_dyn_scalar(array, scalar),
            BitwiseShiftLeft => bitwise_shift_left_dyn_scalar(array, scalar),
            // if scalar operation is not supported - fallback to array implementation
            _ => None,
        };

        Ok(scalar_result)
    }

    fn evaluate_with_resolved_args(
        &self,
        left: Arc<dyn Array>,
        left_data_type: &DataType,
        right: Arc<dyn Array>,
        right_data_type: &DataType,
    ) -> Result<ArrayRef> {
        use Operator::*;
        match &self.op {
            IsDistinctFrom | IsNotDistinctFrom | Lt | LtEq | Gt | GtEq | Eq | NotEq
            | Plus | Minus | Multiply | Divide | Modulo | LikeMatch | ILikeMatch
            | NotLikeMatch | NotILikeMatch => unreachable!(),
            And => {
                if left_data_type == &DataType::Boolean {
                    Ok(boolean_op(&left, &right, and_kleene)?)
                } else {
                    internal_err!(
                        "Cannot evaluate binary expression {:?} with types {:?} and {:?}",
                        self.op,
                        left.data_type(),
                        right.data_type()
                    )
                }
            }
            Or => {
                if left_data_type == &DataType::Boolean {
                    Ok(boolean_op(&left, &right, or_kleene)?)
                } else {
                    internal_err!(
                        "Cannot evaluate binary expression {:?} with types {:?} and {:?}",
                        self.op,
                        left_data_type,
                        right_data_type
                    )
                }
            }
            RegexMatch => {
                binary_string_array_flag_op!(left, right, regexp_is_match, false, false)
            }
            RegexIMatch => {
                binary_string_array_flag_op!(left, right, regexp_is_match, false, true)
            }
            RegexNotMatch => {
                binary_string_array_flag_op!(left, right, regexp_is_match, true, false)
            }
            RegexNotIMatch => {
                binary_string_array_flag_op!(left, right, regexp_is_match, true, true)
            }
            BitwiseAnd => bitwise_and_dyn(left, right),
            BitwiseOr => bitwise_or_dyn(left, right),
            BitwiseXor => bitwise_xor_dyn(left, right),
            BitwiseShiftRight => bitwise_shift_right_dyn(left, right),
            BitwiseShiftLeft => bitwise_shift_left_dyn(left, right),
            StringConcat => concat_elements(left, right),
            AtArrow | ArrowAt | Arrow | LongArrow | HashArrow | HashLongArrow | AtAt
            | HashMinus | AtQuestion | Question | QuestionAnd | QuestionPipe
            | IntegerDivide => {
                not_impl_err!(
                    "Binary operator '{:?}' is not supported in the physical expr",
                    self.op
                )
            }
        }
    }
}

enum ShortCircuitStrategy<'a> {
    None,
    ReturnLeft,
    ReturnRight,
    PreSelection(&'a BooleanArray),
}

/// Based on the results calculated from the left side of the short-circuit operation,
/// if the proportion of `true` is less than 0.2 and the current operation is an `and`,
/// the `RecordBatch` will be filtered in advance.
const PRE_SELECTION_THRESHOLD: f32 = 0.2;

/// Checks if a logical operator (`AND`/`OR`) can short-circuit evaluation based on the left-hand side (lhs) result.
///
/// Short-circuiting occurs under these circumstances:
/// - For `AND`:
///    - if LHS is all false => short-circuit → return LHS
///    - if LHS is all true  => short-circuit → return RHS
///    - if LHS is mixed and true_count/sum_count <= [`PRE_SELECTION_THRESHOLD`] -> pre-selection
/// - For `OR`:
///    - if LHS is all true  => short-circuit → return LHS
///    - if LHS is all false => short-circuit → return RHS
/// # Arguments
/// * `lhs` - The left-hand side (lhs) columnar value (array or scalar)
/// * `lhs` - The left-hand side (lhs) columnar value (array or scalar)
/// * `op` - The logical operator (`AND` or `OR`)
///
/// # Implementation Notes
/// 1. Only works with Boolean-typed arguments (other types automatically return `false`)
/// 2. Handles both scalar values and array values
/// 3. For arrays, uses optimized bit counting techniques for boolean arrays
fn check_short_circuit<'a>(
    lhs: &'a ColumnarValue,
    op: &Operator,
) -> ShortCircuitStrategy<'a> {
    // Quick reject for non-logical operators,and quick judgment when op is and
    let is_and = match op {
        Operator::And => true,
        Operator::Or => false,
        _ => return ShortCircuitStrategy::None,
    };

    // Non-boolean types can't be short-circuited
    if lhs.data_type() != DataType::Boolean {
        return ShortCircuitStrategy::None;
    }

    match lhs {
        ColumnarValue::Array(array) => {
            // Fast path for arrays - try to downcast to boolean array
            if let Ok(bool_array) = as_boolean_array(array) {
                // Arrays with nulls can't be short-circuited
                if bool_array.null_count() > 0 {
                    return ShortCircuitStrategy::None;
                }

                let len = bool_array.len();
                if len == 0 {
                    return ShortCircuitStrategy::None;
                }

                let true_count = bool_array.values().count_set_bits();
                if is_and {
                    // For AND, prioritize checking for all-false (short circuit case)
                    // Uses optimized false_count() method provided by Arrow

                    // Short circuit if all values are false
                    if true_count == 0 {
                        return ShortCircuitStrategy::ReturnLeft;
                    }

                    // If no false values, then all must be true
                    if true_count == len {
                        return ShortCircuitStrategy::ReturnRight;
                    }

                    // determine if we can pre-selection
                    if true_count as f32 / len as f32 <= PRE_SELECTION_THRESHOLD {
                        return ShortCircuitStrategy::PreSelection(bool_array);
                    }
                } else {
                    // For OR, prioritize checking for all-true (short circuit case)
                    // Uses optimized true_count() method provided by Arrow

                    // Short circuit if all values are true
                    if true_count == len {
                        return ShortCircuitStrategy::ReturnLeft;
                    }

                    // If no true values, then all must be false
                    if true_count == 0 {
                        return ShortCircuitStrategy::ReturnRight;
                    }
                }
            }
        }
        ColumnarValue::Scalar(scalar) => {
            // Fast path for scalar values
            if let ScalarValue::Boolean(Some(is_true)) = scalar {
                // Return Left for:
                // - AND with false value
                // - OR with true value
                if (is_and && !is_true) || (!is_and && *is_true) {
                    return ShortCircuitStrategy::ReturnLeft;
                } else {
                    return ShortCircuitStrategy::ReturnRight;
                }
            }
        }
    }

    // If we can't short-circuit, indicate that normal evaluation should continue
    ShortCircuitStrategy::None
}

/// Creates a new boolean array based on the evaluation of the right expression,
/// but only for positions where the left_result is true.
///
/// This function is used for short-circuit evaluation optimization of logical AND operations:
/// - When left_result has few true values, we only evaluate the right expression for those positions
/// - Values are copied from right_array where left_result is true
/// - All other positions are filled with false values
///
/// # Parameters
/// - `left_result` Boolean array with selection mask (typically from left side of AND)
/// - `right_result` Result of evaluating right side of expression (only for selected positions)
///
/// # Returns
/// A combined ColumnarValue with values from right_result where left_result is true
///
/// # Example
///  Initial Data: { 1, 2, 3, 4, 5 }
///  Left Evaluation
///     (Condition: Equal to 2 or 3)
///          ↓
///  Filtered Data: {2, 3}
///    Left Bitmap: { 0, 1, 1, 0, 0 }
///          ↓
///   Right Evaluation
///     (Condition: Even numbers)
///          ↓
///  Right Data: { 2 }
///    Right Bitmap: { 1, 0 }
///          ↓
///   Combine Results
///  Final Bitmap: { 0, 1, 0, 0, 0 }
///
/// # Note
/// Perhaps it would be better to modify `left_result` directly without creating a copy?
/// In practice, `left_result` should have only one owner, so making changes should be safe.
/// However, this is difficult to achieve under the immutable constraints of [`Arc`] and [`BooleanArray`].
fn pre_selection_scatter(
    left_result: &BooleanArray,
    right_result: ColumnarValue,
) -> Result<ColumnarValue> {
    let right_boolean_array = match &right_result {
        ColumnarValue::Array(array) => array.as_boolean(),
        ColumnarValue::Scalar(_) => return Ok(right_result),
    };

    let result_len = left_result.len();

    let mut result_array_builder = BooleanArray::builder(result_len);

    // keep track of current position we have in right boolean array
    let mut right_array_pos = 0;

    // keep track of how much is filled
    let mut last_end = 0;
    SlicesIterator::new(left_result).for_each(|(start, end)| {
        // the gap needs to be filled with false
        if start > last_end {
            result_array_builder.append_n(start - last_end, false);
        }

        // copy values from right array for this slice
        let len = end - start;
        right_boolean_array
            .slice(right_array_pos, len)
            .iter()
            .for_each(|v| result_array_builder.append_option(v));

        right_array_pos += len;
        last_end = end;
    });

    // Fill any remaining positions with false
    if last_end < result_len {
        result_array_builder.append_n(result_len - last_end, false);
    }
    let boolean_result = result_array_builder.finish();

    Ok(ColumnarValue::Array(Arc::new(boolean_result)))
}

fn concat_elements(left: Arc<dyn Array>, right: Arc<dyn Array>) -> Result<ArrayRef> {
    Ok(match left.data_type() {
        DataType::Utf8 => Arc::new(concat_elements_utf8(
            left.as_string::<i32>(),
            right.as_string::<i32>(),
        )?),
        DataType::LargeUtf8 => Arc::new(concat_elements_utf8(
            left.as_string::<i64>(),
            right.as_string::<i64>(),
        )?),
        DataType::Utf8View => Arc::new(concat_elements_utf8view(
            left.as_string_view(),
            right.as_string_view(),
        )?),
        other => {
            return internal_err!(
                "Data type {other:?} not supported for binary operation 'concat_elements' on string arrays"
            );
        }
    })
}

/// Create a binary expression whose arguments are correctly coerced.
/// This function errors if it is not possible to coerce the arguments
/// to computational types supported by the operator.
pub fn binary(
    lhs: Arc<dyn PhysicalExpr>,
    op: Operator,
    rhs: Arc<dyn PhysicalExpr>,
    _input_schema: &Schema,
) -> Result<Arc<dyn PhysicalExpr>> {
    Ok(Arc::new(BinaryExpr::new(lhs, op, rhs)))
}

/// Create a similar to expression
pub fn similar_to(
    negated: bool,
    case_insensitive: bool,
    expr: Arc<dyn PhysicalExpr>,
    pattern: Arc<dyn PhysicalExpr>,
) -> Result<Arc<dyn PhysicalExpr>> {
    let binary_op = match (negated, case_insensitive) {
        (false, false) => Operator::RegexMatch,
        (false, true) => Operator::RegexIMatch,
        (true, false) => Operator::RegexNotMatch,
        (true, true) => Operator::RegexNotIMatch,
    };
    Ok(Arc::new(BinaryExpr::new(expr, binary_op, pattern)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expressions::{col, lit, try_cast, Column, Literal};
    use datafusion_expr::lit as expr_lit;

    use datafusion_common::plan_datafusion_err;
    use datafusion_physical_expr_common::physical_expr::fmt_sql;

    use crate::planner::logical2physical;
    use arrow::array::BooleanArray;
    use datafusion_expr::col as logical_col;
    /// Performs a binary operation, applying any type coercion necessary
    fn binary_op(
        left: Arc<dyn PhysicalExpr>,
        op: Operator,
        right: Arc<dyn PhysicalExpr>,
        schema: &Schema,
    ) -> Result<Arc<dyn PhysicalExpr>> {
        let left_type = left.data_type(schema)?;
        let right_type = right.data_type(schema)?;
        let (lhs, rhs) =
            BinaryTypeCoercer::new(&left_type, &op, &right_type).get_input_types()?;

        let left_expr = try_cast(left, schema, lhs)?;
        let right_expr = try_cast(right, schema, rhs)?;
        binary(left_expr, op, right_expr, schema)
    }

    #[test]
    fn binary_comparison() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
        ]);
        let a = Int32Array::from(vec![1, 2, 3, 4, 5]);
        let b = Int32Array::from(vec![1, 2, 4, 8, 16]);

        // expression: "a < b"
        let lt = binary(
            col("a", &schema)?,
            Operator::Lt,
            col("b", &schema)?,
            &schema,
        )?;
        let batch =
            RecordBatch::try_new(Arc::new(schema), vec![Arc::new(a), Arc::new(b)])?;

        let result = lt
            .evaluate(&batch)?
            .into_array(batch.num_rows())
            .expect("Failed to convert to array");
        assert_eq!(result.len(), 5);

        let expected = [false, false, true, true, true];
        let result =
            as_boolean_array(&result).expect("failed to downcast to BooleanArray");
        for (i, &expected_item) in expected.iter().enumerate().take(5) {
            assert_eq!(result.value(i), expected_item);
        }

        Ok(())
    }

    #[test]
    fn binary_nested() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
        ]);
        let a = Int32Array::from(vec![2, 4, 6, 8, 10]);
        let b = Int32Array::from(vec![2, 5, 4, 8, 8]);

        // expression: "a < b OR a == b"
        let expr = binary(
            binary(
                col("a", &schema)?,
                Operator::Lt,
                col("b", &schema)?,
                &schema,
            )?,
            Operator::Or,
            binary(
                col("a", &schema)?,
                Operator::Eq,
                col("b", &schema)?,
                &schema,
            )?,
            &schema,
        )?;
        let batch =
            RecordBatch::try_new(Arc::new(schema), vec![Arc::new(a), Arc::new(b)])?;

        assert_eq!("a@0 < b@1 OR a@0 = b@1", format!("{expr}"));

        let result = expr
            .evaluate(&batch)?
            .into_array(batch.num_rows())
            .expect("Failed to convert to array");
        assert_eq!(result.len(), 5);

        let expected = [true, true, false, true, false];
        let result =
            as_boolean_array(&result).expect("failed to downcast to BooleanArray");
        for (i, &expected_item) in expected.iter().enumerate().take(5) {
            assert_eq!(result.value(i), expected_item);
        }

        Ok(())
    }

    // runs an end-to-end test of physical type coercion:
    // 1. construct a record batch with two columns of type A and B
    //  (*_ARRAY is the Rust Arrow array type, and *_TYPE is the DataType of the elements)
    // 2. construct a physical expression of A OP B
    // 3. evaluate the expression
    // 4. verify that the resulting expression is of type C
    // 5. verify that the results of evaluation are $VEC
    macro_rules! test_coercion {
        ($A_ARRAY:ident, $A_TYPE:expr, $A_VEC:expr, $B_ARRAY:ident, $B_TYPE:expr, $B_VEC:expr, $OP:expr, $C_ARRAY:ident, $C_TYPE:expr, $VEC:expr,) => {{
            let schema = Schema::new(vec![
                Field::new("a", $A_TYPE, false),
                Field::new("b", $B_TYPE, false),
            ]);
            let a = $A_ARRAY::from($A_VEC);
            let b = $B_ARRAY::from($B_VEC);
            let (lhs, rhs) = BinaryTypeCoercer::new(&$A_TYPE, &$OP, &$B_TYPE).get_input_types()?;

            let left = try_cast(col("a", &schema)?, &schema, lhs)?;
            let right = try_cast(col("b", &schema)?, &schema, rhs)?;

            // verify that we can construct the expression
            let expression = binary(left, $OP, right, &schema)?;
            let batch = RecordBatch::try_new(
                Arc::new(schema.clone()),
                vec![Arc::new(a), Arc::new(b)],
            )?;

            // verify that the expression's type is correct
            assert_eq!(expression.data_type(&schema)?, $C_TYPE);

            // compute
            let result = expression.evaluate(&batch)?.into_array(batch.num_rows()).expect("Failed to convert to array");

            // verify that the array's data_type is correct
            assert_eq!(*result.data_type(), $C_TYPE);

            // verify that the data itself is downcastable
            let result = result
                .as_any()
                .downcast_ref::<$C_ARRAY>()
                .expect("failed to downcast");
            // verify that the result itself is correct
            for (i, x) in $VEC.iter().enumerate() {
                let v = result.value(i);
                assert_eq!(
                    v,
                    *x,
                    "Unexpected output at position {i}:\n\nActual:\n{v}\n\nExpected:\n{x}"
                );
            }
        }};
    }

    #[test]
    fn test_type_coercion() -> Result<()> {
        test_coercion!(
            Int32Array,
            DataType::Int32,
            vec![1i32, 2i32],
            UInt32Array,
            DataType::UInt32,
            vec![1u32, 2u32],
            Operator::Plus,
            Int64Array,
            DataType::Int64,
            [2i64, 4i64],
        );
        test_coercion!(
            Int32Array,
            DataType::Int32,
            vec![1i32],
            UInt16Array,
            DataType::UInt16,
            vec![1u16],
            Operator::Plus,
            Int32Array,
            DataType::Int32,
            [2i32],
        );
        test_coercion!(
            Float32Array,
            DataType::Float32,
            vec![1f32],
            UInt16Array,
            DataType::UInt16,
            vec![1u16],
            Operator::Plus,
            Float32Array,
            DataType::Float32,
            [2f32],
        );
        test_coercion!(
            Float32Array,
            DataType::Float32,
            vec![2f32],
            UInt16Array,
            DataType::UInt16,
            vec![1u16],
            Operator::Multiply,
            Float32Array,
            DataType::Float32,
            [2f32],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["1994-12-13", "1995-01-26"],
            Date32Array,
            DataType::Date32,
            vec![9112, 9156],
            Operator::Eq,
            BooleanArray,
            DataType::Boolean,
            [true, true],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["1994-12-13", "1995-01-26"],
            Date32Array,
            DataType::Date32,
            vec![9113, 9154],
            Operator::Lt,
            BooleanArray,
            DataType::Boolean,
            [true, false],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["1994-12-13T12:34:56", "1995-01-26T01:23:45"],
            Date64Array,
            DataType::Date64,
            vec![787322096000, 791083425000],
            Operator::Eq,
            BooleanArray,
            DataType::Boolean,
            [true, true],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["1994-12-13T12:34:56", "1995-01-26T01:23:45"],
            Date64Array,
            DataType::Date64,
            vec![787322096001, 791083424999],
            Operator::Lt,
            BooleanArray,
            DataType::Boolean,
            [true, false],
        );
        test_coercion!(
            StringViewArray,
            DataType::Utf8View,
            vec!["abc"; 5],
            StringArray,
            DataType::Utf8,
            vec!["^a", "^A", "(b|d)", "(B|D)", "^(b|c)"],
            Operator::RegexMatch,
            BooleanArray,
            DataType::Boolean,
            [true, false, true, false, false],
        );
        test_coercion!(
            StringViewArray,
            DataType::Utf8View,
            vec!["abc"; 5],
            StringArray,
            DataType::Utf8,
            vec!["^a", "^A", "(b|d)", "(B|D)", "^(b|c)"],
            Operator::RegexIMatch,
            BooleanArray,
            DataType::Boolean,
            [true, true, true, true, false],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["abc"; 5],
            StringViewArray,
            DataType::Utf8View,
            vec!["^a", "^A", "(b|d)", "(B|D)", "^(b|c)"],
            Operator::RegexNotMatch,
            BooleanArray,
            DataType::Boolean,
            [false, true, false, true, true],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["abc"; 5],
            StringViewArray,
            DataType::Utf8View,
            vec!["^a", "^A", "(b|d)", "(B|D)", "^(b|c)"],
            Operator::RegexNotIMatch,
            BooleanArray,
            DataType::Boolean,
            [false, false, false, false, true],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["abc"; 5],
            StringArray,
            DataType::Utf8,
            vec!["^a", "^A", "(b|d)", "(B|D)", "^(b|c)"],
            Operator::RegexMatch,
            BooleanArray,
            DataType::Boolean,
            [true, false, true, false, false],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["abc"; 5],
            StringArray,
            DataType::Utf8,
            vec!["^a", "^A", "(b|d)", "(B|D)", "^(b|c)"],
            Operator::RegexIMatch,
            BooleanArray,
            DataType::Boolean,
            [true, true, true, true, false],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["abc"; 5],
            StringArray,
            DataType::Utf8,
            vec!["^a", "^A", "(b|d)", "(B|D)", "^(b|c)"],
            Operator::RegexNotMatch,
            BooleanArray,
            DataType::Boolean,
            [false, true, false, true, true],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["abc"; 5],
            StringArray,
            DataType::Utf8,
            vec!["^a", "^A", "(b|d)", "(B|D)", "^(b|c)"],
            Operator::RegexNotIMatch,
            BooleanArray,
            DataType::Boolean,
            [false, false, false, false, true],
        );
        test_coercion!(
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["abc"; 5],
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["^a", "^A", "(b|d)", "(B|D)", "^(b|c)"],
            Operator::RegexMatch,
            BooleanArray,
            DataType::Boolean,
            [true, false, true, false, false],
        );
        test_coercion!(
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["abc"; 5],
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["^a", "^A", "(b|d)", "(B|D)", "^(b|c)"],
            Operator::RegexIMatch,
            BooleanArray,
            DataType::Boolean,
            [true, true, true, true, false],
        );
        test_coercion!(
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["abc"; 5],
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["^a", "^A", "(b|d)", "(B|D)", "^(b|c)"],
            Operator::RegexNotMatch,
            BooleanArray,
            DataType::Boolean,
            [false, true, false, true, true],
        );
        test_coercion!(
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["abc"; 5],
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["^a", "^A", "(b|d)", "(B|D)", "^(b|c)"],
            Operator::RegexNotIMatch,
            BooleanArray,
            DataType::Boolean,
            [false, false, false, false, true],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["abc"; 5],
            StringArray,
            DataType::Utf8,
            vec!["a__", "A%BC", "A_BC", "abc", "a%C"],
            Operator::LikeMatch,
            BooleanArray,
            DataType::Boolean,
            [true, false, false, true, false],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["abc"; 5],
            StringArray,
            DataType::Utf8,
            vec!["a__", "A%BC", "A_BC", "abc", "a%C"],
            Operator::ILikeMatch,
            BooleanArray,
            DataType::Boolean,
            [true, true, false, true, true],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["abc"; 5],
            StringArray,
            DataType::Utf8,
            vec!["a__", "A%BC", "A_BC", "abc", "a%C"],
            Operator::NotLikeMatch,
            BooleanArray,
            DataType::Boolean,
            [false, true, true, false, true],
        );
        test_coercion!(
            StringArray,
            DataType::Utf8,
            vec!["abc"; 5],
            StringArray,
            DataType::Utf8,
            vec!["a__", "A%BC", "A_BC", "abc", "a%C"],
            Operator::NotILikeMatch,
            BooleanArray,
            DataType::Boolean,
            [false, false, true, false, false],
        );
        test_coercion!(
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["abc"; 5],
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["a__", "A%BC", "A_BC", "abc", "a%C"],
            Operator::LikeMatch,
            BooleanArray,
            DataType::Boolean,
            [true, false, false, true, false],
        );
        test_coercion!(
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["abc"; 5],
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["a__", "A%BC", "A_BC", "abc", "a%C"],
            Operator::ILikeMatch,
            BooleanArray,
            DataType::Boolean,
            [true, true, false, true, true],
        );
        test_coercion!(
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["abc"; 5],
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["a__", "A%BC", "A_BC", "abc", "a%C"],
            Operator::NotLikeMatch,
            BooleanArray,
            DataType::Boolean,
            [false, true, true, false, true],
        );
        test_coercion!(
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["abc"; 5],
            LargeStringArray,
            DataType::LargeUtf8,
            vec!["a__", "A%BC", "A_BC", "abc", "a%C"],
            Operator::NotILikeMatch,
            BooleanArray,
            DataType::Boolean,
            [false, false, true, false, false],
        );
        test_coercion!(
            Int16Array,
            DataType::Int16,
            vec![1i16, 2i16, 3i16],
            Int64Array,
            DataType::Int64,
            vec![10i64, 4i64, 5i64],
            Operator::BitwiseAnd,
            Int64Array,
            DataType::Int64,
            [0i64, 0i64, 1i64],
        );
        test_coercion!(
            UInt16Array,
            DataType::UInt16,
            vec![1u16, 2u16, 3u16],
            UInt64Array,
            DataType::UInt64,
            vec![10u64, 4u64, 5u64],
            Operator::BitwiseAnd,
            UInt64Array,
            DataType::UInt64,
            [0u64, 0u64, 1u64],
        );
        test_coercion!(
            Int16Array,
            DataType::Int16,
            vec![3i16, 2i16, 3i16],
            Int64Array,
            DataType::Int64,
            vec![10i64, 6i64, 5i64],
            Operator::BitwiseOr,
            Int64Array,
            DataType::Int64,
            [11i64, 6i64, 7i64],
        );
        test_coercion!(
            UInt16Array,
            DataType::UInt16,
            vec![1u16, 2u16, 3u16],
            UInt64Array,
            DataType::UInt64,
            vec![10u64, 4u64, 5u64],
            Operator::BitwiseOr,
            UInt64Array,
            DataType::UInt64,
            [11u64, 6u64, 7u64],
        );
        test_coercion!(
            Int16Array,
            DataType::Int16,
            vec![3i16, 2i16, 3i16],
            Int64Array,
            DataType::Int64,
            vec![10i64, 6i64, 5i64],
            Operator::BitwiseXor,
            Int64Array,
            DataType::Int64,
            [9i64, 4i64, 6i64],
        );
        test_coercion!(
            UInt16Array,
            DataType::UInt16,
            vec![3u16, 2u16, 3u16],
            UInt64Array,
            DataType::UInt64,
            vec![10u64, 6u64, 5u64],
            Operator::BitwiseXor,
            UInt64Array,
            DataType::UInt64,
            [9u64, 4u64, 6u64],
        );
        test_coercion!(
            Int16Array,
            DataType::Int16,
            vec![4i16, 27i16, 35i16],
            Int64Array,
            DataType::Int64,
            vec![2i64, 3i64, 4i64],
            Operator::BitwiseShiftRight,
            Int64Array,
            DataType::Int64,
            [1i64, 3i64, 2i64],
        );
        test_coercion!(
            UInt16Array,
            DataType::UInt16,
            vec![4u16, 27u16, 35u16],
            UInt64Array,
            DataType::UInt64,
            vec![2u64, 3u64, 4u64],
            Operator::BitwiseShiftRight,
            UInt64Array,
            DataType::UInt64,
            [1u64, 3u64, 2u64],
        );
        test_coercion!(
            Int16Array,
            DataType::Int16,
            vec![2i16, 3i16, 4i16],
            Int64Array,
            DataType::Int64,
            vec![4i64, 12i64, 7i64],
            Operator::BitwiseShiftLeft,
            Int64Array,
            DataType::Int64,
            [32i64, 12288i64, 512i64],
        );
        test_coercion!(
            UInt16Array,
            DataType::UInt16,
            vec![2u16, 3u16, 4u16],
            UInt64Array,
            DataType::UInt64,
            vec![4u64, 12u64, 7u64],
            Operator::BitwiseShiftLeft,
            UInt64Array,
            DataType::UInt64,
            [32u64, 12288u64, 512u64],
        );
        Ok(())
    }

    // Note it would be nice to use the same test_coercion macro as
    // above, but sadly the type of the values of the dictionary are
    // not encoded in the rust type of the DictionaryArray. Thus there
    // is no way at the time of this writing to create a dictionary
    // array using the `From` trait
    #[test]
    fn test_dictionary_type_to_array_coercion() -> Result<()> {
        // Test string  a string dictionary
        let dict_type =
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8));
        let string_type = DataType::Utf8;

        // build dictionary
        let mut dict_builder = StringDictionaryBuilder::<Int32Type>::new();

        dict_builder.append("one")?;
        dict_builder.append_null();
        dict_builder.append("three")?;
        dict_builder.append("four")?;
        let dict_array = Arc::new(dict_builder.finish()) as ArrayRef;

        let str_array = Arc::new(StringArray::from(vec![
            Some("not one"),
            Some("two"),
            None,
            Some("four"),
        ])) as ArrayRef;

        let schema = Arc::new(Schema::new(vec![
            Field::new("a", dict_type.clone(), true),
            Field::new("b", string_type.clone(), true),
        ]));

        // Test 1: a = b
        let result = BooleanArray::from(vec![Some(false), None, None, Some(true)]);
        apply_logic_op(&schema, &dict_array, &str_array, Operator::Eq, result)?;

        // Test 2: now test the other direction
        // b = a
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", string_type, true),
            Field::new("b", dict_type, true),
        ]));
        let result = BooleanArray::from(vec![Some(false), None, None, Some(true)]);
        apply_logic_op(&schema, &str_array, &dict_array, Operator::Eq, result)?;

        Ok(())
    }

    #[test]
    fn plus_op() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
        ]);
        let a = Int32Array::from(vec![1, 2, 3, 4, 5]);
        let b = Int32Array::from(vec![1, 2, 4, 8, 16]);

        apply_arithmetic::<Int32Type>(
            Arc::new(schema),
            vec![Arc::new(a), Arc::new(b)],
            Operator::Plus,
            Int32Array::from(vec![2, 4, 7, 12, 21]),
        )?;

        Ok(())
    }

    #[test]
    fn plus_op_dict() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new(
                "a",
                DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
                true,
            ),
            Field::new(
                "b",
                DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
                true,
            ),
        ]);

        let a = Int32Array::from(vec![1, 2, 3, 4, 5]);
        let keys = Int8Array::from(vec![Some(0), None, Some(1), Some(3), None]);
        let a = DictionaryArray::try_new(keys, Arc::new(a))?;

        let b = Int32Array::from(vec![1, 2, 4, 8, 16]);
        let keys = Int8Array::from(vec![0, 1, 1, 2, 1]);
        let b = DictionaryArray::try_new(keys, Arc::new(b))?;

        apply_arithmetic::<Int32Type>(
            Arc::new(schema),
            vec![Arc::new(a), Arc::new(b)],
            Operator::Plus,
            Int32Array::from(vec![Some(2), None, Some(4), Some(8), None]),
        )?;

        Ok(())
    }

    #[test]
    fn plus_op_dict_decimal() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new(
                "a",
                DataType::Dictionary(
                    Box::new(DataType::Int8),
                    Box::new(DataType::Decimal128(10, 0)),
                ),
                true,
            ),
            Field::new(
                "b",
                DataType::Dictionary(
                    Box::new(DataType::Int8),
                    Box::new(DataType::Decimal128(10, 0)),
                ),
                true,
            ),
        ]);

        let value = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value),
                Some(value + 2),
                Some(value - 1),
                Some(value + 1),
            ],
            10,
            0,
        ));

        let keys = Int8Array::from(vec![Some(0), Some(2), None, Some(3), Some(0)]);
        let a = DictionaryArray::try_new(keys, decimal_array)?;

        let keys = Int8Array::from(vec![Some(0), None, Some(3), Some(2), Some(2)]);
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value + 1),
                Some(value + 3),
                Some(value),
                Some(value + 2),
            ],
            10,
            0,
        ));
        let b = DictionaryArray::try_new(keys, decimal_array)?;

        apply_arithmetic(
            Arc::new(schema),
            vec![Arc::new(a), Arc::new(b)],
            Operator::Plus,
            create_decimal_array(&[Some(247), None, None, Some(247), Some(246)], 11, 0),
        )?;

        Ok(())
    }

    #[test]
    fn plus_op_scalar() -> Result<()> {
        let schema = Schema::new(vec![Field::new("a", DataType::Int32, false)]);
        let a = Int32Array::from(vec![1, 2, 3, 4, 5]);

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Plus,
            ScalarValue::Int32(Some(1)),
            Arc::new(Int32Array::from(vec![2, 3, 4, 5, 6])),
        )?;

        Ok(())
    }

    #[test]
    fn plus_op_dict_scalar() -> Result<()> {
        let schema = Schema::new(vec![Field::new(
            "a",
            DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
            true,
        )]);

        let mut dict_builder = PrimitiveDictionaryBuilder::<Int8Type, Int32Type>::new();

        dict_builder.append(1)?;
        dict_builder.append_null();
        dict_builder.append(2)?;
        dict_builder.append(5)?;

        let a = dict_builder.finish();

        let expected: PrimitiveArray<Int32Type> =
            PrimitiveArray::from(vec![Some(2), None, Some(3), Some(6)]);

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Plus,
            ScalarValue::Dictionary(
                Box::new(DataType::Int8),
                Box::new(ScalarValue::Int32(Some(1))),
            ),
            Arc::new(expected),
        )?;

        Ok(())
    }

    #[test]
    fn plus_op_dict_scalar_decimal() -> Result<()> {
        let schema = Schema::new(vec![Field::new(
            "a",
            DataType::Dictionary(
                Box::new(DataType::Int8),
                Box::new(DataType::Decimal128(10, 0)),
            ),
            true,
        )]);

        let value = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[Some(value), None, Some(value - 1), Some(value + 1)],
            10,
            0,
        ));

        let keys = Int8Array::from(vec![0, 2, 1, 3, 0]);
        let a = DictionaryArray::try_new(keys, decimal_array)?;

        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value + 1),
                Some(value),
                None,
                Some(value + 2),
                Some(value + 1),
            ],
            11,
            0,
        ));

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Plus,
            ScalarValue::Dictionary(
                Box::new(DataType::Int8),
                Box::new(ScalarValue::Decimal128(Some(1), 10, 0)),
            ),
            decimal_array,
        )?;

        Ok(())
    }

    #[test]
    fn minus_op() -> Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
        ]));
        let a = Arc::new(Int32Array::from(vec![1, 2, 4, 8, 16]));
        let b = Arc::new(Int32Array::from(vec![1, 2, 3, 4, 5]));

        apply_arithmetic::<Int32Type>(
            Arc::clone(&schema),
            vec![
                Arc::clone(&a) as Arc<dyn Array>,
                Arc::clone(&b) as Arc<dyn Array>,
            ],
            Operator::Minus,
            Int32Array::from(vec![0, 0, 1, 4, 11]),
        )?;

        // should handle have negative values in result (for signed)
        apply_arithmetic::<Int32Type>(
            schema,
            vec![b, a],
            Operator::Minus,
            Int32Array::from(vec![0, 0, -1, -4, -11]),
        )?;

        Ok(())
    }

    #[test]
    fn minus_op_dict() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new(
                "a",
                DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
                true,
            ),
            Field::new(
                "b",
                DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
                true,
            ),
        ]);

        let a = Int32Array::from(vec![1, 2, 3, 4, 5]);
        let keys = Int8Array::from(vec![Some(0), None, Some(1), Some(3), None]);
        let a = DictionaryArray::try_new(keys, Arc::new(a))?;

        let b = Int32Array::from(vec![1, 2, 4, 8, 16]);
        let keys = Int8Array::from(vec![0, 1, 1, 2, 1]);
        let b = DictionaryArray::try_new(keys, Arc::new(b))?;

        apply_arithmetic::<Int32Type>(
            Arc::new(schema),
            vec![Arc::new(a), Arc::new(b)],
            Operator::Minus,
            Int32Array::from(vec![Some(0), None, Some(0), Some(0), None]),
        )?;

        Ok(())
    }

    #[test]
    fn minus_op_dict_decimal() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new(
                "a",
                DataType::Dictionary(
                    Box::new(DataType::Int8),
                    Box::new(DataType::Decimal128(10, 0)),
                ),
                true,
            ),
            Field::new(
                "b",
                DataType::Dictionary(
                    Box::new(DataType::Int8),
                    Box::new(DataType::Decimal128(10, 0)),
                ),
                true,
            ),
        ]);

        let value = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value),
                Some(value + 2),
                Some(value - 1),
                Some(value + 1),
            ],
            10,
            0,
        ));

        let keys = Int8Array::from(vec![Some(0), Some(2), None, Some(3), Some(0)]);
        let a = DictionaryArray::try_new(keys, decimal_array)?;

        let keys = Int8Array::from(vec![Some(0), None, Some(3), Some(2), Some(2)]);
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value + 1),
                Some(value + 3),
                Some(value),
                Some(value + 2),
            ],
            10,
            0,
        ));
        let b = DictionaryArray::try_new(keys, decimal_array)?;

        apply_arithmetic(
            Arc::new(schema),
            vec![Arc::new(a), Arc::new(b)],
            Operator::Minus,
            create_decimal_array(&[Some(-1), None, None, Some(1), Some(0)], 11, 0),
        )?;

        Ok(())
    }

    #[test]
    fn minus_op_scalar() -> Result<()> {
        let schema = Schema::new(vec![Field::new("a", DataType::Int32, false)]);
        let a = Int32Array::from(vec![1, 2, 3, 4, 5]);

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Minus,
            ScalarValue::Int32(Some(1)),
            Arc::new(Int32Array::from(vec![0, 1, 2, 3, 4])),
        )?;

        Ok(())
    }

    #[test]
    fn minus_op_dict_scalar() -> Result<()> {
        let schema = Schema::new(vec![Field::new(
            "a",
            DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
            true,
        )]);

        let mut dict_builder = PrimitiveDictionaryBuilder::<Int8Type, Int32Type>::new();

        dict_builder.append(1)?;
        dict_builder.append_null();
        dict_builder.append(2)?;
        dict_builder.append(5)?;

        let a = dict_builder.finish();

        let expected: PrimitiveArray<Int32Type> =
            PrimitiveArray::from(vec![Some(0), None, Some(1), Some(4)]);

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Minus,
            ScalarValue::Dictionary(
                Box::new(DataType::Int8),
                Box::new(ScalarValue::Int32(Some(1))),
            ),
            Arc::new(expected),
        )?;

        Ok(())
    }

    #[test]
    fn minus_op_dict_scalar_decimal() -> Result<()> {
        let schema = Schema::new(vec![Field::new(
            "a",
            DataType::Dictionary(
                Box::new(DataType::Int8),
                Box::new(DataType::Decimal128(10, 0)),
            ),
            true,
        )]);

        let value = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[Some(value), None, Some(value - 1), Some(value + 1)],
            10,
            0,
        ));

        let keys = Int8Array::from(vec![0, 2, 1, 3, 0]);
        let a = DictionaryArray::try_new(keys, decimal_array)?;

        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value - 1),
                Some(value - 2),
                None,
                Some(value),
                Some(value - 1),
            ],
            11,
            0,
        ));

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Minus,
            ScalarValue::Dictionary(
                Box::new(DataType::Int8),
                Box::new(ScalarValue::Decimal128(Some(1), 10, 0)),
            ),
            decimal_array,
        )?;

        Ok(())
    }

    #[test]
    fn multiply_op() -> Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
        ]));
        let a = Arc::new(Int32Array::from(vec![4, 8, 16, 32, 64]));
        let b = Arc::new(Int32Array::from(vec![2, 4, 8, 16, 32]));

        apply_arithmetic::<Int32Type>(
            schema,
            vec![a, b],
            Operator::Multiply,
            Int32Array::from(vec![8, 32, 128, 512, 2048]),
        )?;

        Ok(())
    }

    #[test]
    fn multiply_op_dict() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new(
                "a",
                DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
                true,
            ),
            Field::new(
                "b",
                DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
                true,
            ),
        ]);

        let a = Int32Array::from(vec![1, 2, 3, 4, 5]);
        let keys = Int8Array::from(vec![Some(0), None, Some(1), Some(3), None]);
        let a = DictionaryArray::try_new(keys, Arc::new(a))?;

        let b = Int32Array::from(vec![1, 2, 4, 8, 16]);
        let keys = Int8Array::from(vec![0, 1, 1, 2, 1]);
        let b = DictionaryArray::try_new(keys, Arc::new(b))?;

        apply_arithmetic::<Int32Type>(
            Arc::new(schema),
            vec![Arc::new(a), Arc::new(b)],
            Operator::Multiply,
            Int32Array::from(vec![Some(1), None, Some(4), Some(16), None]),
        )?;

        Ok(())
    }

    #[test]
    fn multiply_op_dict_decimal() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new(
                "a",
                DataType::Dictionary(
                    Box::new(DataType::Int8),
                    Box::new(DataType::Decimal128(10, 0)),
                ),
                true,
            ),
            Field::new(
                "b",
                DataType::Dictionary(
                    Box::new(DataType::Int8),
                    Box::new(DataType::Decimal128(10, 0)),
                ),
                true,
            ),
        ]);

        let value = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value),
                Some(value + 2),
                Some(value - 1),
                Some(value + 1),
            ],
            10,
            0,
        )) as ArrayRef;

        let keys = Int8Array::from(vec![Some(0), Some(2), None, Some(3), Some(0)]);
        let a = DictionaryArray::try_new(keys, decimal_array)?;

        let keys = Int8Array::from(vec![Some(0), None, Some(3), Some(2), Some(2)]);
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value + 1),
                Some(value + 3),
                Some(value),
                Some(value + 2),
            ],
            10,
            0,
        ));
        let b = DictionaryArray::try_new(keys, decimal_array)?;

        apply_arithmetic(
            Arc::new(schema),
            vec![Arc::new(a), Arc::new(b)],
            Operator::Multiply,
            create_decimal_array(
                &[Some(15252), None, None, Some(15252), Some(15129)],
                21,
                0,
            ),
        )?;

        Ok(())
    }

    #[test]
    fn multiply_op_scalar() -> Result<()> {
        let schema = Schema::new(vec![Field::new("a", DataType::Int32, false)]);
        let a = Int32Array::from(vec![1, 2, 3, 4, 5]);

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Multiply,
            ScalarValue::Int32(Some(2)),
            Arc::new(Int32Array::from(vec![2, 4, 6, 8, 10])),
        )?;

        Ok(())
    }

    #[test]
    fn multiply_op_dict_scalar() -> Result<()> {
        let schema = Schema::new(vec![Field::new(
            "a",
            DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
            true,
        )]);

        let mut dict_builder = PrimitiveDictionaryBuilder::<Int8Type, Int32Type>::new();

        dict_builder.append(1)?;
        dict_builder.append_null();
        dict_builder.append(2)?;
        dict_builder.append(5)?;

        let a = dict_builder.finish();

        let expected: PrimitiveArray<Int32Type> =
            PrimitiveArray::from(vec![Some(2), None, Some(4), Some(10)]);

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Multiply,
            ScalarValue::Dictionary(
                Box::new(DataType::Int8),
                Box::new(ScalarValue::Int32(Some(2))),
            ),
            Arc::new(expected),
        )?;

        Ok(())
    }

    #[test]
    fn multiply_op_dict_scalar_decimal() -> Result<()> {
        let schema = Schema::new(vec![Field::new(
            "a",
            DataType::Dictionary(
                Box::new(DataType::Int8),
                Box::new(DataType::Decimal128(10, 0)),
            ),
            true,
        )]);

        let value = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[Some(value), None, Some(value - 1), Some(value + 1)],
            10,
            0,
        ));

        let keys = Int8Array::from(vec![0, 2, 1, 3, 0]);
        let a = DictionaryArray::try_new(keys, decimal_array)?;

        let decimal_array = Arc::new(create_decimal_array(
            &[Some(246), Some(244), None, Some(248), Some(246)],
            21,
            0,
        ));

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Multiply,
            ScalarValue::Dictionary(
                Box::new(DataType::Int8),
                Box::new(ScalarValue::Decimal128(Some(2), 10, 0)),
            ),
            decimal_array,
        )?;

        Ok(())
    }

    #[test]
    fn divide_op() -> Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
        ]));
        let a = Arc::new(Int32Array::from(vec![8, 32, 128, 512, 2048]));
        let b = Arc::new(Int32Array::from(vec![2, 4, 8, 16, 32]));

        apply_arithmetic::<Int32Type>(
            schema,
            vec![a, b],
            Operator::Divide,
            Int32Array::from(vec![4, 8, 16, 32, 64]),
        )?;

        Ok(())
    }

    #[test]
    fn divide_op_dict() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new(
                "a",
                DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
                true,
            ),
            Field::new(
                "b",
                DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
                true,
            ),
        ]);

        let mut dict_builder = PrimitiveDictionaryBuilder::<Int8Type, Int32Type>::new();

        dict_builder.append(1)?;
        dict_builder.append_null();
        dict_builder.append(2)?;
        dict_builder.append(5)?;
        dict_builder.append(0)?;

        let a = dict_builder.finish();

        let b = Int32Array::from(vec![1, 2, 4, 8, 16]);
        let keys = Int8Array::from(vec![0, 1, 1, 2, 1]);
        let b = DictionaryArray::try_new(keys, Arc::new(b))?;

        apply_arithmetic::<Int32Type>(
            Arc::new(schema),
            vec![Arc::new(a), Arc::new(b)],
            Operator::Divide,
            Int32Array::from(vec![Some(1), None, Some(1), Some(1), Some(0)]),
        )?;

        Ok(())
    }

    #[test]
    fn divide_op_dict_decimal() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new(
                "a",
                DataType::Dictionary(
                    Box::new(DataType::Int8),
                    Box::new(DataType::Decimal128(10, 0)),
                ),
                true,
            ),
            Field::new(
                "b",
                DataType::Dictionary(
                    Box::new(DataType::Int8),
                    Box::new(DataType::Decimal128(10, 0)),
                ),
                true,
            ),
        ]);

        let value = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value),
                Some(value + 2),
                Some(value - 1),
                Some(value + 1),
            ],
            10,
            0,
        ));

        let keys = Int8Array::from(vec![Some(0), Some(2), None, Some(3), Some(0)]);
        let a = DictionaryArray::try_new(keys, decimal_array)?;

        let keys = Int8Array::from(vec![Some(0), None, Some(3), Some(2), Some(2)]);
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value + 1),
                Some(value + 3),
                Some(value),
                Some(value + 2),
            ],
            10,
            0,
        ));
        let b = DictionaryArray::try_new(keys, decimal_array)?;

        apply_arithmetic(
            Arc::new(schema),
            vec![Arc::new(a), Arc::new(b)],
            Operator::Divide,
            create_decimal_array(
                &[
                    Some(9919), // 0.9919
                    None,
                    None,
                    Some(10081), // 1.0081
                    Some(10000), // 1.0
                ],
                14,
                4,
            ),
        )?;

        Ok(())
    }

    #[test]
    fn divide_op_scalar() -> Result<()> {
        let schema = Schema::new(vec![Field::new("a", DataType::Int32, false)]);
        let a = Int32Array::from(vec![1, 2, 3, 4, 5]);

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Divide,
            ScalarValue::Int32(Some(2)),
            Arc::new(Int32Array::from(vec![0, 1, 1, 2, 2])),
        )?;

        Ok(())
    }

    #[test]
    fn divide_op_dict_scalar() -> Result<()> {
        let schema = Schema::new(vec![Field::new(
            "a",
            DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
            true,
        )]);

        let mut dict_builder = PrimitiveDictionaryBuilder::<Int8Type, Int32Type>::new();

        dict_builder.append(1)?;
        dict_builder.append_null();
        dict_builder.append(2)?;
        dict_builder.append(5)?;

        let a = dict_builder.finish();

        let expected: PrimitiveArray<Int32Type> =
            PrimitiveArray::from(vec![Some(0), None, Some(1), Some(2)]);

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Divide,
            ScalarValue::Dictionary(
                Box::new(DataType::Int8),
                Box::new(ScalarValue::Int32(Some(2))),
            ),
            Arc::new(expected),
        )?;

        Ok(())
    }

    #[test]
    fn divide_op_dict_scalar_decimal() -> Result<()> {
        let schema = Schema::new(vec![Field::new(
            "a",
            DataType::Dictionary(
                Box::new(DataType::Int8),
                Box::new(DataType::Decimal128(10, 0)),
            ),
            true,
        )]);

        let value = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[Some(value), None, Some(value - 1), Some(value + 1)],
            10,
            0,
        ));

        let keys = Int8Array::from(vec![0, 2, 1, 3, 0]);
        let a = DictionaryArray::try_new(keys, decimal_array)?;

        let decimal_array = Arc::new(create_decimal_array(
            &[Some(615000), Some(610000), None, Some(620000), Some(615000)],
            14,
            4,
        ));

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Divide,
            ScalarValue::Dictionary(
                Box::new(DataType::Int8),
                Box::new(ScalarValue::Decimal128(Some(2), 10, 0)),
            ),
            decimal_array,
        )?;

        Ok(())
    }

    #[test]
    fn modulus_op() -> Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
        ]));
        let a = Arc::new(Int32Array::from(vec![8, 32, 128, 512, 2048]));
        let b = Arc::new(Int32Array::from(vec![2, 4, 7, 14, 32]));

        apply_arithmetic::<Int32Type>(
            schema,
            vec![a, b],
            Operator::Modulo,
            Int32Array::from(vec![0, 0, 2, 8, 0]),
        )?;

        Ok(())
    }

    #[test]
    fn modulus_op_dict() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new(
                "a",
                DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
                true,
            ),
            Field::new(
                "b",
                DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
                true,
            ),
        ]);

        let mut dict_builder = PrimitiveDictionaryBuilder::<Int8Type, Int32Type>::new();

        dict_builder.append(1)?;
        dict_builder.append_null();
        dict_builder.append(2)?;
        dict_builder.append(5)?;
        dict_builder.append(0)?;

        let a = dict_builder.finish();

        let b = Int32Array::from(vec![1, 2, 4, 8, 16]);
        let keys = Int8Array::from(vec![0, 1, 1, 2, 1]);
        let b = DictionaryArray::try_new(keys, Arc::new(b))?;

        apply_arithmetic::<Int32Type>(
            Arc::new(schema),
            vec![Arc::new(a), Arc::new(b)],
            Operator::Modulo,
            Int32Array::from(vec![Some(0), None, Some(0), Some(1), Some(0)]),
        )?;

        Ok(())
    }

    #[test]
    fn modulus_op_dict_decimal() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new(
                "a",
                DataType::Dictionary(
                    Box::new(DataType::Int8),
                    Box::new(DataType::Decimal128(10, 0)),
                ),
                true,
            ),
            Field::new(
                "b",
                DataType::Dictionary(
                    Box::new(DataType::Int8),
                    Box::new(DataType::Decimal128(10, 0)),
                ),
                true,
            ),
        ]);

        let value = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value),
                Some(value + 2),
                Some(value - 1),
                Some(value + 1),
            ],
            10,
            0,
        ));

        let keys = Int8Array::from(vec![Some(0), Some(2), None, Some(3), Some(0)]);
        let a = DictionaryArray::try_new(keys, decimal_array)?;

        let keys = Int8Array::from(vec![Some(0), None, Some(3), Some(2), Some(2)]);
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value + 1),
                Some(value + 3),
                Some(value),
                Some(value + 2),
            ],
            10,
            0,
        ));
        let b = DictionaryArray::try_new(keys, decimal_array)?;

        apply_arithmetic(
            Arc::new(schema),
            vec![Arc::new(a), Arc::new(b)],
            Operator::Modulo,
            create_decimal_array(&[Some(123), None, None, Some(1), Some(0)], 10, 0),
        )?;

        Ok(())
    }

    #[test]
    fn modulus_op_scalar() -> Result<()> {
        let schema = Schema::new(vec![Field::new("a", DataType::Int32, false)]);
        let a = Int32Array::from(vec![1, 2, 3, 4, 5]);

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Modulo,
            ScalarValue::Int32(Some(2)),
            Arc::new(Int32Array::from(vec![1, 0, 1, 0, 1])),
        )?;

        Ok(())
    }

    #[test]
    fn modules_op_dict_scalar() -> Result<()> {
        let schema = Schema::new(vec![Field::new(
            "a",
            DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Int32)),
            true,
        )]);

        let mut dict_builder = PrimitiveDictionaryBuilder::<Int8Type, Int32Type>::new();

        dict_builder.append(1)?;
        dict_builder.append_null();
        dict_builder.append(2)?;
        dict_builder.append(5)?;

        let a = dict_builder.finish();

        let expected: PrimitiveArray<Int32Type> =
            PrimitiveArray::from(vec![Some(1), None, Some(0), Some(1)]);

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Modulo,
            ScalarValue::Dictionary(
                Box::new(DataType::Int8),
                Box::new(ScalarValue::Int32(Some(2))),
            ),
            Arc::new(expected),
        )?;

        Ok(())
    }

    #[test]
    fn modulus_op_dict_scalar_decimal() -> Result<()> {
        let schema = Schema::new(vec![Field::new(
            "a",
            DataType::Dictionary(
                Box::new(DataType::Int8),
                Box::new(DataType::Decimal128(10, 0)),
            ),
            true,
        )]);

        let value = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[Some(value), None, Some(value - 1), Some(value + 1)],
            10,
            0,
        ));

        let keys = Int8Array::from(vec![0, 2, 1, 3, 0]);
        let a = DictionaryArray::try_new(keys, decimal_array)?;

        let decimal_array = Arc::new(create_decimal_array(
            &[Some(1), Some(0), None, Some(0), Some(1)],
            10,
            0,
        ));

        apply_arithmetic_scalar(
            Arc::new(schema),
            vec![Arc::new(a)],
            Operator::Modulo,
            ScalarValue::Dictionary(
                Box::new(DataType::Int8),
                Box::new(ScalarValue::Decimal128(Some(2), 10, 0)),
            ),
            decimal_array,
        )?;

        Ok(())
    }

    fn apply_arithmetic<T: ArrowNumericType>(
        schema: SchemaRef,
        data: Vec<ArrayRef>,
        op: Operator,
        expected: PrimitiveArray<T>,
    ) -> Result<()> {
        let arithmetic_op =
            binary_op(col("a", &schema)?, op, col("b", &schema)?, &schema)?;
        let batch = RecordBatch::try_new(schema, data)?;
        let result = arithmetic_op
            .evaluate(&batch)?
            .into_array(batch.num_rows())
            .expect("Failed to convert to array");

        assert_eq!(result.as_ref(), &expected);
        Ok(())
    }

    fn apply_arithmetic_scalar(
        schema: SchemaRef,
        data: Vec<ArrayRef>,
        op: Operator,
        literal: ScalarValue,
        expected: ArrayRef,
    ) -> Result<()> {
        let lit = Arc::new(Literal::new(literal));
        let arithmetic_op = binary_op(col("a", &schema)?, op, lit, &schema)?;
        let batch = RecordBatch::try_new(schema, data)?;
        let result = arithmetic_op
            .evaluate(&batch)?
            .into_array(batch.num_rows())
            .expect("Failed to convert to array");

        assert_eq!(&result, &expected);
        Ok(())
    }

    fn apply_logic_op(
        schema: &SchemaRef,
        left: &ArrayRef,
        right: &ArrayRef,
        op: Operator,
        expected: BooleanArray,
    ) -> Result<()> {
        let op = binary_op(col("a", schema)?, op, col("b", schema)?, schema)?;
        let data: Vec<ArrayRef> = vec![Arc::clone(left), Arc::clone(right)];
        let batch = RecordBatch::try_new(Arc::clone(schema), data)?;
        let result = op
            .evaluate(&batch)?
            .into_array(batch.num_rows())
            .expect("Failed to convert to array");

        assert_eq!(result.as_ref(), &expected);
        Ok(())
    }

    // Test `scalar <op> arr` produces expected
    fn apply_logic_op_scalar_arr(
        schema: &SchemaRef,
        scalar: &ScalarValue,
        arr: &ArrayRef,
        op: Operator,
        expected: &BooleanArray,
    ) -> Result<()> {
        let scalar = lit(scalar.clone());
        let op = binary_op(scalar, op, col("a", schema)?, schema)?;
        let batch = RecordBatch::try_new(Arc::clone(schema), vec![Arc::clone(arr)])?;
        let result = op
            .evaluate(&batch)?
            .into_array(batch.num_rows())
            .expect("Failed to convert to array");
        assert_eq!(result.as_ref(), expected);

        Ok(())
    }

    // Test `arr <op> scalar` produces expected
    fn apply_logic_op_arr_scalar(
        schema: &SchemaRef,
        arr: &ArrayRef,
        scalar: &ScalarValue,
        op: Operator,
        expected: &BooleanArray,
    ) -> Result<()> {
        let scalar = lit(scalar.clone());
        let op = binary_op(col("a", schema)?, op, scalar, schema)?;
        let batch = RecordBatch::try_new(Arc::clone(schema), vec![Arc::clone(arr)])?;
        let result = op
            .evaluate(&batch)?
            .into_array(batch.num_rows())
            .expect("Failed to convert to array");
        assert_eq!(result.as_ref(), expected);

        Ok(())
    }

    #[test]
    fn and_with_nulls_op() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Boolean, true),
            Field::new("b", DataType::Boolean, true),
        ]);
        let a = Arc::new(BooleanArray::from(vec![
            Some(true),
            Some(false),
            None,
            Some(true),
            Some(false),
            None,
            Some(true),
            Some(false),
            None,
        ])) as ArrayRef;
        let b = Arc::new(BooleanArray::from(vec![
            Some(true),
            Some(true),
            Some(true),
            Some(false),
            Some(false),
            Some(false),
            None,
            None,
            None,
        ])) as ArrayRef;

        let expected = BooleanArray::from(vec![
            Some(true),
            Some(false),
            None,
            Some(false),
            Some(false),
            Some(false),
            None,
            Some(false),
            None,
        ]);
        apply_logic_op(&Arc::new(schema), &a, &b, Operator::And, expected)?;

        Ok(())
    }

    #[test]
    fn regex_with_nulls() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Utf8, true),
            Field::new("b", DataType::Utf8, true),
        ]);
        let a = Arc::new(StringArray::from(vec![
            Some("abc"),
            None,
            Some("abc"),
            None,
            Some("abc"),
        ])) as ArrayRef;
        let b = Arc::new(StringArray::from(vec![
            Some("^a"),
            Some("^A"),
            None,
            None,
            Some("^(b|c)"),
        ])) as ArrayRef;

        let regex_expected =
            BooleanArray::from(vec![Some(true), None, None, None, Some(false)]);
        let regex_not_expected =
            BooleanArray::from(vec![Some(false), None, None, None, Some(true)]);
        apply_logic_op(
            &Arc::new(schema.clone()),
            &a,
            &b,
            Operator::RegexMatch,
            regex_expected.clone(),
        )?;
        apply_logic_op(
            &Arc::new(schema.clone()),
            &a,
            &b,
            Operator::RegexIMatch,
            regex_expected.clone(),
        )?;
        apply_logic_op(
            &Arc::new(schema.clone()),
            &a,
            &b,
            Operator::RegexNotMatch,
            regex_not_expected.clone(),
        )?;
        apply_logic_op(
            &Arc::new(schema),
            &a,
            &b,
            Operator::RegexNotIMatch,
            regex_not_expected.clone(),
        )?;

        let schema = Schema::new(vec![
            Field::new("a", DataType::LargeUtf8, true),
            Field::new("b", DataType::LargeUtf8, true),
        ]);
        let a = Arc::new(LargeStringArray::from(vec![
            Some("abc"),
            None,
            Some("abc"),
            None,
            Some("abc"),
        ])) as ArrayRef;
        let b = Arc::new(LargeStringArray::from(vec![
            Some("^a"),
            Some("^A"),
            None,
            None,
            Some("^(b|c)"),
        ])) as ArrayRef;

        apply_logic_op(
            &Arc::new(schema.clone()),
            &a,
            &b,
            Operator::RegexMatch,
            regex_expected.clone(),
        )?;
        apply_logic_op(
            &Arc::new(schema.clone()),
            &a,
            &b,
            Operator::RegexIMatch,
            regex_expected,
        )?;
        apply_logic_op(
            &Arc::new(schema.clone()),
            &a,
            &b,
            Operator::RegexNotMatch,
            regex_not_expected.clone(),
        )?;
        apply_logic_op(
            &Arc::new(schema),
            &a,
            &b,
            Operator::RegexNotIMatch,
            regex_not_expected,
        )?;

        Ok(())
    }

    #[test]
    fn or_with_nulls_op() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Boolean, true),
            Field::new("b", DataType::Boolean, true),
        ]);
        let a = Arc::new(BooleanArray::from(vec![
            Some(true),
            Some(false),
            None,
            Some(true),
            Some(false),
            None,
            Some(true),
            Some(false),
            None,
        ])) as ArrayRef;
        let b = Arc::new(BooleanArray::from(vec![
            Some(true),
            Some(true),
            Some(true),
            Some(false),
            Some(false),
            Some(false),
            None,
            None,
            None,
        ])) as ArrayRef;

        let expected = BooleanArray::from(vec![
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            Some(false),
            None,
            Some(true),
            None,
            None,
        ]);
        apply_logic_op(&Arc::new(schema), &a, &b, Operator::Or, expected)?;

        Ok(())
    }

    /// Returns (schema, a: BooleanArray, b: BooleanArray) with all possible inputs
    ///
    /// a: [true, true, true,  NULL, NULL, NULL,  false, false, false]
    /// b: [true, NULL, false, true, NULL, false, true,  NULL,  false]
    fn bool_test_arrays() -> (SchemaRef, ArrayRef, ArrayRef) {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Boolean, true),
            Field::new("b", DataType::Boolean, true),
        ]);
        let a: BooleanArray = [
            Some(true),
            Some(true),
            Some(true),
            None,
            None,
            None,
            Some(false),
            Some(false),
            Some(false),
        ]
        .iter()
        .collect();
        let b: BooleanArray = [
            Some(true),
            None,
            Some(false),
            Some(true),
            None,
            Some(false),
            Some(true),
            None,
            Some(false),
        ]
        .iter()
        .collect();
        (Arc::new(schema), Arc::new(a), Arc::new(b))
    }

    /// Returns (schema, BooleanArray) with [true, NULL, false]
    fn scalar_bool_test_array() -> (SchemaRef, ArrayRef) {
        let schema = Schema::new(vec![Field::new("a", DataType::Boolean, true)]);
        let a: BooleanArray = [Some(true), None, Some(false)].iter().collect();
        (Arc::new(schema), Arc::new(a))
    }

    #[test]
    fn eq_op_bool() {
        let (schema, a, b) = bool_test_arrays();
        let expected = [
            Some(true),
            None,
            Some(false),
            None,
            None,
            None,
            Some(false),
            None,
            Some(true),
        ]
        .iter()
        .collect();
        apply_logic_op(&schema, &a, &b, Operator::Eq, expected).unwrap();
    }

    #[test]
    fn eq_op_bool_scalar() {
        let (schema, a) = scalar_bool_test_array();
        let expected = [Some(true), None, Some(false)].iter().collect();
        apply_logic_op_scalar_arr(
            &schema,
            &ScalarValue::from(true),
            &a,
            Operator::Eq,
            &expected,
        )
        .unwrap();
        apply_logic_op_arr_scalar(
            &schema,
            &a,
            &ScalarValue::from(true),
            Operator::Eq,
            &expected,
        )
        .unwrap();

        let expected = [Some(false), None, Some(true)].iter().collect();
        apply_logic_op_scalar_arr(
            &schema,
            &ScalarValue::from(false),
            &a,
            Operator::Eq,
            &expected,
        )
        .unwrap();
        apply_logic_op_arr_scalar(
            &schema,
            &a,
            &ScalarValue::from(false),
            Operator::Eq,
            &expected,
        )
        .unwrap();
    }

    #[test]
    fn neq_op_bool() {
        let (schema, a, b) = bool_test_arrays();
        let expected = [
            Some(false),
            None,
            Some(true),
            None,
            None,
            None,
            Some(true),
            None,
            Some(false),
        ]
        .iter()
        .collect();
        apply_logic_op(&schema, &a, &b, Operator::NotEq, expected).unwrap();
    }

    #[test]
    fn neq_op_bool_scalar() {
        let (schema, a) = scalar_bool_test_array();
        let expected = [Some(false), None, Some(true)].iter().collect();
        apply_logic_op_scalar_arr(
            &schema,
            &ScalarValue::from(true),
            &a,
            Operator::NotEq,
            &expected,
        )
        .unwrap();
        apply_logic_op_arr_scalar(
            &schema,
            &a,
            &ScalarValue::from(true),
            Operator::NotEq,
            &expected,
        )
        .unwrap();

        let expected = [Some(true), None, Some(false)].iter().collect();
        apply_logic_op_scalar_arr(
            &schema,
            &ScalarValue::from(false),
            &a,
            Operator::NotEq,
            &expected,
        )
        .unwrap();
        apply_logic_op_arr_scalar(
            &schema,
            &a,
            &ScalarValue::from(false),
            Operator::NotEq,
            &expected,
        )
        .unwrap();
    }

    #[test]
    fn lt_op_bool() {
        let (schema, a, b) = bool_test_arrays();
        let expected = [
            Some(false),
            None,
            Some(false),
            None,
            None,
            None,
            Some(true),
            None,
            Some(false),
        ]
        .iter()
        .collect();
        apply_logic_op(&schema, &a, &b, Operator::Lt, expected).unwrap();
    }

    #[test]
    fn lt_op_bool_scalar() {
        let (schema, a) = scalar_bool_test_array();
        let expected = [Some(false), None, Some(false)].iter().collect();
        apply_logic_op_scalar_arr(
            &schema,
            &ScalarValue::from(true),
            &a,
            Operator::Lt,
            &expected,
        )
        .unwrap();

        let expected = [Some(false), None, Some(true)].iter().collect();
        apply_logic_op_arr_scalar(
            &schema,
            &a,
            &ScalarValue::from(true),
            Operator::Lt,
            &expected,
        )
        .unwrap();

        let expected = [Some(true), None, Some(false)].iter().collect();
        apply_logic_op_scalar_arr(
            &schema,
            &ScalarValue::from(false),
            &a,
            Operator::Lt,
            &expected,
        )
        .unwrap();

        let expected = [Some(false), None, Some(false)].iter().collect();
        apply_logic_op_arr_scalar(
            &schema,
            &a,
            &ScalarValue::from(false),
            Operator::Lt,
            &expected,
        )
        .unwrap();
    }

    #[test]
    fn lt_eq_op_bool() {
        let (schema, a, b) = bool_test_arrays();
        let expected = [
            Some(true),
            None,
            Some(false),
            None,
            None,
            None,
            Some(true),
            None,
            Some(true),
        ]
        .iter()
        .collect();
        apply_logic_op(&schema, &a, &b, Operator::LtEq, expected).unwrap();
    }

    #[test]
    fn lt_eq_op_bool_scalar() {
        let (schema, a) = scalar_bool_test_array();
        let expected = [Some(true), None, Some(false)].iter().collect();
        apply_logic_op_scalar_arr(
            &schema,
            &ScalarValue::from(true),
            &a,
            Operator::LtEq,
            &expected,
        )
        .unwrap();

        let expected = [Some(true), None, Some(true)].iter().collect();
        apply_logic_op_arr_scalar(
            &schema,
            &a,
            &ScalarValue::from(true),
            Operator::LtEq,
            &expected,
        )
        .unwrap();

        let expected = [Some(true), None, Some(true)].iter().collect();
        apply_logic_op_scalar_arr(
            &schema,
            &ScalarValue::from(false),
            &a,
            Operator::LtEq,
            &expected,
        )
        .unwrap();

        let expected = [Some(false), None, Some(true)].iter().collect();
        apply_logic_op_arr_scalar(
            &schema,
            &a,
            &ScalarValue::from(false),
            Operator::LtEq,
            &expected,
        )
        .unwrap();
    }

    #[test]
    fn gt_op_bool() {
        let (schema, a, b) = bool_test_arrays();
        let expected = [
            Some(false),
            None,
            Some(true),
            None,
            None,
            None,
            Some(false),
            None,
            Some(false),
        ]
        .iter()
        .collect();
        apply_logic_op(&schema, &a, &b, Operator::Gt, expected).unwrap();
    }

    #[test]
    fn gt_op_bool_scalar() {
        let (schema, a) = scalar_bool_test_array();
        let expected = [Some(false), None, Some(true)].iter().collect();
        apply_logic_op_scalar_arr(
            &schema,
            &ScalarValue::from(true),
            &a,
            Operator::Gt,
            &expected,
        )
        .unwrap();

        let expected = [Some(false), None, Some(false)].iter().collect();
        apply_logic_op_arr_scalar(
            &schema,
            &a,
            &ScalarValue::from(true),
            Operator::Gt,
            &expected,
        )
        .unwrap();

        let expected = [Some(false), None, Some(false)].iter().collect();
        apply_logic_op_scalar_arr(
            &schema,
            &ScalarValue::from(false),
            &a,
            Operator::Gt,
            &expected,
        )
        .unwrap();

        let expected = [Some(true), None, Some(false)].iter().collect();
        apply_logic_op_arr_scalar(
            &schema,
            &a,
            &ScalarValue::from(false),
            Operator::Gt,
            &expected,
        )
        .unwrap();
    }

    #[test]
    fn gt_eq_op_bool() {
        let (schema, a, b) = bool_test_arrays();
        let expected = [
            Some(true),
            None,
            Some(true),
            None,
            None,
            None,
            Some(false),
            None,
            Some(true),
        ]
        .iter()
        .collect();
        apply_logic_op(&schema, &a, &b, Operator::GtEq, expected).unwrap();
    }

    #[test]
    fn gt_eq_op_bool_scalar() {
        let (schema, a) = scalar_bool_test_array();
        let expected = [Some(true), None, Some(true)].iter().collect();
        apply_logic_op_scalar_arr(
            &schema,
            &ScalarValue::from(true),
            &a,
            Operator::GtEq,
            &expected,
        )
        .unwrap();

        let expected = [Some(true), None, Some(false)].iter().collect();
        apply_logic_op_arr_scalar(
            &schema,
            &a,
            &ScalarValue::from(true),
            Operator::GtEq,
            &expected,
        )
        .unwrap();

        let expected = [Some(false), None, Some(true)].iter().collect();
        apply_logic_op_scalar_arr(
            &schema,
            &ScalarValue::from(false),
            &a,
            Operator::GtEq,
            &expected,
        )
        .unwrap();

        let expected = [Some(true), None, Some(true)].iter().collect();
        apply_logic_op_arr_scalar(
            &schema,
            &a,
            &ScalarValue::from(false),
            Operator::GtEq,
            &expected,
        )
        .unwrap();
    }

    #[test]
    fn is_distinct_from_op_bool() {
        let (schema, a, b) = bool_test_arrays();
        let expected = [
            Some(false),
            Some(true),
            Some(true),
            Some(true),
            Some(false),
            Some(true),
            Some(true),
            Some(true),
            Some(false),
        ]
        .iter()
        .collect();
        apply_logic_op(&schema, &a, &b, Operator::IsDistinctFrom, expected).unwrap();
    }

    #[test]
    fn is_not_distinct_from_op_bool() {
        let (schema, a, b) = bool_test_arrays();
        let expected = [
            Some(true),
            Some(false),
            Some(false),
            Some(false),
            Some(true),
            Some(false),
            Some(false),
            Some(false),
            Some(true),
        ]
        .iter()
        .collect();
        apply_logic_op(&schema, &a, &b, Operator::IsNotDistinctFrom, expected).unwrap();
    }

    #[test]
    fn relatively_deeply_nested() {
        // Reproducer for https://github.com/apache/datafusion/issues/419

        // where even relatively shallow binary expressions overflowed
        // the stack in debug builds

        let input: Vec<_> = vec![1, 2, 3, 4, 5].into_iter().map(Some).collect();
        let a: Int32Array = input.iter().collect();

        let batch = RecordBatch::try_from_iter(vec![("a", Arc::new(a) as _)]).unwrap();
        let schema = batch.schema();

        // build a left deep tree ((((a + a) + a) + a ....
        let tree_depth: i32 = 100;
        let expr = (0..tree_depth)
            .map(|_| col("a", schema.as_ref()).unwrap())
            .reduce(|l, r| binary(l, Operator::Plus, r, &schema).unwrap())
            .unwrap();

        let result = expr
            .evaluate(&batch)
            .expect("evaluation")
            .into_array(batch.num_rows())
            .expect("Failed to convert to array");

        let expected: Int32Array = input
            .into_iter()
            .map(|i| i.map(|i| i * tree_depth))
            .collect();
        assert_eq!(result.as_ref(), &expected);
    }

    fn create_decimal_array(
        array: &[Option<i128>],
        precision: u8,
        scale: i8,
    ) -> Decimal128Array {
        let mut decimal_builder = Decimal128Builder::with_capacity(array.len());
        for value in array.iter().copied() {
            decimal_builder.append_option(value)
        }
        decimal_builder
            .finish()
            .with_precision_and_scale(precision, scale)
            .unwrap()
    }

    #[test]
    fn comparison_dict_decimal_scalar_expr_test() -> Result<()> {
        // scalar of decimal compare with dictionary decimal array
        let value_i128 = 123;
        let decimal_scalar = ScalarValue::Dictionary(
            Box::new(DataType::Int8),
            Box::new(ScalarValue::Decimal128(Some(value_i128), 25, 3)),
        );
        let schema = Arc::new(Schema::new(vec![Field::new(
            "a",
            DataType::Dictionary(
                Box::new(DataType::Int8),
                Box::new(DataType::Decimal128(25, 3)),
            ),
            true,
        )]));
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value_i128),
                None,
                Some(value_i128 - 1),
                Some(value_i128 + 1),
            ],
            25,
            3,
        ));

        let keys = Int8Array::from(vec![Some(0), None, Some(2), Some(3)]);
        let dictionary =
            Arc::new(DictionaryArray::try_new(keys, decimal_array)?) as ArrayRef;

        // array = scalar
        apply_logic_op_arr_scalar(
            &schema,
            &dictionary,
            &decimal_scalar,
            Operator::Eq,
            &BooleanArray::from(vec![Some(true), None, Some(false), Some(false)]),
        )
        .unwrap();
        // array != scalar
        apply_logic_op_arr_scalar(
            &schema,
            &dictionary,
            &decimal_scalar,
            Operator::NotEq,
            &BooleanArray::from(vec![Some(false), None, Some(true), Some(true)]),
        )
        .unwrap();
        //  array < scalar
        apply_logic_op_arr_scalar(
            &schema,
            &dictionary,
            &decimal_scalar,
            Operator::Lt,
            &BooleanArray::from(vec![Some(false), None, Some(true), Some(false)]),
        )
        .unwrap();

        //  array <= scalar
        apply_logic_op_arr_scalar(
            &schema,
            &dictionary,
            &decimal_scalar,
            Operator::LtEq,
            &BooleanArray::from(vec![Some(true), None, Some(true), Some(false)]),
        )
        .unwrap();
        // array > scalar
        apply_logic_op_arr_scalar(
            &schema,
            &dictionary,
            &decimal_scalar,
            Operator::Gt,
            &BooleanArray::from(vec![Some(false), None, Some(false), Some(true)]),
        )
        .unwrap();

        // array >= scalar
        apply_logic_op_arr_scalar(
            &schema,
            &dictionary,
            &decimal_scalar,
            Operator::GtEq,
            &BooleanArray::from(vec![Some(true), None, Some(false), Some(true)]),
        )
        .unwrap();

        Ok(())
    }

    #[test]
    fn comparison_decimal_expr_test() -> Result<()> {
        // scalar of decimal compare with decimal array
        let value_i128 = 123;
        let decimal_scalar = ScalarValue::Decimal128(Some(value_i128), 25, 3);
        let schema = Arc::new(Schema::new(vec![Field::new(
            "a",
            DataType::Decimal128(25, 3),
            true,
        )]));
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value_i128),
                None,
                Some(value_i128 - 1),
                Some(value_i128 + 1),
            ],
            25,
            3,
        )) as ArrayRef;
        // array = scalar
        apply_logic_op_arr_scalar(
            &schema,
            &decimal_array,
            &decimal_scalar,
            Operator::Eq,
            &BooleanArray::from(vec![Some(true), None, Some(false), Some(false)]),
        )
        .unwrap();
        // array != scalar
        apply_logic_op_arr_scalar(
            &schema,
            &decimal_array,
            &decimal_scalar,
            Operator::NotEq,
            &BooleanArray::from(vec![Some(false), None, Some(true), Some(true)]),
        )
        .unwrap();
        //  array < scalar
        apply_logic_op_arr_scalar(
            &schema,
            &decimal_array,
            &decimal_scalar,
            Operator::Lt,
            &BooleanArray::from(vec![Some(false), None, Some(true), Some(false)]),
        )
        .unwrap();

        //  array <= scalar
        apply_logic_op_arr_scalar(
            &schema,
            &decimal_array,
            &decimal_scalar,
            Operator::LtEq,
            &BooleanArray::from(vec![Some(true), None, Some(true), Some(false)]),
        )
        .unwrap();
        // array > scalar
        apply_logic_op_arr_scalar(
            &schema,
            &decimal_array,
            &decimal_scalar,
            Operator::Gt,
            &BooleanArray::from(vec![Some(false), None, Some(false), Some(true)]),
        )
        .unwrap();

        // array >= scalar
        apply_logic_op_arr_scalar(
            &schema,
            &decimal_array,
            &decimal_scalar,
            Operator::GtEq,
            &BooleanArray::from(vec![Some(true), None, Some(false), Some(true)]),
        )
        .unwrap();

        // scalar of different data type with decimal array
        let decimal_scalar = ScalarValue::Decimal128(Some(123_456), 10, 3);
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Int64, true)]));
        // scalar == array
        apply_logic_op_scalar_arr(
            &schema,
            &decimal_scalar,
            &(Arc::new(Int64Array::from(vec![Some(124), None])) as ArrayRef),
            Operator::Eq,
            &BooleanArray::from(vec![Some(false), None]),
        )
        .unwrap();

        // array != scalar
        apply_logic_op_arr_scalar(
            &schema,
            &(Arc::new(Int64Array::from(vec![Some(123), None, Some(1)])) as ArrayRef),
            &decimal_scalar,
            Operator::NotEq,
            &BooleanArray::from(vec![Some(true), None, Some(true)]),
        )
        .unwrap();

        // array < scalar
        apply_logic_op_arr_scalar(
            &schema,
            &(Arc::new(Int64Array::from(vec![Some(123), None, Some(124)])) as ArrayRef),
            &decimal_scalar,
            Operator::Lt,
            &BooleanArray::from(vec![Some(true), None, Some(false)]),
        )
        .unwrap();

        // array > scalar
        apply_logic_op_arr_scalar(
            &schema,
            &(Arc::new(Int64Array::from(vec![Some(123), None, Some(124)])) as ArrayRef),
            &decimal_scalar,
            Operator::Gt,
            &BooleanArray::from(vec![Some(false), None, Some(true)]),
        )
        .unwrap();

        let schema =
            Arc::new(Schema::new(vec![Field::new("a", DataType::Float64, true)]));
        // array == scalar
        apply_logic_op_arr_scalar(
            &schema,
            &(Arc::new(Float64Array::from(vec![Some(123.456), None, Some(123.457)]))
                as ArrayRef),
            &decimal_scalar,
            Operator::Eq,
            &BooleanArray::from(vec![Some(true), None, Some(false)]),
        )
        .unwrap();

        // array <= scalar
        apply_logic_op_arr_scalar(
            &schema,
            &(Arc::new(Float64Array::from(vec![
                Some(123.456),
                None,
                Some(123.457),
                Some(123.45),
            ])) as ArrayRef),
            &decimal_scalar,
            Operator::LtEq,
            &BooleanArray::from(vec![Some(true), None, Some(false), Some(true)]),
        )
        .unwrap();
        // array >= scalar
        apply_logic_op_arr_scalar(
            &schema,
            &(Arc::new(Float64Array::from(vec![
                Some(123.456),
                None,
                Some(123.457),
                Some(123.45),
            ])) as ArrayRef),
            &decimal_scalar,
            Operator::GtEq,
            &BooleanArray::from(vec![Some(true), None, Some(true), Some(false)]),
        )
        .unwrap();

        let value: i128 = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[Some(value), None, Some(value - 1), Some(value + 1)],
            10,
            0,
        )) as ArrayRef;

        // comparison array op for decimal array
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Decimal128(10, 0), true),
            Field::new("b", DataType::Decimal128(10, 0), true),
        ]));
        let right_decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value - 1),
                Some(value),
                Some(value + 1),
                Some(value + 1),
            ],
            10,
            0,
        )) as ArrayRef;

        apply_logic_op(
            &schema,
            &decimal_array,
            &right_decimal_array,
            Operator::Eq,
            BooleanArray::from(vec![Some(false), None, Some(false), Some(true)]),
        )
        .unwrap();

        apply_logic_op(
            &schema,
            &decimal_array,
            &right_decimal_array,
            Operator::NotEq,
            BooleanArray::from(vec![Some(true), None, Some(true), Some(false)]),
        )
        .unwrap();

        apply_logic_op(
            &schema,
            &decimal_array,
            &right_decimal_array,
            Operator::Lt,
            BooleanArray::from(vec![Some(false), None, Some(true), Some(false)]),
        )
        .unwrap();

        apply_logic_op(
            &schema,
            &decimal_array,
            &right_decimal_array,
            Operator::LtEq,
            BooleanArray::from(vec![Some(false), None, Some(true), Some(true)]),
        )
        .unwrap();

        apply_logic_op(
            &schema,
            &decimal_array,
            &right_decimal_array,
            Operator::Gt,
            BooleanArray::from(vec![Some(true), None, Some(false), Some(false)]),
        )
        .unwrap();

        apply_logic_op(
            &schema,
            &decimal_array,
            &right_decimal_array,
            Operator::GtEq,
            BooleanArray::from(vec![Some(true), None, Some(false), Some(true)]),
        )
        .unwrap();

        // compare decimal array with other array type
        let value: i64 = 123;
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int64, true),
            Field::new("b", DataType::Decimal128(10, 0), true),
        ]));

        let int64_array = Arc::new(Int64Array::from(vec![
            Some(value),
            Some(value - 1),
            Some(value),
            Some(value + 1),
        ])) as ArrayRef;

        // eq: int64array == decimal array
        apply_logic_op(
            &schema,
            &int64_array,
            &decimal_array,
            Operator::Eq,
            BooleanArray::from(vec![Some(true), None, Some(false), Some(true)]),
        )
        .unwrap();
        // neq: int64array != decimal array
        apply_logic_op(
            &schema,
            &int64_array,
            &decimal_array,
            Operator::NotEq,
            BooleanArray::from(vec![Some(false), None, Some(true), Some(false)]),
        )
        .unwrap();

        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Float64, true),
            Field::new("b", DataType::Decimal128(10, 2), true),
        ]));

        let value: i128 = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value), // 1.23
                None,
                Some(value - 1), // 1.22
                Some(value + 1), // 1.24
            ],
            10,
            2,
        )) as ArrayRef;
        let float64_array = Arc::new(Float64Array::from(vec![
            Some(1.23),
            Some(1.22),
            Some(1.23),
            Some(1.24),
        ])) as ArrayRef;
        // lt: float64array < decimal array
        apply_logic_op(
            &schema,
            &float64_array,
            &decimal_array,
            Operator::Lt,
            BooleanArray::from(vec![Some(false), None, Some(false), Some(false)]),
        )
        .unwrap();
        // lt_eq: float64array <= decimal array
        apply_logic_op(
            &schema,
            &float64_array,
            &decimal_array,
            Operator::LtEq,
            BooleanArray::from(vec![Some(true), None, Some(false), Some(true)]),
        )
        .unwrap();
        // gt: float64array > decimal array
        apply_logic_op(
            &schema,
            &float64_array,
            &decimal_array,
            Operator::Gt,
            BooleanArray::from(vec![Some(false), None, Some(true), Some(false)]),
        )
        .unwrap();
        apply_logic_op(
            &schema,
            &float64_array,
            &decimal_array,
            Operator::GtEq,
            BooleanArray::from(vec![Some(true), None, Some(true), Some(true)]),
        )
        .unwrap();
        // is distinct: float64array is distinct decimal array
        // TODO: now we do not refactor the `is distinct or is not distinct` rule of coercion.
        // traced by https://github.com/apache/datafusion/issues/1590
        // the decimal array will be casted to float64array
        apply_logic_op(
            &schema,
            &float64_array,
            &decimal_array,
            Operator::IsDistinctFrom,
            BooleanArray::from(vec![Some(false), Some(true), Some(true), Some(false)]),
        )
        .unwrap();
        // is not distinct
        apply_logic_op(
            &schema,
            &float64_array,
            &decimal_array,
            Operator::IsNotDistinctFrom,
            BooleanArray::from(vec![Some(true), Some(false), Some(false), Some(true)]),
        )
        .unwrap();

        Ok(())
    }

    fn apply_decimal_arithmetic_op(
        schema: &SchemaRef,
        left: &ArrayRef,
        right: &ArrayRef,
        op: Operator,
        expected: ArrayRef,
    ) -> Result<()> {
        let arithmetic_op = binary_op(col("a", schema)?, op, col("b", schema)?, schema)?;
        let data: Vec<ArrayRef> = vec![Arc::clone(left), Arc::clone(right)];
        let batch = RecordBatch::try_new(Arc::clone(schema), data)?;
        let result = arithmetic_op
            .evaluate(&batch)?
            .into_array(batch.num_rows())
            .expect("Failed to convert to array");

        assert_eq!(result.as_ref(), expected.as_ref());
        Ok(())
    }

    #[test]
    fn arithmetic_decimal_expr_test() -> Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Decimal128(10, 2), true),
        ]));
        let value: i128 = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value), // 1.23
                None,
                Some(value - 1), // 1.22
                Some(value + 1), // 1.24
            ],
            10,
            2,
        )) as ArrayRef;
        let int32_array = Arc::new(Int32Array::from(vec![
            Some(123),
            Some(122),
            Some(123),
            Some(124),
        ])) as ArrayRef;

        // add: Int32array add decimal array
        let expect = Arc::new(create_decimal_array(
            &[Some(12423), None, Some(12422), Some(12524)],
            13,
            2,
        )) as ArrayRef;
        apply_decimal_arithmetic_op(
            &schema,
            &int32_array,
            &decimal_array,
            Operator::Plus,
            expect,
        )
        .unwrap();

        // subtract: decimal array subtract int32 array
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Decimal128(10, 2), true),
            Field::new("b", DataType::Int32, true),
        ]));
        let expect = Arc::new(create_decimal_array(
            &[Some(-12177), None, Some(-12178), Some(-12276)],
            13,
            2,
        )) as ArrayRef;
        apply_decimal_arithmetic_op(
            &schema,
            &decimal_array,
            &int32_array,
            Operator::Minus,
            expect,
        )
        .unwrap();

        // multiply: decimal array multiply int32 array
        let expect = Arc::new(create_decimal_array(
            &[Some(15129), None, Some(15006), Some(15376)],
            21,
            2,
        )) as ArrayRef;
        apply_decimal_arithmetic_op(
            &schema,
            &decimal_array,
            &int32_array,
            Operator::Multiply,
            expect,
        )
        .unwrap();

        // divide: int32 array divide decimal array
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Decimal128(10, 2), true),
        ]));
        let expect = Arc::new(create_decimal_array(
            &[Some(1000000), None, Some(1008196), Some(1000000)],
            16,
            4,
        )) as ArrayRef;
        apply_decimal_arithmetic_op(
            &schema,
            &int32_array,
            &decimal_array,
            Operator::Divide,
            expect,
        )
        .unwrap();

        // modulus: int32 array modulus decimal array
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Decimal128(10, 2), true),
        ]));
        let expect = Arc::new(create_decimal_array(
            &[Some(000), None, Some(100), Some(000)],
            10,
            2,
        )) as ArrayRef;
        apply_decimal_arithmetic_op(
            &schema,
            &int32_array,
            &decimal_array,
            Operator::Modulo,
            expect,
        )
        .unwrap();

        Ok(())
    }

    #[test]
    fn arithmetic_decimal_float_expr_test() -> Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Float64, true),
            Field::new("b", DataType::Decimal128(10, 2), true),
        ]));
        let value: i128 = 123;
        let decimal_array = Arc::new(create_decimal_array(
            &[
                Some(value), // 1.23
                None,
                Some(value - 1), // 1.22
                Some(value + 1), // 1.24
            ],
            10,
            2,
        )) as ArrayRef;
        let float64_array = Arc::new(Float64Array::from(vec![
            Some(123.0),
            Some(122.0),
            Some(123.0),
            Some(124.0),
        ])) as ArrayRef;

        // add: float64 array add decimal array
        let expect = Arc::new(Float64Array::from(vec![
            Some(124.23),
            None,
            Some(124.22),
            Some(125.24),
        ])) as ArrayRef;
        apply_decimal_arithmetic_op(
            &schema,
            &float64_array,
            &decimal_array,
            Operator::Plus,
            expect,
        )
        .unwrap();

        // subtract: decimal array subtract float64 array
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Float64, true),
            Field::new("b", DataType::Decimal128(10, 2), true),
        ]));
        let expect = Arc::new(Float64Array::from(vec![
            Some(121.77),
            None,
            Some(121.78),
            Some(122.76),
        ])) as ArrayRef;
        apply_decimal_arithmetic_op(
            &schema,
            &float64_array,
            &decimal_array,
            Operator::Minus,
            expect,
        )
        .unwrap();

        // multiply: decimal array multiply float64 array
        let expect = Arc::new(Float64Array::from(vec![
            Some(151.29),
            None,
            Some(150.06),
            Some(153.76),
        ])) as ArrayRef;
        apply_decimal_arithmetic_op(
            &schema,
            &float64_array,
            &decimal_array,
            Operator::Multiply,
            expect,
        )
        .unwrap();

        // divide: float64 array divide decimal array
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Float64, true),
            Field::new("b", DataType::Decimal128(10, 2), true),
        ]));
        let expect = Arc::new(Float64Array::from(vec![
            Some(100.0),
            None,
            Some(100.81967213114754),
            Some(100.0),
        ])) as ArrayRef;
        apply_decimal_arithmetic_op(
            &schema,
            &float64_array,
            &decimal_array,
            Operator::Divide,
            expect,
        )
        .unwrap();

        // modulus: float64 array modulus decimal array
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Float64, true),
            Field::new("b", DataType::Decimal128(10, 2), true),
        ]));
        let expect = Arc::new(Float64Array::from(vec![
            Some(1.7763568394002505e-15),
            None,
            Some(1.0000000000000027),
            Some(8.881784197001252e-16),
        ])) as ArrayRef;
        apply_decimal_arithmetic_op(
            &schema,
            &float64_array,
            &decimal_array,
            Operator::Modulo,
            expect,
        )
        .unwrap();

        Ok(())
    }

    #[test]
    fn arithmetic_divide_zero() -> Result<()> {
        // other data type
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Int32, true),
        ]));
        let a = Arc::new(Int32Array::from(vec![100]));
        let b = Arc::new(Int32Array::from(vec![0]));

        let err = apply_arithmetic::<Int32Type>(
            schema,
            vec![a, b],
            Operator::Divide,
            Int32Array::from(vec![Some(4), Some(8), Some(16), Some(32), Some(64)]),
        )
        .unwrap_err();

        let _expected = plan_datafusion_err!("Divide by zero");

        assert!(matches!(err, ref _expected), "{err}");

        // decimal
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Decimal128(25, 3), true),
            Field::new("b", DataType::Decimal128(25, 3), true),
        ]));
        let left_decimal_array = Arc::new(create_decimal_array(&[Some(1234567)], 25, 3));
        let right_decimal_array = Arc::new(create_decimal_array(&[Some(0)], 25, 3));

        let err = apply_arithmetic::<Decimal128Type>(
            schema,
            vec![left_decimal_array, right_decimal_array],
            Operator::Divide,
            create_decimal_array(
                &[Some(12345670000000000000000000000000000), None],
                38,
                29,
            ),
        )
        .unwrap_err();

        assert!(matches!(err, ref _expected), "{err}");

        Ok(())
    }

    #[test]
    fn bitwise_array_test() -> Result<()> {
        let left = Arc::new(Int32Array::from(vec![Some(12), None, Some(11)])) as ArrayRef;
        let right =
            Arc::new(Int32Array::from(vec![Some(1), Some(3), Some(7)])) as ArrayRef;
        let mut result = bitwise_and_dyn(Arc::clone(&left), Arc::clone(&right))?;
        let expected = Int32Array::from(vec![Some(0), None, Some(3)]);
        assert_eq!(result.as_ref(), &expected);

        result = bitwise_or_dyn(Arc::clone(&left), Arc::clone(&right))?;
        let expected = Int32Array::from(vec![Some(13), None, Some(15)]);
        assert_eq!(result.as_ref(), &expected);

        result = bitwise_xor_dyn(Arc::clone(&left), Arc::clone(&right))?;
        let expected = Int32Array::from(vec![Some(13), None, Some(12)]);
        assert_eq!(result.as_ref(), &expected);

        let left =
            Arc::new(UInt32Array::from(vec![Some(12), None, Some(11)])) as ArrayRef;
        let right =
            Arc::new(UInt32Array::from(vec![Some(1), Some(3), Some(7)])) as ArrayRef;
        let mut result = bitwise_and_dyn(Arc::clone(&left), Arc::clone(&right))?;
        let expected = UInt32Array::from(vec![Some(0), None, Some(3)]);
        assert_eq!(result.as_ref(), &expected);

        result = bitwise_or_dyn(Arc::clone(&left), Arc::clone(&right))?;
        let expected = UInt32Array::from(vec![Some(13), None, Some(15)]);
        assert_eq!(result.as_ref(), &expected);

        result = bitwise_xor_dyn(Arc::clone(&left), Arc::clone(&right))?;
        let expected = UInt32Array::from(vec![Some(13), None, Some(12)]);
        assert_eq!(result.as_ref(), &expected);

        Ok(())
    }

    #[test]
    fn bitwise_shift_array_test() -> Result<()> {
        let input = Arc::new(Int32Array::from(vec![Some(2), None, Some(10)])) as ArrayRef;
        let modules =
            Arc::new(Int32Array::from(vec![Some(2), Some(4), Some(8)])) as ArrayRef;
        let mut result =
            bitwise_shift_left_dyn(Arc::clone(&input), Arc::clone(&modules))?;

        let expected = Int32Array::from(vec![Some(8), None, Some(2560)]);
        assert_eq!(result.as_ref(), &expected);

        result = bitwise_shift_right_dyn(Arc::clone(&result), Arc::clone(&modules))?;
        assert_eq!(result.as_ref(), &input);

        let input =
            Arc::new(UInt32Array::from(vec![Some(2), None, Some(10)])) as ArrayRef;
        let modules =
            Arc::new(UInt32Array::from(vec![Some(2), Some(4), Some(8)])) as ArrayRef;
        let mut result =
            bitwise_shift_left_dyn(Arc::clone(&input), Arc::clone(&modules))?;

        let expected = UInt32Array::from(vec![Some(8), None, Some(2560)]);
        assert_eq!(result.as_ref(), &expected);

        result = bitwise_shift_right_dyn(Arc::clone(&result), Arc::clone(&modules))?;
        assert_eq!(result.as_ref(), &input);
        Ok(())
    }

    #[test]
    fn bitwise_shift_array_overflow_test() -> Result<()> {
        let input = Arc::new(Int32Array::from(vec![Some(2)])) as ArrayRef;
        let modules = Arc::new(Int32Array::from(vec![Some(100)])) as ArrayRef;
        let result = bitwise_shift_left_dyn(Arc::clone(&input), Arc::clone(&modules))?;

        let expected = Int32Array::from(vec![Some(32)]);
        assert_eq!(result.as_ref(), &expected);

        let input = Arc::new(UInt32Array::from(vec![Some(2)])) as ArrayRef;
        let modules = Arc::new(UInt32Array::from(vec![Some(100)])) as ArrayRef;
        let result = bitwise_shift_left_dyn(Arc::clone(&input), Arc::clone(&modules))?;

        let expected = UInt32Array::from(vec![Some(32)]);
        assert_eq!(result.as_ref(), &expected);
        Ok(())
    }

    #[test]
    fn bitwise_scalar_test() -> Result<()> {
        let left = Arc::new(Int32Array::from(vec![Some(12), None, Some(11)])) as ArrayRef;
        let right = ScalarValue::from(3i32);
        let mut result = bitwise_and_dyn_scalar(&left, right.clone()).unwrap()?;
        let expected = Int32Array::from(vec![Some(0), None, Some(3)]);
        assert_eq!(result.as_ref(), &expected);

        result = bitwise_or_dyn_scalar(&left, right.clone()).unwrap()?;
        let expected = Int32Array::from(vec![Some(15), None, Some(11)]);
        assert_eq!(result.as_ref(), &expected);

        result = bitwise_xor_dyn_scalar(&left, right).unwrap()?;
        let expected = Int32Array::from(vec![Some(15), None, Some(8)]);
        assert_eq!(result.as_ref(), &expected);

        let left =
            Arc::new(UInt32Array::from(vec![Some(12), None, Some(11)])) as ArrayRef;
        let right = ScalarValue::from(3u32);
        let mut result = bitwise_and_dyn_scalar(&left, right.clone()).unwrap()?;
        let expected = UInt32Array::from(vec![Some(0), None, Some(3)]);
        assert_eq!(result.as_ref(), &expected);

        result = bitwise_or_dyn_scalar(&left, right.clone()).unwrap()?;
        let expected = UInt32Array::from(vec![Some(15), None, Some(11)]);
        assert_eq!(result.as_ref(), &expected);

        result = bitwise_xor_dyn_scalar(&left, right).unwrap()?;
        let expected = UInt32Array::from(vec![Some(15), None, Some(8)]);
        assert_eq!(result.as_ref(), &expected);
        Ok(())
    }

    #[test]
    fn bitwise_shift_scalar_test() -> Result<()> {
        let input = Arc::new(Int32Array::from(vec![Some(2), None, Some(4)])) as ArrayRef;
        let module = ScalarValue::from(10i32);
        let mut result =
            bitwise_shift_left_dyn_scalar(&input, module.clone()).unwrap()?;

        let expected = Int32Array::from(vec![Some(2048), None, Some(4096)]);
        assert_eq!(result.as_ref(), &expected);

        result = bitwise_shift_right_dyn_scalar(&result, module).unwrap()?;
        assert_eq!(result.as_ref(), &input);

        let input = Arc::new(UInt32Array::from(vec![Some(2), None, Some(4)])) as ArrayRef;
        let module = ScalarValue::from(10u32);
        let mut result =
            bitwise_shift_left_dyn_scalar(&input, module.clone()).unwrap()?;

        let expected = UInt32Array::from(vec![Some(2048), None, Some(4096)]);
        assert_eq!(result.as_ref(), &expected);

        result = bitwise_shift_right_dyn_scalar(&result, module).unwrap()?;
        assert_eq!(result.as_ref(), &input);
        Ok(())
    }

    #[test]
    fn test_display_and_or_combo() {
        let expr = BinaryExpr::new(
            Arc::new(BinaryExpr::new(
                lit(ScalarValue::from(1)),
                Operator::And,
                lit(ScalarValue::from(2)),
            )),
            Operator::And,
            Arc::new(BinaryExpr::new(
                lit(ScalarValue::from(3)),
                Operator::And,
                lit(ScalarValue::from(4)),
            )),
        );
        assert_eq!(expr.to_string(), "1 AND 2 AND 3 AND 4");

        let expr = BinaryExpr::new(
            Arc::new(BinaryExpr::new(
                lit(ScalarValue::from(1)),
                Operator::Or,
                lit(ScalarValue::from(2)),
            )),
            Operator::Or,
            Arc::new(BinaryExpr::new(
                lit(ScalarValue::from(3)),
                Operator::Or,
                lit(ScalarValue::from(4)),
            )),
        );
        assert_eq!(expr.to_string(), "1 OR 2 OR 3 OR 4");

        let expr = BinaryExpr::new(
            Arc::new(BinaryExpr::new(
                lit(ScalarValue::from(1)),
                Operator::And,
                lit(ScalarValue::from(2)),
            )),
            Operator::Or,
            Arc::new(BinaryExpr::new(
                lit(ScalarValue::from(3)),
                Operator::And,
                lit(ScalarValue::from(4)),
            )),
        );
        assert_eq!(expr.to_string(), "1 AND 2 OR 3 AND 4");

        let expr = BinaryExpr::new(
            Arc::new(BinaryExpr::new(
                lit(ScalarValue::from(1)),
                Operator::Or,
                lit(ScalarValue::from(2)),
            )),
            Operator::And,
            Arc::new(BinaryExpr::new(
                lit(ScalarValue::from(3)),
                Operator::Or,
                lit(ScalarValue::from(4)),
            )),
        );
        assert_eq!(expr.to_string(), "(1 OR 2) AND (3 OR 4)");
    }

    #[test]
    fn test_to_result_type_array() {
        let values = Arc::new(Int32Array::from(vec![1, 2, 3, 4]));
        let keys = Int8Array::from(vec![Some(0), None, Some(2), Some(3)]);
        let dictionary =
            Arc::new(DictionaryArray::try_new(keys, values).unwrap()) as ArrayRef;

        // Casting Dictionary to Int32
        let casted = to_result_type_array(
            &Operator::Plus,
            Arc::clone(&dictionary),
            &DataType::Int32,
        )
        .unwrap();
        assert_eq!(
            &casted,
            &(Arc::new(Int32Array::from(vec![Some(1), None, Some(3), Some(4)]))
                as ArrayRef)
        );

        // Array has same datatype as result type, no casting
        let casted = to_result_type_array(
            &Operator::Plus,
            Arc::clone(&dictionary),
            dictionary.data_type(),
        )
        .unwrap();
        assert_eq!(&casted, &dictionary);

        // Not numerical operator, no casting
        let casted = to_result_type_array(
            &Operator::Eq,
            Arc::clone(&dictionary),
            &DataType::Int32,
        )
        .unwrap();
        assert_eq!(&casted, &dictionary);
    }

    #[test]
    fn test_add_with_overflow() -> Result<()> {
        // create test data
        let l = Arc::new(Int32Array::from(vec![1, i32::MAX]));
        let r = Arc::new(Int32Array::from(vec![2, 1]));
        let schema = Arc::new(Schema::new(vec![
            Field::new("l", DataType::Int32, false),
            Field::new("r", DataType::Int32, false),
        ]));
        let batch = RecordBatch::try_new(schema, vec![l, r])?;

        // create expression
        let expr = BinaryExpr::new(
            Arc::new(Column::new("l", 0)),
            Operator::Plus,
            Arc::new(Column::new("r", 1)),
        )
        .with_fail_on_overflow(true);

        // evaluate expression
        let result = expr.evaluate(&batch);
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("Overflow happened on: 2147483647 + 1"));
        Ok(())
    }

    #[test]
    fn test_subtract_with_overflow() -> Result<()> {
        // create test data
        let l = Arc::new(Int32Array::from(vec![1, i32::MIN]));
        let r = Arc::new(Int32Array::from(vec![2, 1]));
        let schema = Arc::new(Schema::new(vec![
            Field::new("l", DataType::Int32, false),
            Field::new("r", DataType::Int32, false),
        ]));
        let batch = RecordBatch::try_new(schema, vec![l, r])?;

        // create expression
        let expr = BinaryExpr::new(
            Arc::new(Column::new("l", 0)),
            Operator::Minus,
            Arc::new(Column::new("r", 1)),
        )
        .with_fail_on_overflow(true);

        // evaluate expression
        let result = expr.evaluate(&batch);
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("Overflow happened on: -2147483648 - 1"));
        Ok(())
    }

    #[test]
    fn test_mul_with_overflow() -> Result<()> {
        // create test data
        let l = Arc::new(Int32Array::from(vec![1, i32::MAX]));
        let r = Arc::new(Int32Array::from(vec![2, 2]));
        let schema = Arc::new(Schema::new(vec![
            Field::new("l", DataType::Int32, false),
            Field::new("r", DataType::Int32, false),
        ]));
        let batch = RecordBatch::try_new(schema, vec![l, r])?;

        // create expression
        let expr = BinaryExpr::new(
            Arc::new(Column::new("l", 0)),
            Operator::Multiply,
            Arc::new(Column::new("r", 1)),
        )
        .with_fail_on_overflow(true);

        // evaluate expression
        let result = expr.evaluate(&batch);
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("Overflow happened on: 2147483647 * 2"));
        Ok(())
    }

    /// Test helper for SIMILAR TO binary operation
    fn apply_similar_to(
        schema: &SchemaRef,
        va: Vec<&str>,
        vb: Vec<&str>,
        negated: bool,
        case_insensitive: bool,
        expected: &BooleanArray,
    ) -> Result<()> {
        let a = StringArray::from(va);
        let b = StringArray::from(vb);
        let op = similar_to(
            negated,
            case_insensitive,
            col("a", schema)?,
            col("b", schema)?,
        )?;
        let batch =
            RecordBatch::try_new(Arc::clone(schema), vec![Arc::new(a), Arc::new(b)])?;
        let result = op
            .evaluate(&batch)?
            .into_array(batch.num_rows())
            .expect("Failed to convert to array");
        assert_eq!(result.as_ref(), expected);

        Ok(())
    }

    #[test]
    fn test_similar_to() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Utf8, false),
            Field::new("b", DataType::Utf8, false),
        ]));

        let expected = [Some(true), Some(false)].iter().collect();
        // case-sensitive
        apply_similar_to(
            &schema,
            vec!["hello world", "Hello World"],
            vec!["hello.*", "hello.*"],
            false,
            false,
            &expected,
        )
        .unwrap();
        // case-insensitive
        apply_similar_to(
            &schema,
            vec!["hello world", "bye"],
            vec!["hello.*", "hello.*"],
            false,
            true,
            &expected,
        )
        .unwrap();
    }

    pub fn binary_expr(
        left: Arc<dyn PhysicalExpr>,
        op: Operator,
        right: Arc<dyn PhysicalExpr>,
        schema: &Schema,
    ) -> Result<BinaryExpr> {
        Ok(binary_op(left, op, right, schema)?
            .as_any()
            .downcast_ref::<BinaryExpr>()
            .unwrap()
            .clone())
    }

    /// Test for Uniform-Uniform, Unknown-Uniform, Uniform-Unknown and Unknown-Unknown evaluation.
    #[test]
    fn test_evaluate_statistics_combination_of_range_holders() -> Result<()> {
        let schema = &Schema::new(vec![Field::new("a", DataType::Float64, false)]);
        let a = Arc::new(Column::new("a", 0)) as _;
        let b = lit(ScalarValue::from(12.0));

        let left_interval = Interval::make(Some(0.0), Some(12.0))?;
        let right_interval = Interval::make(Some(12.0), Some(36.0))?;
        let (left_mean, right_mean) = (ScalarValue::from(6.0), ScalarValue::from(24.0));
        let (left_med, right_med) = (ScalarValue::from(6.0), ScalarValue::from(24.0));

        for children in [
            vec![
                &Distribution::new_uniform(left_interval.clone())?,
                &Distribution::new_uniform(right_interval.clone())?,
            ],
            vec![
                &Distribution::new_generic(
                    left_mean.clone(),
                    left_med.clone(),
                    ScalarValue::Float64(None),
                    left_interval.clone(),
                )?,
                &Distribution::new_uniform(right_interval.clone())?,
            ],
            vec![
                &Distribution::new_uniform(right_interval.clone())?,
                &Distribution::new_generic(
                    right_mean.clone(),
                    right_med.clone(),
                    ScalarValue::Float64(None),
                    right_interval.clone(),
                )?,
            ],
            vec![
                &Distribution::new_generic(
                    left_mean.clone(),
                    left_med.clone(),
                    ScalarValue::Float64(None),
                    left_interval.clone(),
                )?,
                &Distribution::new_generic(
                    right_mean.clone(),
                    right_med.clone(),
                    ScalarValue::Float64(None),
                    right_interval.clone(),
                )?,
            ],
        ] {
            let ops = vec![
                Operator::Plus,
                Operator::Minus,
                Operator::Multiply,
                Operator::Divide,
            ];

            for op in ops {
                let expr = binary_expr(Arc::clone(&a), op, Arc::clone(&b), schema)?;
                assert_eq!(
                    expr.evaluate_statistics(&children)?,
                    new_generic_from_binary_op(&op, children[0], children[1])?
                );
            }
        }
        Ok(())
    }

    #[test]
    fn test_evaluate_statistics_bernoulli() -> Result<()> {
        let schema = &Schema::new(vec![
            Field::new("a", DataType::Int64, false),
            Field::new("b", DataType::Int64, false),
        ]);
        let a = Arc::new(Column::new("a", 0)) as _;
        let b = Arc::new(Column::new("b", 1)) as _;
        let eq = Arc::new(binary_expr(
            Arc::clone(&a),
            Operator::Eq,
            Arc::clone(&b),
            schema,
        )?);
        let neq = Arc::new(binary_expr(a, Operator::NotEq, b, schema)?);

        let left_stat = &Distribution::new_uniform(Interval::make(Some(0), Some(7))?)?;
        let right_stat = &Distribution::new_uniform(Interval::make(Some(4), Some(11))?)?;

        // Intervals: [0, 7], [4, 11].
        // The intersection is [4, 7], so the probability of equality is 4 / 64 = 1 / 16.
        assert_eq!(
            eq.evaluate_statistics(&[left_stat, right_stat])?,
            Distribution::new_bernoulli(ScalarValue::from(1.0 / 16.0))?
        );

        // The probability of being distinct is 1 - 1 / 16 = 15 / 16.
        assert_eq!(
            neq.evaluate_statistics(&[left_stat, right_stat])?,
            Distribution::new_bernoulli(ScalarValue::from(15.0 / 16.0))?
        );

        Ok(())
    }

    #[test]
    fn test_propagate_statistics_combination_of_range_holders_arithmetic() -> Result<()> {
        let schema = &Schema::new(vec![Field::new("a", DataType::Float64, false)]);
        let a = Arc::new(Column::new("a", 0)) as _;
        let b = lit(ScalarValue::from(12.0));

        let left_interval = Interval::make(Some(0.0), Some(12.0))?;
        let right_interval = Interval::make(Some(12.0), Some(36.0))?;

        let parent = Distribution::new_uniform(Interval::make(Some(-432.), Some(432.))?)?;
        let children = vec![
            vec![
                Distribution::new_uniform(left_interval.clone())?,
                Distribution::new_uniform(right_interval.clone())?,
            ],
            vec![
                Distribution::new_generic(
                    ScalarValue::from(6.),
                    ScalarValue::from(6.),
                    ScalarValue::Float64(None),
                    left_interval.clone(),
                )?,
                Distribution::new_uniform(right_interval.clone())?,
            ],
            vec![
                Distribution::new_uniform(left_interval.clone())?,
                Distribution::new_generic(
                    ScalarValue::from(12.),
                    ScalarValue::from(12.),
                    ScalarValue::Float64(None),
                    right_interval.clone(),
                )?,
            ],
            vec![
                Distribution::new_generic(
                    ScalarValue::from(6.),
                    ScalarValue::from(6.),
                    ScalarValue::Float64(None),
                    left_interval.clone(),
                )?,
                Distribution::new_generic(
                    ScalarValue::from(12.),
                    ScalarValue::from(12.),
                    ScalarValue::Float64(None),
                    right_interval.clone(),
                )?,
            ],
        ];

        let ops = vec![
            Operator::Plus,
            Operator::Minus,
            Operator::Multiply,
            Operator::Divide,
        ];

        for child_view in children {
            let child_refs = child_view.iter().collect::<Vec<_>>();
            for op in &ops {
                let expr = binary_expr(Arc::clone(&a), *op, Arc::clone(&b), schema)?;
                assert_eq!(
                    expr.propagate_statistics(&parent, child_refs.as_slice())?,
                    Some(child_view.clone())
                );
            }
        }
        Ok(())
    }

    #[test]
    fn test_propagate_statistics_combination_of_range_holders_comparison() -> Result<()> {
        let schema = &Schema::new(vec![Field::new("a", DataType::Float64, false)]);
        let a = Arc::new(Column::new("a", 0)) as _;
        let b = lit(ScalarValue::from(12.0));

        let left_interval = Interval::make(Some(0.0), Some(12.0))?;
        let right_interval = Interval::make(Some(6.0), Some(18.0))?;

        let one = ScalarValue::from(1.0);
        let parent = Distribution::new_bernoulli(one)?;
        let children = vec![
            vec![
                Distribution::new_uniform(left_interval.clone())?,
                Distribution::new_uniform(right_interval.clone())?,
            ],
            vec![
                Distribution::new_generic(
                    ScalarValue::from(6.),
                    ScalarValue::from(6.),
                    ScalarValue::Float64(None),
                    left_interval.clone(),
                )?,
                Distribution::new_uniform(right_interval.clone())?,
            ],
            vec![
                Distribution::new_uniform(left_interval.clone())?,
                Distribution::new_generic(
                    ScalarValue::from(12.),
                    ScalarValue::from(12.),
                    ScalarValue::Float64(None),
                    right_interval.clone(),
                )?,
            ],
            vec![
                Distribution::new_generic(
                    ScalarValue::from(6.),
                    ScalarValue::from(6.),
                    ScalarValue::Float64(None),
                    left_interval.clone(),
                )?,
                Distribution::new_generic(
                    ScalarValue::from(12.),
                    ScalarValue::from(12.),
                    ScalarValue::Float64(None),
                    right_interval.clone(),
                )?,
            ],
        ];

        let ops = vec![
            Operator::Eq,
            Operator::Gt,
            Operator::GtEq,
            Operator::Lt,
            Operator::LtEq,
        ];

        for child_view in children {
            let child_refs = child_view.iter().collect::<Vec<_>>();
            for op in &ops {
                let expr = binary_expr(Arc::clone(&a), *op, Arc::clone(&b), schema)?;
                assert!(expr
                    .propagate_statistics(&parent, child_refs.as_slice())?
                    .is_some());
            }
        }

        Ok(())
    }

    #[test]
    fn test_fmt_sql() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
        ]);

        // Test basic binary expressions
        let simple_expr = binary_expr(
            col("a", &schema)?,
            Operator::Plus,
            col("b", &schema)?,
            &schema,
        )?;
        let display_string = simple_expr.to_string();
        assert_eq!(display_string, "a@0 + b@1");
        let sql_string = fmt_sql(&simple_expr).to_string();
        assert_eq!(sql_string, "a + b");

        // Test nested expressions with different operator precedence
        let nested_expr = binary_expr(
            Arc::new(binary_expr(
                col("a", &schema)?,
                Operator::Plus,
                col("b", &schema)?,
                &schema,
            )?),
            Operator::Multiply,
            col("b", &schema)?,
            &schema,
        )?;
        let display_string = nested_expr.to_string();
        assert_eq!(display_string, "(a@0 + b@1) * b@1");
        let sql_string = fmt_sql(&nested_expr).to_string();
        assert_eq!(sql_string, "(a + b) * b");

        // Test nested expressions with same operator precedence
        let nested_same_prec = binary_expr(
            Arc::new(binary_expr(
                col("a", &schema)?,
                Operator::Plus,
                col("b", &schema)?,
                &schema,
            )?),
            Operator::Plus,
            col("b", &schema)?,
            &schema,
        )?;
        let display_string = nested_same_prec.to_string();
        assert_eq!(display_string, "a@0 + b@1 + b@1");
        let sql_string = fmt_sql(&nested_same_prec).to_string();
        assert_eq!(sql_string, "a + b + b");

        // Test with literals
        let lit_expr = binary_expr(
            col("a", &schema)?,
            Operator::Eq,
            lit(ScalarValue::Int32(Some(42))),
            &schema,
        )?;
        let display_string = lit_expr.to_string();
        assert_eq!(display_string, "a@0 = 42");
        let sql_string = fmt_sql(&lit_expr).to_string();
        assert_eq!(sql_string, "a = 42");

        Ok(())
    }

    #[test]
    fn test_check_short_circuit() {
        // Test with non-nullable arrays
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
        ]));
        let a_array = Int32Array::from(vec![1, 3, 4, 5, 6]);
        let b_array = Int32Array::from(vec![1, 2, 3, 4, 5]);
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(a_array), Arc::new(b_array)],
        )
        .unwrap();

        // op: AND left: all false
        let left_expr = logical2physical(&logical_col("a").eq(expr_lit(2)), &schema);
        let left_value = left_expr.evaluate(&batch).unwrap();
        assert!(matches!(
            check_short_circuit(&left_value, &Operator::And),
            ShortCircuitStrategy::ReturnLeft
        ));

        // op: AND left: not all false
        let left_expr = logical2physical(&logical_col("a").eq(expr_lit(3)), &schema);
        let left_value = left_expr.evaluate(&batch).unwrap();
        let ColumnarValue::Array(array) = &left_value else {
            panic!("Expected ColumnarValue::Array");
        };
        let ShortCircuitStrategy::PreSelection(value) =
            check_short_circuit(&left_value, &Operator::And)
        else {
            panic!("Expected ShortCircuitStrategy::PreSelection");
        };
        let expected_boolean_arr: Vec<_> =
            as_boolean_array(array).unwrap().iter().collect();
        let boolean_arr: Vec<_> = value.iter().collect();
        assert_eq!(expected_boolean_arr, boolean_arr);

        // op: OR left: all true
        let left_expr = logical2physical(&logical_col("a").gt(expr_lit(0)), &schema);
        let left_value = left_expr.evaluate(&batch).unwrap();
        assert!(matches!(
            check_short_circuit(&left_value, &Operator::Or),
            ShortCircuitStrategy::ReturnLeft
        ));

        // op: OR left: not all true
        let left_expr: Arc<dyn PhysicalExpr> =
            logical2physical(&logical_col("a").gt(expr_lit(2)), &schema);
        let left_value = left_expr.evaluate(&batch).unwrap();
        assert!(matches!(
            check_short_circuit(&left_value, &Operator::Or),
            ShortCircuitStrategy::None
        ));

        // Test with nullable arrays and null values
        let schema_nullable = Arc::new(Schema::new(vec![
            Field::new("c", DataType::Boolean, true),
            Field::new("d", DataType::Boolean, true),
        ]));

        // Create arrays with null values
        let c_array = Arc::new(BooleanArray::from(vec![
            Some(true),
            Some(false),
            None,
            Some(true),
            None,
        ])) as ArrayRef;
        let d_array = Arc::new(BooleanArray::from(vec![
            Some(false),
            Some(true),
            Some(false),
            None,
            Some(true),
        ])) as ArrayRef;

        let batch_nullable = RecordBatch::try_new(
            Arc::clone(&schema_nullable),
            vec![Arc::clone(&c_array), Arc::clone(&d_array)],
        )
        .unwrap();

        // Case: Mixed values with nulls - shouldn't short-circuit for AND
        let mixed_nulls = logical2physical(&logical_col("c"), &schema_nullable);
        let mixed_nulls_value = mixed_nulls.evaluate(&batch_nullable).unwrap();
        assert!(matches!(
            check_short_circuit(&mixed_nulls_value, &Operator::And),
            ShortCircuitStrategy::None
        ));

        // Case: Mixed values with nulls - shouldn't short-circuit for OR
        assert!(matches!(
            check_short_circuit(&mixed_nulls_value, &Operator::Or),
            ShortCircuitStrategy::None
        ));

        // Test with all nulls
        let all_nulls = Arc::new(BooleanArray::from(vec![None, None, None])) as ArrayRef;
        let null_batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("e", DataType::Boolean, true)])),
            vec![all_nulls],
        )
        .unwrap();

        let null_expr = logical2physical(&logical_col("e"), &null_batch.schema());
        let null_value = null_expr.evaluate(&null_batch).unwrap();

        // All nulls shouldn't short-circuit for AND or OR
        assert!(matches!(
            check_short_circuit(&null_value, &Operator::And),
            ShortCircuitStrategy::None
        ));
        assert!(matches!(
            check_short_circuit(&null_value, &Operator::Or),
            ShortCircuitStrategy::None
        ));

        // Test with scalar values
        // Scalar true
        let scalar_true = ColumnarValue::Scalar(ScalarValue::Boolean(Some(true)));
        assert!(matches!(
            check_short_circuit(&scalar_true, &Operator::Or),
            ShortCircuitStrategy::ReturnLeft
        )); // Should short-circuit OR
        assert!(matches!(
            check_short_circuit(&scalar_true, &Operator::And),
            ShortCircuitStrategy::ReturnRight
        )); // Should return the RHS for AND

        // Scalar false
        let scalar_false = ColumnarValue::Scalar(ScalarValue::Boolean(Some(false)));
        assert!(matches!(
            check_short_circuit(&scalar_false, &Operator::And),
            ShortCircuitStrategy::ReturnLeft
        )); // Should short-circuit AND
        assert!(matches!(
            check_short_circuit(&scalar_false, &Operator::Or),
            ShortCircuitStrategy::ReturnRight
        )); // Should return the RHS for OR

        // Scalar null
        let scalar_null = ColumnarValue::Scalar(ScalarValue::Boolean(None));
        assert!(matches!(
            check_short_circuit(&scalar_null, &Operator::And),
            ShortCircuitStrategy::None
        ));
        assert!(matches!(
            check_short_circuit(&scalar_null, &Operator::Or),
            ShortCircuitStrategy::None
        ));
    }

    /// Test for [pre_selection_scatter]
    /// Since [check_short_circuit] ensures that the left side does not contain null and is neither all_true nor all_false, as well as not being empty,
    /// the following tests have been designed:
    /// 1. Test sparse left with interleaved true/false
    /// 2. Test multiple consecutive true blocks
    /// 3. Test multiple consecutive true blocks
    /// 4. Test single true at first position
    /// 5. Test single true at last position
    /// 6. Test nulls in right array
    /// 7. Test scalar right handling
    #[test]
    fn test_pre_selection_scatter() {
        fn create_bool_array(bools: Vec<bool>) -> BooleanArray {
            BooleanArray::from(bools.into_iter().map(Some).collect::<Vec<_>>())
        }
        // Test sparse left with interleaved true/false
        {
            // Left: [T, F, T, F, T]
            // Right: [F, T, F] (values for 3 true positions)
            let left = create_bool_array(vec![true, false, true, false, true]);
            let right = ColumnarValue::Array(Arc::new(create_bool_array(vec![
                false, true, false,
            ])));

            let result = pre_selection_scatter(&left, right).unwrap();
            let result_arr = result.into_array(left.len()).unwrap();

            let expected = create_bool_array(vec![false, false, true, false, false]);
            assert_eq!(&expected, result_arr.as_boolean());
        }
        // Test multiple consecutive true blocks
        {
            // Left: [F, T, T, F, T, T, T]
            // Right: [T, F, F, T, F]
            let left =
                create_bool_array(vec![false, true, true, false, true, true, true]);
            let right = ColumnarValue::Array(Arc::new(create_bool_array(vec![
                true, false, false, true, false,
            ])));

            let result = pre_selection_scatter(&left, right).unwrap();
            let result_arr = result.into_array(left.len()).unwrap();

            let expected =
                create_bool_array(vec![false, true, false, false, false, true, false]);
            assert_eq!(&expected, result_arr.as_boolean());
        }
        // Test single true at first position
        {
            // Left: [T, F, F]
            // Right: [F]
            let left = create_bool_array(vec![true, false, false]);
            let right = ColumnarValue::Array(Arc::new(create_bool_array(vec![false])));

            let result = pre_selection_scatter(&left, right).unwrap();
            let result_arr = result.into_array(left.len()).unwrap();

            let expected = create_bool_array(vec![false, false, false]);
            assert_eq!(&expected, result_arr.as_boolean());
        }
        // Test single true at last position
        {
            // Left: [F, F, T]
            // Right: [F]
            let left = create_bool_array(vec![false, false, true]);
            let right = ColumnarValue::Array(Arc::new(create_bool_array(vec![false])));

            let result = pre_selection_scatter(&left, right).unwrap();
            let result_arr = result.into_array(left.len()).unwrap();

            let expected = create_bool_array(vec![false, false, false]);
            assert_eq!(&expected, result_arr.as_boolean());
        }
        // Test nulls in right array
        {
            // Left: [F, T, F, T]
            // Right: [None, Some(false)] (with null at first position)
            let left = create_bool_array(vec![false, true, false, true]);
            let right_arr = BooleanArray::from(vec![None, Some(false)]);
            let right = ColumnarValue::Array(Arc::new(right_arr));

            let result = pre_selection_scatter(&left, right).unwrap();
            let result_arr = result.into_array(left.len()).unwrap();

            let expected = BooleanArray::from(vec![
                Some(false),
                None, // null from right
                Some(false),
                Some(false),
            ]);
            assert_eq!(&expected, result_arr.as_boolean());
        }
        // Test scalar right handling
        {
            // Left: [T, F, T]
            // Right: Scalar true
            let left = create_bool_array(vec![true, false, true]);
            let right = ColumnarValue::Scalar(ScalarValue::Boolean(Some(true)));

            let result = pre_selection_scatter(&left, right).unwrap();
            assert!(matches!(result, ColumnarValue::Scalar(_)));
        }
    }

    #[test]
    fn test_evaluate_bounds_int32() {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
        ]);

        let a = Arc::new(Column::new("a", 0)) as _;
        let b = Arc::new(Column::new("b", 1)) as _;

        // Test addition bounds
        let add_expr =
            binary_expr(Arc::clone(&a), Operator::Plus, Arc::clone(&b), &schema).unwrap();
        let add_bounds = add_expr
            .evaluate_bounds(&[
                &Interval::make(Some(1), Some(10)).unwrap(),
                &Interval::make(Some(5), Some(15)).unwrap(),
            ])
            .unwrap();
        assert_eq!(add_bounds, Interval::make(Some(6), Some(25)).unwrap());

        // Test subtraction bounds
        let sub_expr =
            binary_expr(Arc::clone(&a), Operator::Minus, Arc::clone(&b), &schema)
                .unwrap();
        let sub_bounds = sub_expr
            .evaluate_bounds(&[
                &Interval::make(Some(1), Some(10)).unwrap(),
                &Interval::make(Some(5), Some(15)).unwrap(),
            ])
            .unwrap();
        assert_eq!(sub_bounds, Interval::make(Some(-14), Some(5)).unwrap());

        // Test multiplication bounds
        let mul_expr =
            binary_expr(Arc::clone(&a), Operator::Multiply, Arc::clone(&b), &schema)
                .unwrap();
        let mul_bounds = mul_expr
            .evaluate_bounds(&[
                &Interval::make(Some(1), Some(10)).unwrap(),
                &Interval::make(Some(5), Some(15)).unwrap(),
            ])
            .unwrap();
        assert_eq!(mul_bounds, Interval::make(Some(5), Some(150)).unwrap());

        // Test division bounds
        let div_expr =
            binary_expr(Arc::clone(&a), Operator::Divide, Arc::clone(&b), &schema)
                .unwrap();
        let div_bounds = div_expr
            .evaluate_bounds(&[
                &Interval::make(Some(10), Some(20)).unwrap(),
                &Interval::make(Some(2), Some(5)).unwrap(),
            ])
            .unwrap();
        assert_eq!(div_bounds, Interval::make(Some(2), Some(10)).unwrap());
    }

    #[test]
    fn test_evaluate_bounds_bool() {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Boolean, false),
            Field::new("b", DataType::Boolean, false),
        ]);

        let a = Arc::new(Column::new("a", 0)) as _;
        let b = Arc::new(Column::new("b", 1)) as _;

        // Test OR bounds
        let or_expr =
            binary_expr(Arc::clone(&a), Operator::Or, Arc::clone(&b), &schema).unwrap();
        let or_bounds = or_expr
            .evaluate_bounds(&[
                &Interval::make(Some(true), Some(true)).unwrap(),
                &Interval::make(Some(false), Some(false)).unwrap(),
            ])
            .unwrap();
        assert_eq!(or_bounds, Interval::make(Some(true), Some(true)).unwrap());

        // Test AND bounds
        let and_expr =
            binary_expr(Arc::clone(&a), Operator::And, Arc::clone(&b), &schema).unwrap();
        let and_bounds = and_expr
            .evaluate_bounds(&[
                &Interval::make(Some(true), Some(true)).unwrap(),
                &Interval::make(Some(false), Some(false)).unwrap(),
            ])
            .unwrap();
        assert_eq!(
            and_bounds,
            Interval::make(Some(false), Some(false)).unwrap()
        );
    }

    #[test]
    fn test_evaluate_nested_type() {
        let batch_schema = Arc::new(Schema::new(vec![
            Field::new(
                "a",
                DataType::List(Arc::new(Field::new_list_field(DataType::Int32, true))),
                true,
            ),
            Field::new(
                "b",
                DataType::List(Arc::new(Field::new_list_field(DataType::Int32, true))),
                true,
            ),
        ]));

        let mut list_builder_a = ListBuilder::new(Int32Builder::new());

        list_builder_a.append_value([Some(1)]);
        list_builder_a.append_value([Some(2)]);
        list_builder_a.append_value([]);
        list_builder_a.append_value([None]);

        let list_array_a: ArrayRef = Arc::new(list_builder_a.finish());

        let mut list_builder_b = ListBuilder::new(Int32Builder::new());

        list_builder_b.append_value([Some(1)]);
        list_builder_b.append_value([Some(2)]);
        list_builder_b.append_value([]);
        list_builder_b.append_value([None]);

        let list_array_b: ArrayRef = Arc::new(list_builder_b.finish());

        let batch =
            RecordBatch::try_new(batch_schema, vec![list_array_a, list_array_b]).unwrap();

        let schema = Arc::new(Schema::new(vec![
            Field::new(
                "a",
                DataType::List(Arc::new(Field::new("foo", DataType::Int32, true))),
                true,
            ),
            Field::new(
                "b",
                DataType::List(Arc::new(Field::new("bar", DataType::Int32, true))),
                true,
            ),
        ]));

        let a = Arc::new(Column::new("a", 0)) as _;
        let b = Arc::new(Column::new("b", 1)) as _;

        let eq_expr =
            binary_expr(Arc::clone(&a), Operator::Eq, Arc::clone(&b), &schema).unwrap();

        let eq_result = eq_expr.evaluate(&batch).unwrap();
        let expected =
            BooleanArray::from_iter(vec![Some(true), Some(true), Some(true), Some(true)]);
        assert_eq!(eq_result.into_array(4).unwrap().as_boolean(), &expected);
    }
}
