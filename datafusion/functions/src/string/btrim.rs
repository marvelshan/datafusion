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

use crate::string::common::*;
use crate::utils::{make_scalar_function, utf8_to_str_type};
use arrow::array::{ArrayRef, OffsetSizeTrait};
use arrow::datatypes::DataType;
use datafusion_common::types::logical_string;
use datafusion_common::{exec_err, Result};
use datafusion_expr::function::Hint;
use datafusion_expr::{
    Coercion, ColumnarValue, Documentation, ScalarFunctionArgs, ScalarUDFImpl, Signature,
    TypeSignature, TypeSignatureClass, Volatility,
};
use datafusion_macros::user_doc;
use std::any::Any;
use std::sync::Arc;

/// Returns the longest string with leading and trailing characters removed. If the characters are not specified, whitespace is removed.
/// btrim('xyxtrimyyx', 'xyz') = 'trim'
fn btrim<T: OffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    let use_string_view = args[0].data_type() == &DataType::Utf8View;
    let args = if args.len() > 1 {
        let arg1 = arrow::compute::kernels::cast::cast(&args[1], args[0].data_type())?;
        vec![Arc::clone(&args[0]), arg1]
    } else {
        args.to_owned()
    };
    general_trim::<T>(&args, TrimType::Both, use_string_view)
}

#[user_doc(
    doc_section(label = "String Functions"),
    description = "Trims the specified trim string from the start and end of a string. If no trim string is provided, all whitespace is removed from the start and end of the input string.",
    syntax_example = "btrim(str[, trim_str])",
    sql_example = r#"```sql
> select btrim('__datafusion____', '_');
+-------------------------------------------+
| btrim(Utf8("__datafusion____"),Utf8("_")) |
+-------------------------------------------+
| datafusion                                |
+-------------------------------------------+
```"#,
    standard_argument(name = "str", prefix = "String"),
    argument(
        name = "trim_str",
        description = r"String expression to operate on. Can be a constant, column, or function, and any combination of operators. _Default is whitespace characters._"
    ),
    alternative_syntax = "trim(BOTH trim_str FROM str)",
    alternative_syntax = "trim(trim_str FROM str)",
    related_udf(name = "ltrim"),
    related_udf(name = "rtrim")
)]
#[derive(Debug)]
pub struct BTrimFunc {
    signature: Signature,
    aliases: Vec<String>,
}

impl Default for BTrimFunc {
    fn default() -> Self {
        Self::new()
    }
}

impl BTrimFunc {
    pub fn new() -> Self {
        Self {
            signature: Signature::one_of(
                vec![
                    TypeSignature::Coercible(vec![
                        Coercion::new_exact(TypeSignatureClass::Native(logical_string())),
                        Coercion::new_exact(TypeSignatureClass::Native(logical_string())),
                    ]),
                    TypeSignature::Coercible(vec![Coercion::new_exact(
                        TypeSignatureClass::Native(logical_string()),
                    )]),
                ],
                Volatility::Immutable,
            ),
            aliases: vec![String::from("trim")],
        }
    }
}

impl ScalarUDFImpl for BTrimFunc {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "btrim"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, arg_types: &[DataType]) -> Result<DataType> {
        if arg_types[0] == DataType::Utf8View {
            Ok(DataType::Utf8View)
        } else {
            utf8_to_str_type(&arg_types[0], "btrim")
        }
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        match args.args[0].data_type() {
            DataType::Utf8 | DataType::Utf8View => make_scalar_function(
                btrim::<i32>,
                vec![Hint::Pad, Hint::AcceptsSingular],
            )(&args.args),
            DataType::LargeUtf8 => make_scalar_function(
                btrim::<i64>,
                vec![Hint::Pad, Hint::AcceptsSingular],
            )(&args.args),
            other => exec_err!(
                "Unsupported data type {other:?} for function btrim,\
                expected Utf8, LargeUtf8 or Utf8View."
            ),
        }
    }

    fn aliases(&self) -> &[String] {
        &self.aliases
    }

    fn documentation(&self) -> Option<&Documentation> {
        self.doc()
    }
}

#[cfg(test)]
mod tests {
    use arrow::array::{Array, StringArray, StringViewArray};
    use arrow::datatypes::DataType::{Utf8, Utf8View};

    use datafusion_common::{Result, ScalarValue};
    use datafusion_expr::{ColumnarValue, ScalarUDFImpl};

    use crate::string::btrim::BTrimFunc;
    use crate::utils::test::test_function;

    #[test]
    fn test_functions() {
        // String view cases for checking normal logic
        test_function!(
            BTrimFunc::new(),
            vec![ColumnarValue::Scalar(ScalarValue::Utf8View(Some(
                String::from("alphabet  ")
            )))],
            Ok(Some("alphabet")),
            &str,
            Utf8View,
            StringViewArray
        );
        test_function!(
            BTrimFunc::new(),
            vec![ColumnarValue::Scalar(ScalarValue::Utf8View(Some(
                String::from("  alphabet  ")
            ))),],
            Ok(Some("alphabet")),
            &str,
            Utf8View,
            StringViewArray
        );
        test_function!(
            BTrimFunc::new(),
            vec![
                ColumnarValue::Scalar(ScalarValue::Utf8View(Some(String::from(
                    "alphabet"
                )))),
                ColumnarValue::Scalar(ScalarValue::Utf8View(Some(String::from("t")))),
            ],
            Ok(Some("alphabe")),
            &str,
            Utf8View,
            StringViewArray
        );
        test_function!(
            BTrimFunc::new(),
            vec![
                ColumnarValue::Scalar(ScalarValue::Utf8View(Some(String::from(
                    "alphabet"
                )))),
                ColumnarValue::Scalar(ScalarValue::Utf8View(Some(String::from(
                    "alphabe"
                )))),
            ],
            Ok(Some("t")),
            &str,
            Utf8View,
            StringViewArray
        );
        test_function!(
            BTrimFunc::new(),
            vec![
                ColumnarValue::Scalar(ScalarValue::Utf8View(Some(String::from(
                    "alphabet"
                )))),
                ColumnarValue::Scalar(ScalarValue::Utf8View(None)),
            ],
            Ok(None),
            &str,
            Utf8View,
            StringViewArray
        );
        // Special string view case for checking unlined output(len > 12)
        test_function!(
            BTrimFunc::new(),
            vec![
                ColumnarValue::Scalar(ScalarValue::Utf8View(Some(String::from(
                    "xxxalphabetalphabetxxx"
                )))),
                ColumnarValue::Scalar(ScalarValue::Utf8View(Some(String::from("x")))),
            ],
            Ok(Some("alphabetalphabet")),
            &str,
            Utf8View,
            StringViewArray
        );
        // String cases
        test_function!(
            BTrimFunc::new(),
            vec![ColumnarValue::Scalar(ScalarValue::Utf8(Some(
                String::from("alphabet  ")
            ))),],
            Ok(Some("alphabet")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            BTrimFunc::new(),
            vec![ColumnarValue::Scalar(ScalarValue::Utf8(Some(
                String::from("alphabet  ")
            ))),],
            Ok(Some("alphabet")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            BTrimFunc::new(),
            vec![
                ColumnarValue::Scalar(ScalarValue::Utf8(Some(String::from("alphabet")))),
                ColumnarValue::Scalar(ScalarValue::Utf8(Some(String::from("t")))),
            ],
            Ok(Some("alphabe")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            BTrimFunc::new(),
            vec![
                ColumnarValue::Scalar(ScalarValue::Utf8(Some(String::from("alphabet")))),
                ColumnarValue::Scalar(ScalarValue::Utf8(Some(String::from("alphabe")))),
            ],
            Ok(Some("t")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            BTrimFunc::new(),
            vec![
                ColumnarValue::Scalar(ScalarValue::Utf8(Some(String::from("alphabet")))),
                ColumnarValue::Scalar(ScalarValue::Utf8(None)),
            ],
            Ok(None),
            &str,
            Utf8,
            StringArray
        );
    }
}
