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

//! Defines `Avg` & `Mean` aggregate & accumulators

use arrow::array::{
    Array, ArrayRef, ArrowNativeTypeOp, ArrowNumericType, ArrowPrimitiveType, AsArray,
    BooleanArray, PrimitiveArray, PrimitiveBuilder, UInt64Array,
};

use arrow::compute::sum;
use arrow::datatypes::{
    i256, ArrowNativeType, DataType, Decimal128Type, Decimal256Type, DecimalType,
    DurationMicrosecondType, DurationMillisecondType, DurationNanosecondType,
    DurationSecondType, Field, FieldRef, Float64Type, TimeUnit, UInt64Type,
};
use datafusion_common::{
    exec_err, not_impl_err, utils::take_function_args, Result, ScalarValue,
};
use datafusion_expr::function::{AccumulatorArgs, StateFieldsArgs};
use datafusion_expr::type_coercion::aggregates::{avg_return_type, coerce_avg_type};
use datafusion_expr::utils::format_state_name;
use datafusion_expr::Volatility::Immutable;
use datafusion_expr::{
    Accumulator, AggregateUDFImpl, Documentation, EmitTo, GroupsAccumulator,
    ReversedUDAF, Signature,
};

use datafusion_functions_aggregate_common::aggregate::groups_accumulator::accumulate::NullState;
use datafusion_functions_aggregate_common::aggregate::groups_accumulator::nulls::{
    filtered_null_mask, set_nulls,
};

use datafusion_functions_aggregate_common::utils::DecimalAverager;
use datafusion_macros::user_doc;
use log::debug;
use std::any::Any;
use std::fmt::Debug;
use std::mem::{size_of, size_of_val};
use std::sync::Arc;

make_udaf_expr_and_func!(
    Avg,
    avg,
    expression,
    "Returns the avg of a group of values.",
    avg_udaf
);

#[user_doc(
    doc_section(label = "General Functions"),
    description = "Returns the average of numeric values in the specified column.",
    syntax_example = "avg(expression)",
    sql_example = r#"```sql
> SELECT avg(column_name) FROM table_name;
+---------------------------+
| avg(column_name)           |
+---------------------------+
| 42.75                      |
+---------------------------+
```"#,
    standard_argument(name = "expression",)
)]
#[derive(Debug)]
pub struct Avg {
    signature: Signature,
    aliases: Vec<String>,
}

impl Avg {
    pub fn new() -> Self {
        Self {
            signature: Signature::user_defined(Immutable),
            aliases: vec![String::from("mean")],
        }
    }
}

impl Default for Avg {
    fn default() -> Self {
        Self::new()
    }
}

impl AggregateUDFImpl for Avg {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "avg"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, arg_types: &[DataType]) -> Result<DataType> {
        avg_return_type(self.name(), &arg_types[0])
    }

    fn accumulator(&self, acc_args: AccumulatorArgs) -> Result<Box<dyn Accumulator>> {
        if acc_args.is_distinct {
            return exec_err!("avg(DISTINCT) aggregations are not available");
        }
        use DataType::*;

        let data_type = acc_args.exprs[0].data_type(acc_args.schema)?;
        // instantiate specialized accumulator based for the type
        match (&data_type, acc_args.return_field.data_type()) {
            (Float64, Float64) => Ok(Box::<AvgAccumulator>::default()),
            (
                Decimal128(sum_precision, sum_scale),
                Decimal128(target_precision, target_scale),
            ) => Ok(Box::new(DecimalAvgAccumulator::<Decimal128Type> {
                sum: None,
                count: 0,
                sum_scale: *sum_scale,
                sum_precision: *sum_precision,
                target_precision: *target_precision,
                target_scale: *target_scale,
            })),

            (
                Decimal256(sum_precision, sum_scale),
                Decimal256(target_precision, target_scale),
            ) => Ok(Box::new(DecimalAvgAccumulator::<Decimal256Type> {
                sum: None,
                count: 0,
                sum_scale: *sum_scale,
                sum_precision: *sum_precision,
                target_precision: *target_precision,
                target_scale: *target_scale,
            })),

            (Duration(time_unit), Duration(result_unit)) => {
                Ok(Box::new(DurationAvgAccumulator {
                    sum: None,
                    count: 0,
                    time_unit: *time_unit,
                    result_unit: *result_unit,
                }))
            }

            _ => exec_err!(
                "AvgAccumulator for ({} --> {})",
                &data_type,
                acc_args.return_field.data_type()
            ),
        }
    }

    fn state_fields(&self, args: StateFieldsArgs) -> Result<Vec<FieldRef>> {
        Ok(vec![
            Field::new(
                format_state_name(args.name, "count"),
                DataType::UInt64,
                true,
            ),
            Field::new(
                format_state_name(args.name, "sum"),
                args.input_fields[0].data_type().clone(),
                true,
            ),
        ]
        .into_iter()
        .map(Arc::new)
        .collect())
    }

    fn groups_accumulator_supported(&self, args: AccumulatorArgs) -> bool {
        matches!(
            args.return_field.data_type(),
            DataType::Float64 | DataType::Decimal128(_, _) | DataType::Duration(_)
        )
    }

    fn create_groups_accumulator(
        &self,
        args: AccumulatorArgs,
    ) -> Result<Box<dyn GroupsAccumulator>> {
        use DataType::*;

        let data_type = args.exprs[0].data_type(args.schema)?;
        // instantiate specialized accumulator based for the type
        match (&data_type, args.return_field.data_type()) {
            (Float64, Float64) => {
                Ok(Box::new(AvgGroupsAccumulator::<Float64Type, _>::new(
                    &data_type,
                    args.return_field.data_type(),
                    |sum: f64, count: u64| Ok(sum / count as f64),
                )))
            }
            (
                Decimal128(_sum_precision, sum_scale),
                Decimal128(target_precision, target_scale),
            ) => {
                let decimal_averager = DecimalAverager::<Decimal128Type>::try_new(
                    *sum_scale,
                    *target_precision,
                    *target_scale,
                )?;

                let avg_fn =
                    move |sum: i128, count: u64| decimal_averager.avg(sum, count as i128);

                Ok(Box::new(AvgGroupsAccumulator::<Decimal128Type, _>::new(
                    &data_type,
                    args.return_field.data_type(),
                    avg_fn,
                )))
            }

            (
                Decimal256(_sum_precision, sum_scale),
                Decimal256(target_precision, target_scale),
            ) => {
                let decimal_averager = DecimalAverager::<Decimal256Type>::try_new(
                    *sum_scale,
                    *target_precision,
                    *target_scale,
                )?;

                let avg_fn = move |sum: i256, count: u64| {
                    decimal_averager.avg(sum, i256::from_usize(count as usize).unwrap())
                };

                Ok(Box::new(AvgGroupsAccumulator::<Decimal256Type, _>::new(
                    &data_type,
                    args.return_field.data_type(),
                    avg_fn,
                )))
            }

            (Duration(time_unit), Duration(_result_unit)) => {
                let avg_fn = move |sum: i64, count: u64| Ok(sum / count as i64);

                match time_unit {
                    TimeUnit::Second => Ok(Box::new(AvgGroupsAccumulator::<
                        DurationSecondType,
                        _,
                    >::new(
                        &data_type,
                        args.return_type(),
                        avg_fn,
                    ))),
                    TimeUnit::Millisecond => Ok(Box::new(AvgGroupsAccumulator::<
                        DurationMillisecondType,
                        _,
                    >::new(
                        &data_type,
                        args.return_type(),
                        avg_fn,
                    ))),
                    TimeUnit::Microsecond => Ok(Box::new(AvgGroupsAccumulator::<
                        DurationMicrosecondType,
                        _,
                    >::new(
                        &data_type,
                        args.return_type(),
                        avg_fn,
                    ))),
                    TimeUnit::Nanosecond => Ok(Box::new(AvgGroupsAccumulator::<
                        DurationNanosecondType,
                        _,
                    >::new(
                        &data_type,
                        args.return_type(),
                        avg_fn,
                    ))),
                }
            }

            _ => not_impl_err!(
                "AvgGroupsAccumulator for ({} --> {})",
                &data_type,
                args.return_field.data_type()
            ),
        }
    }

    fn aliases(&self) -> &[String] {
        &self.aliases
    }

    fn reverse_expr(&self) -> ReversedUDAF {
        ReversedUDAF::Identical
    }

    fn coerce_types(&self, arg_types: &[DataType]) -> Result<Vec<DataType>> {
        let [args] = take_function_args(self.name(), arg_types)?;
        coerce_avg_type(self.name(), std::slice::from_ref(args))
    }

    fn documentation(&self) -> Option<&Documentation> {
        self.doc()
    }
}

/// An accumulator to compute the average
#[derive(Debug, Default)]
pub struct AvgAccumulator {
    sum: Option<f64>,
    count: u64,
}

impl Accumulator for AvgAccumulator {
    fn update_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        let values = values[0].as_primitive::<Float64Type>();
        self.count += (values.len() - values.null_count()) as u64;
        if let Some(x) = sum(values) {
            let v = self.sum.get_or_insert(0.);
            *v += x;
        }
        Ok(())
    }

    fn evaluate(&mut self) -> Result<ScalarValue> {
        Ok(ScalarValue::Float64(
            self.sum.map(|f| f / self.count as f64),
        ))
    }

    fn size(&self) -> usize {
        size_of_val(self)
    }

    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        Ok(vec![
            ScalarValue::from(self.count),
            ScalarValue::Float64(self.sum),
        ])
    }

    fn merge_batch(&mut self, states: &[ArrayRef]) -> Result<()> {
        // counts are summed
        self.count += sum(states[0].as_primitive::<UInt64Type>()).unwrap_or_default();

        // sums are summed
        if let Some(x) = sum(states[1].as_primitive::<Float64Type>()) {
            let v = self.sum.get_or_insert(0.);
            *v += x;
        }
        Ok(())
    }
    fn retract_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        let values = values[0].as_primitive::<Float64Type>();
        self.count -= (values.len() - values.null_count()) as u64;
        if let Some(x) = sum(values) {
            self.sum = Some(self.sum.unwrap() - x);
        }
        Ok(())
    }

    fn supports_retract_batch(&self) -> bool {
        true
    }
}

/// An accumulator to compute the average for decimals
#[derive(Debug)]
struct DecimalAvgAccumulator<T: DecimalType + ArrowNumericType + Debug> {
    sum: Option<T::Native>,
    count: u64,
    sum_scale: i8,
    sum_precision: u8,
    target_precision: u8,
    target_scale: i8,
}

impl<T: DecimalType + ArrowNumericType + Debug> Accumulator for DecimalAvgAccumulator<T> {
    fn update_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        let values = values[0].as_primitive::<T>();
        self.count += (values.len() - values.null_count()) as u64;

        if let Some(x) = sum(values) {
            let v = self.sum.get_or_insert(T::Native::default());
            self.sum = Some(v.add_wrapping(x));
        }
        Ok(())
    }

    fn evaluate(&mut self) -> Result<ScalarValue> {
        let v = self
            .sum
            .map(|v| {
                DecimalAverager::<T>::try_new(
                    self.sum_scale,
                    self.target_precision,
                    self.target_scale,
                )?
                .avg(v, T::Native::from_usize(self.count as usize).unwrap())
            })
            .transpose()?;

        ScalarValue::new_primitive::<T>(
            v,
            &T::TYPE_CONSTRUCTOR(self.target_precision, self.target_scale),
        )
    }

    fn size(&self) -> usize {
        size_of_val(self)
    }

    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        Ok(vec![
            ScalarValue::from(self.count),
            ScalarValue::new_primitive::<T>(
                self.sum,
                &T::TYPE_CONSTRUCTOR(self.sum_precision, self.sum_scale),
            )?,
        ])
    }

    fn merge_batch(&mut self, states: &[ArrayRef]) -> Result<()> {
        // counts are summed
        self.count += sum(states[0].as_primitive::<UInt64Type>()).unwrap_or_default();

        // sums are summed
        if let Some(x) = sum(states[1].as_primitive::<T>()) {
            let v = self.sum.get_or_insert(T::Native::default());
            self.sum = Some(v.add_wrapping(x));
        }
        Ok(())
    }
    fn retract_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        let values = values[0].as_primitive::<T>();
        self.count -= (values.len() - values.null_count()) as u64;
        if let Some(x) = sum(values) {
            self.sum = Some(self.sum.unwrap().sub_wrapping(x));
        }
        Ok(())
    }

    fn supports_retract_batch(&self) -> bool {
        true
    }
}

/// An accumulator to compute the average for duration values
#[derive(Debug)]
struct DurationAvgAccumulator {
    sum: Option<i64>,
    count: u64,
    time_unit: TimeUnit,
    result_unit: TimeUnit,
}

impl Accumulator for DurationAvgAccumulator {
    fn update_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        let array = &values[0];
        self.count += (array.len() - array.null_count()) as u64;

        let sum_value = match self.time_unit {
            TimeUnit::Second => sum(array.as_primitive::<DurationSecondType>()),
            TimeUnit::Millisecond => sum(array.as_primitive::<DurationMillisecondType>()),
            TimeUnit::Microsecond => sum(array.as_primitive::<DurationMicrosecondType>()),
            TimeUnit::Nanosecond => sum(array.as_primitive::<DurationNanosecondType>()),
        };

        if let Some(x) = sum_value {
            let v = self.sum.get_or_insert(0);
            *v += x;
        }
        Ok(())
    }

    fn evaluate(&mut self) -> Result<ScalarValue> {
        let avg = self.sum.map(|sum| sum / self.count as i64);

        match self.result_unit {
            TimeUnit::Second => Ok(ScalarValue::DurationSecond(avg)),
            TimeUnit::Millisecond => Ok(ScalarValue::DurationMillisecond(avg)),
            TimeUnit::Microsecond => Ok(ScalarValue::DurationMicrosecond(avg)),
            TimeUnit::Nanosecond => Ok(ScalarValue::DurationNanosecond(avg)),
        }
    }

    fn size(&self) -> usize {
        size_of_val(self)
    }

    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        let duration_value = match self.time_unit {
            TimeUnit::Second => ScalarValue::DurationSecond(self.sum),
            TimeUnit::Millisecond => ScalarValue::DurationMillisecond(self.sum),
            TimeUnit::Microsecond => ScalarValue::DurationMicrosecond(self.sum),
            TimeUnit::Nanosecond => ScalarValue::DurationNanosecond(self.sum),
        };

        Ok(vec![ScalarValue::from(self.count), duration_value])
    }

    fn merge_batch(&mut self, states: &[ArrayRef]) -> Result<()> {
        self.count += sum(states[0].as_primitive::<UInt64Type>()).unwrap_or_default();

        let sum_value = match self.time_unit {
            TimeUnit::Second => sum(states[1].as_primitive::<DurationSecondType>()),
            TimeUnit::Millisecond => {
                sum(states[1].as_primitive::<DurationMillisecondType>())
            }
            TimeUnit::Microsecond => {
                sum(states[1].as_primitive::<DurationMicrosecondType>())
            }
            TimeUnit::Nanosecond => {
                sum(states[1].as_primitive::<DurationNanosecondType>())
            }
        };

        if let Some(x) = sum_value {
            let v = self.sum.get_or_insert(0);
            *v += x;
        }
        Ok(())
    }

    fn retract_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        let array = &values[0];
        self.count -= (array.len() - array.null_count()) as u64;

        let sum_value = match self.time_unit {
            TimeUnit::Second => sum(array.as_primitive::<DurationSecondType>()),
            TimeUnit::Millisecond => sum(array.as_primitive::<DurationMillisecondType>()),
            TimeUnit::Microsecond => sum(array.as_primitive::<DurationMicrosecondType>()),
            TimeUnit::Nanosecond => sum(array.as_primitive::<DurationNanosecondType>()),
        };

        if let Some(x) = sum_value {
            self.sum = Some(self.sum.unwrap() - x);
        }
        Ok(())
    }

    fn supports_retract_batch(&self) -> bool {
        true
    }
}

/// An accumulator to compute the average of `[PrimitiveArray<T>]`.
/// Stores values as native types, and does overflow checking
///
/// F: Function that calculates the average value from a sum of
/// T::Native and a total count
#[derive(Debug)]
struct AvgGroupsAccumulator<T, F>
where
    T: ArrowNumericType + Send,
    F: Fn(T::Native, u64) -> Result<T::Native> + Send,
{
    /// The type of the internal sum
    sum_data_type: DataType,

    /// The type of the returned sum
    return_data_type: DataType,

    /// Count per group (use u64 to make UInt64Array)
    counts: Vec<u64>,

    /// Sums per group, stored as the native type
    sums: Vec<T::Native>,

    /// Track nulls in the input / filters
    null_state: NullState,

    /// Function that computes the final average (value / count)
    avg_fn: F,
}

impl<T, F> AvgGroupsAccumulator<T, F>
where
    T: ArrowNumericType + Send,
    F: Fn(T::Native, u64) -> Result<T::Native> + Send,
{
    pub fn new(sum_data_type: &DataType, return_data_type: &DataType, avg_fn: F) -> Self {
        debug!(
            "AvgGroupsAccumulator ({}, sum type: {sum_data_type:?}) --> {return_data_type:?}",
            std::any::type_name::<T>()
        );

        Self {
            return_data_type: return_data_type.clone(),
            sum_data_type: sum_data_type.clone(),
            counts: vec![],
            sums: vec![],
            null_state: NullState::new(),
            avg_fn,
        }
    }
}

impl<T, F> GroupsAccumulator for AvgGroupsAccumulator<T, F>
where
    T: ArrowNumericType + Send,
    F: Fn(T::Native, u64) -> Result<T::Native> + Send,
{
    fn update_batch(
        &mut self,
        values: &[ArrayRef],
        group_indices: &[usize],
        opt_filter: Option<&BooleanArray>,
        total_num_groups: usize,
    ) -> Result<()> {
        assert_eq!(values.len(), 1, "single argument to update_batch");
        let values = values[0].as_primitive::<T>();

        // increment counts, update sums
        self.counts.resize(total_num_groups, 0);
        self.sums.resize(total_num_groups, T::default_value());
        self.null_state.accumulate(
            group_indices,
            values,
            opt_filter,
            total_num_groups,
            |group_index, new_value| {
                let sum = &mut self.sums[group_index];
                *sum = sum.add_wrapping(new_value);

                self.counts[group_index] += 1;
            },
        );

        Ok(())
    }

    fn evaluate(&mut self, emit_to: EmitTo) -> Result<ArrayRef> {
        let counts = emit_to.take_needed(&mut self.counts);
        let sums = emit_to.take_needed(&mut self.sums);
        let nulls = self.null_state.build(emit_to);

        assert_eq!(nulls.len(), sums.len());
        assert_eq!(counts.len(), sums.len());

        // don't evaluate averages with null inputs to avoid errors on null values

        let array: PrimitiveArray<T> = if nulls.null_count() > 0 {
            let mut builder = PrimitiveBuilder::<T>::with_capacity(nulls.len())
                .with_data_type(self.return_data_type.clone());
            let iter = sums.into_iter().zip(counts).zip(nulls.iter());

            for ((sum, count), is_valid) in iter {
                if is_valid {
                    builder.append_value((self.avg_fn)(sum, count)?)
                } else {
                    builder.append_null();
                }
            }
            builder.finish()
        } else {
            let averages: Vec<T::Native> = sums
                .into_iter()
                .zip(counts.into_iter())
                .map(|(sum, count)| (self.avg_fn)(sum, count))
                .collect::<Result<Vec<_>>>()?;
            PrimitiveArray::new(averages.into(), Some(nulls)) // no copy
                .with_data_type(self.return_data_type.clone())
        };

        Ok(Arc::new(array))
    }

    // return arrays for sums and counts
    fn state(&mut self, emit_to: EmitTo) -> Result<Vec<ArrayRef>> {
        let nulls = self.null_state.build(emit_to);
        let nulls = Some(nulls);

        let counts = emit_to.take_needed(&mut self.counts);
        let counts = UInt64Array::new(counts.into(), nulls.clone()); // zero copy

        let sums = emit_to.take_needed(&mut self.sums);
        let sums = PrimitiveArray::<T>::new(sums.into(), nulls) // zero copy
            .with_data_type(self.sum_data_type.clone());

        Ok(vec![
            Arc::new(counts) as ArrayRef,
            Arc::new(sums) as ArrayRef,
        ])
    }

    fn merge_batch(
        &mut self,
        values: &[ArrayRef],
        group_indices: &[usize],
        opt_filter: Option<&BooleanArray>,
        total_num_groups: usize,
    ) -> Result<()> {
        assert_eq!(values.len(), 2, "two arguments to merge_batch");
        // first batch is counts, second is partial sums
        let partial_counts = values[0].as_primitive::<UInt64Type>();
        let partial_sums = values[1].as_primitive::<T>();
        // update counts with partial counts
        self.counts.resize(total_num_groups, 0);
        self.null_state.accumulate(
            group_indices,
            partial_counts,
            opt_filter,
            total_num_groups,
            |group_index, partial_count| {
                self.counts[group_index] += partial_count;
            },
        );

        // update sums
        self.sums.resize(total_num_groups, T::default_value());
        self.null_state.accumulate(
            group_indices,
            partial_sums,
            opt_filter,
            total_num_groups,
            |group_index, new_value: <T as ArrowPrimitiveType>::Native| {
                let sum = &mut self.sums[group_index];
                *sum = sum.add_wrapping(new_value);
            },
        );

        Ok(())
    }

    fn convert_to_state(
        &self,
        values: &[ArrayRef],
        opt_filter: Option<&BooleanArray>,
    ) -> Result<Vec<ArrayRef>> {
        let sums = values[0]
            .as_primitive::<T>()
            .clone()
            .with_data_type(self.sum_data_type.clone());
        let counts = UInt64Array::from_value(1, sums.len());

        let nulls = filtered_null_mask(opt_filter, &sums);

        // set nulls on the arrays
        let counts = set_nulls(counts, nulls.clone());
        let sums = set_nulls(sums, nulls);

        Ok(vec![Arc::new(counts) as ArrayRef, Arc::new(sums)])
    }

    fn supports_convert_to_state(&self) -> bool {
        true
    }

    fn size(&self) -> usize {
        self.counts.capacity() * size_of::<u64>() + self.sums.capacity() * size_of::<T>()
    }
}
