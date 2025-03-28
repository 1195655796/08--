use core::fmt;
use std::sync::Arc;

use arrow::datatypes::{DataType, Field};
use datafusion::{
    dataframe::DataFrame,
    functions::expr_fn::length,
    functions_array::length::array_length,
    logical_expr::{avg, case, cast, col, count, is_null, lit, max, median, min, stddev, sum},
};
use datafusion::logical_expr::approx_percentile_cont;



#[allow(unused)]
#[derive(Debug)]
pub enum DescribeMethod {
    Total,
    NullTotal,
    Mean,
    Stddev,
    Min,
    Max,
    Median,
    Percentile(u8),
}

#[derive(Debug)]
pub struct DataFrameDescriber {
    original: DataFrame,
    transformed: DataFrame,
    methods: Vec<DescribeMethod>,
}

impl DataFrameDescriber {
    pub fn try_new(df: DataFrame) -> anyhow::Result<Self> {
        let fields = df.schema().fields().iter();
        // change all temporal columns to Float64
        let expressions = fields
            .map(|field| {
                let dt = field.data_type();
                let expr = match dt {
                    dt if dt.is_temporal() => cast(col(field.name()), DataType::Float64),
                    dt if dt.is_numeric() => col(field.name()),
                    DataType::List(_) | DataType::LargeList(_) => array_length(col(field.name())),
                    _ => length(cast(col(field.name()), DataType::Utf8)),
                };
                expr.alias(field.name())
            })
            .collect();

        let transformed = df.clone().select(expressions)?;

        Ok(Self {
            original: df,
            transformed,
            methods: vec![
                DescribeMethod::Total,
                DescribeMethod::NullTotal,
                DescribeMethod::Mean,
                DescribeMethod::Stddev,
                DescribeMethod::Min,
                DescribeMethod::Max,
                DescribeMethod::Median,
                // 作业：实现 25th, 50th, 75th percentile
                DescribeMethod::Percentile(25),
                DescribeMethod::Percentile(50),
                DescribeMethod::Percentile(75),
            ],
        })
    }

    pub async fn describe(&self) -> anyhow::Result<DataFrame> {
        let df = self.do_describe().await?;
        self.cast_back(df)
    }

    async fn do_describe(&self) -> anyhow::Result<DataFrame> {
        let df: Option<DataFrame> = self.methods.iter().fold(None, |acc, method| {
            let df = self.transformed.clone();
            let stat_df = match method {
                DescribeMethod::Total => total(df).unwrap(),
                DescribeMethod::NullTotal => null_total(df).unwrap(),
                DescribeMethod::Mean => mean(df).unwrap(),
                DescribeMethod::Stddev => std_div(df).unwrap(),
                DescribeMethod::Min => minimum(df).unwrap(),
                DescribeMethod::Max => maximum(df).unwrap(),
                DescribeMethod::Median => med(df).unwrap(),
                DescribeMethod::Percentile(p) => percentile(df, *p as f64 / 100.0).unwrap(),
            };
            // add a new column to the beginning of the DataFrame
            let mut select_expr = vec![lit(method.to_string()).alias("describe")];
            select_expr.extend(stat_df.schema().fields().iter().map(|f| col(f.name())));

            let stat_df = stat_df.select(select_expr).unwrap();

            match acc {
                Some(acc) => Some(acc.union(stat_df).unwrap()),
                None => Some(stat_df),
            }
        });

        df.ok_or_else(|| anyhow::anyhow!("No statistics found"))
    }

    fn cast_back(&self, df: DataFrame) -> anyhow::Result<DataFrame> {
        // we need the describe column
        let describe = Arc::new(Field::new("describe", DataType::Utf8, false));
        let mut fields = vec![&describe];
        fields.extend(self.original.schema().fields().iter());
        let expressions = fields
            .into_iter()
            .map(|field| {
                let dt = field.data_type();
                let expr = match dt {
                    dt if dt.is_temporal() => cast(col(field.name()), dt.clone()),
                    DataType::List(_) | DataType::LargeList(_) => {
                        cast(col(field.name()), DataType::Int32)
                    }
                    _ => col(field.name()),
                };
                expr.alias(field.name())
            })
            .collect();

        Ok(df
            .select(expressions)?
            .sort(vec![col("describe").sort(true, false)])?)
    }
}

impl fmt::Display for DescribeMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DescribeMethod::Total => write!(f, "total"),
            DescribeMethod::NullTotal => write!(f, "null_total"),
            DescribeMethod::Mean => write!(f, "mean"),
            DescribeMethod::Stddev => write!(f, "stddev"),
            DescribeMethod::Min => write!(f, "min"),
            DescribeMethod::Max => write!(f, "max"),
            DescribeMethod::Median => write!(f, "median"),
            DescribeMethod::Percentile(p) => write!(f, "percentile_{}", p),
        }
    }
}

macro_rules! describe_method {
    ($name:ident, $method:ident) => {
        fn $name(df: DataFrame) -> anyhow::Result<DataFrame> {
            let fields = df.schema().fields().iter();
            let ret = df.clone().aggregate(
                vec![],
                fields
                    .filter(|f| f.data_type().is_numeric())
                    .map(|f| $method(col(f.name())).alias(f.name()))
                    .collect::<Vec<_>>(),
            )?;
            Ok(ret)
        }
    };
}

describe_method!(total, count);
describe_method!(mean, avg);
describe_method!(std_div, stddev);
describe_method!(minimum, min);
describe_method!(maximum, max);
describe_method!(med, median);

fn null_total(df: DataFrame) -> anyhow::Result<DataFrame> {
    let fields = df.schema().fields().iter();
    let ret = df.clone().aggregate(
        vec![],
        fields
            .map(|f| {
                sum(case(is_null(col(f.name())))
                    .when(lit(true), lit(1))
                    .otherwise(lit(0))
                    .unwrap())
                .alias(f.name())
            })
            .collect::<Vec<_>>(),
    )?;
    Ok(ret)
}

fn percentile(df: DataFrame, percentile: f64) -> anyhow::Result<DataFrame> {
    let fields = df.schema().fields().iter();
    let ret = df.clone().aggregate(
        vec![],
        fields
            .filter(|f| f.data_type().is_numeric())
            .map(|f| approx_percentile_cont(col(f.name()), lit(percentile)).alias(f.name()))
            .collect::<Vec<_>>(),
    )?;
    Ok(ret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::prelude::*;
    use arrow::record_batch::RecordBatch;
    use arrow::array::{Float64Array, Int32Array, StringArray, ArrayRef};
    use arrow::datatypes::Schema;
    use std::sync::Arc;


    fn create_test_dataframe() -> DataFrame {
        // Create a simple RecordBatch
        let schema = Arc::new(Schema::new(vec![
            Field::new("float_col", DataType::Float64, false),
            Field::new("int_col", DataType::Int32, false),
            Field::new("string_col", DataType::Utf8, false),
        ]));

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0,4.0])) as ArrayRef,
                Arc::new(Int32Array::from(vec![4, 5, 6, 7])) as ArrayRef,
                Arc::new(StringArray::from(vec!["a", "b", "c", "d"])) as ArrayRef,
            ],
        ).unwrap();

        // Convert the RecordBatch to a DataFrame
        let ctx = SessionContext::new();
        ctx.read_batch(batch).unwrap()
    }

    #[tokio::test]
    async fn test_try_new() {
        let df = create_test_dataframe();
        let describer = DataFrameDescriber::try_new(df);
        assert!(describer.is_ok());
    }

    #[tokio::test]
    async fn test_describe_total() {
        let df = create_test_dataframe();
        let describer = DataFrameDescriber::try_new(df).unwrap();

        let described_df = describer.describe().await;
        assert!(described_df.is_ok());

        let result = described_df.unwrap().collect().await.unwrap();
        assert_eq!(result.len(), 1);  // should have one row for each statistic
        assert_eq!(result[0].num_columns(), 4);  // describe + three columns
    }
    #[tokio::test]
    async fn test_percentile() {
        let df = create_test_dataframe();
        let percentile_df = percentile(df.clone(), 0.5).unwrap();
        
        let result = percentile_df.collect().await.unwrap();
        
        // Verify the percentile values
        assert_eq!(result.len(), 1); // should have one row for the percentile
        assert_eq!(result[0].num_columns(), 2); // percentile + one column
        
        let float_col = result[0].column(0).as_any().downcast_ref::<Float64Array>().unwrap();
        assert_eq!(float_col.value(0), 2.5); // median of [1.0, 2.0, 3.0, 4.0] is 2.5
        
        let int_col = result[0].column(1).as_any().downcast_ref::<Int32Array>().unwrap();
        assert_eq!(int_col.value(0), 5); // median of [4, 5, 6, 7] is 5.5 but approx_percentile_cont rounds down to 5
    }
}
