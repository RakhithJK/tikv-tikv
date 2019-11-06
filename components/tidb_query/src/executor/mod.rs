// Copyright 2017 TiKV Project Authors. Licensed under Apache-2.0.

mod aggregate;
mod aggregation;
mod index_scan;
mod limit;
pub mod runner;
mod scan;
mod selection;
mod table_scan;
mod topn;
mod topn_heap;

pub use self::aggregation::{HashAggExecutor, StreamAggExecutor};
pub use self::index_scan::IndexScanExecutor;
pub use self::limit::LimitExecutor;
pub use self::runner::ExecutorsRunner;
pub use self::scan::{ScanExecutor, ScanExecutorOptions};
pub use self::selection::SelectionExecutor;
pub use self::table_scan::TableScanExecutor;
pub use self::topn::TopNExecutor;

use std::sync::Arc;

use codec::prelude::NumberDecoder;
use tidb_query_datatype::prelude::*;
use tidb_query_datatype::FieldTypeFlag;
use tikv_util::collections::HashSet;
use tipb::ColumnInfo;
use tipb::{Expr, ExprType};

use crate::codec::datum::{self, Datum, DatumEncoder};
use crate::codec::table::{self, RowColsDict};
use crate::execute_stats::*;
use crate::expr::{EvalContext, EvalWarnings};
use crate::storage::IntervalRange;
use crate::util;
use crate::Result;

/// An expression tree visitor that extracts all column offsets in the tree.
pub struct ExprColumnRefVisitor {
    cols_offset: HashSet<usize>,
    cols_len: usize,
}

impl ExprColumnRefVisitor {
    pub fn new(cols_len: usize) -> ExprColumnRefVisitor {
        ExprColumnRefVisitor {
            cols_offset: HashSet::default(),
            cols_len,
        }
    }

    pub fn visit(&mut self, expr: &Expr) -> Result<()> {
        if expr.get_tp() == ExprType::ColumnRef {
            let offset = box_try!(expr.get_val().read_i64()) as usize;
            if offset >= self.cols_len {
                return Err(other_err!(
                    "offset {} overflow, should be less than {}",
                    offset,
                    self.cols_len
                ));
            }
            self.cols_offset.insert(offset);
        } else {
            self.batch_visit(expr.get_children())?;
        }
        Ok(())
    }

    pub fn batch_visit(&mut self, exprs: &[Expr]) -> Result<()> {
        for expr in exprs {
            self.visit(expr)?;
        }
        Ok(())
    }

    pub fn column_offsets(self) -> Vec<usize> {
        self.cols_offset.into_iter().collect()
    }
}

#[derive(Debug)]
pub struct OriginCols {
    pub handle: i64,
    pub data: RowColsDict,
    cols: Arc<Vec<ColumnInfo>>,
}

/// Row generated by aggregation.
#[derive(Debug)]
pub struct AggCols {
    // row's suffix, may be the binary of the group by key.
    suffix: Vec<u8>,
    value: Vec<Datum>,
}

impl AggCols {
    pub fn get_binary(&self) -> Result<Vec<u8>> {
        let mut value =
            Vec::with_capacity(self.suffix.len() + datum::approximate_size(&self.value, false));
        box_try!(value.write_datum(&self.value, false));
        if !self.suffix.is_empty() {
            value.extend_from_slice(&self.suffix);
        }
        Ok(value)
    }
}

#[derive(Debug)]
pub enum Row {
    Origin(OriginCols),
    Agg(AggCols),
}

impl Row {
    pub fn origin(handle: i64, data: RowColsDict, cols: Arc<Vec<ColumnInfo>>) -> Row {
        Row::Origin(OriginCols::new(handle, data, cols))
    }

    pub fn agg(value: Vec<Datum>, suffix: Vec<u8>) -> Row {
        Row::Agg(AggCols { suffix, value })
    }

    pub fn take_origin(self) -> Result<OriginCols> {
        match self {
            Row::Origin(row) => Ok(row),
            _ => Err(other_err!(
                "unexpected: aggregation columns cannot take origin"
            )),
        }
    }

    pub fn get_binary(&self, output_offsets: &[u32]) -> Result<Vec<u8>> {
        match self {
            Row::Origin(row) => row.get_binary(output_offsets),
            Row::Agg(row) => row.get_binary(), // ignore output offsets for aggregation.
        }
    }
}

impl OriginCols {
    pub fn new(handle: i64, data: RowColsDict, cols: Arc<Vec<ColumnInfo>>) -> OriginCols {
        OriginCols { handle, data, cols }
    }

    // get binary of each column in order of columns
    pub fn get_binary_cols(&self) -> Result<Vec<Vec<u8>>> {
        let mut res = Vec::with_capacity(self.cols.len());
        for col in self.cols.iter() {
            if col.get_pk_handle() {
                let v = util::get_pk(col, self.handle);
                let bt = box_try!(datum::encode_value(&[v]));
                res.push(bt);
                continue;
            }
            let col_id = col.get_column_id();
            let value = match self.data.get(col_id) {
                None if col.has_default_val() => col.get_default_val().to_vec(),
                None if col.as_accessor().flag().contains(FieldTypeFlag::NOT_NULL) => {
                    return Err(other_err!(
                        "column {} of {} is missing",
                        col_id,
                        self.handle
                    ));
                }
                None => box_try!(datum::encode_value(&[Datum::Null])),
                Some(bs) => bs.to_vec(),
            };
            res.push(value);
        }
        Ok(res)
    }

    pub fn get_binary(&self, output_offsets: &[u32]) -> Result<Vec<u8>> {
        // TODO capacity is not enough
        let mut values = Vec::with_capacity(self.data.value.len());
        for offset in output_offsets {
            let col = &self.cols[*offset as usize];
            let col_id = col.get_column_id();
            match self.data.get(col_id) {
                Some(value) => values.extend_from_slice(value),
                None if col.get_pk_handle() => {
                    let pk = util::get_pk(col, self.handle);
                    box_try!(values.write_datum(&[pk], false));
                }
                None if col.has_default_val() => {
                    values.extend_from_slice(col.get_default_val());
                }
                None if col.as_accessor().flag().contains(FieldTypeFlag::NOT_NULL) => {
                    return Err(other_err!(
                        "column {} of {} is missing",
                        col_id,
                        self.handle
                    ));
                }
                None => {
                    box_try!(values.write_datum(&[Datum::Null], false));
                }
            }
        }
        Ok(values)
    }

    // inflate with the real value(Datum) for each columns in offsets
    // inflate with Datum::Null for those cols not in offsets.
    // It's used in expression since column is marked with offset
    // in expression.
    pub fn inflate_cols_with_offsets(
        &self,
        ctx: &EvalContext,
        offsets: &[usize],
    ) -> Result<Vec<Datum>> {
        let mut res = vec![Datum::Null; self.cols.len()];
        for offset in offsets {
            let col = &self.cols[*offset];
            if col.get_pk_handle() {
                let v = util::get_pk(col, self.handle);
                res[*offset] = v;
            } else {
                let col_id = col.get_column_id();
                let value = match self.data.get(col_id) {
                    None if col.has_default_val() => {
                        // TODO: optimize it to decode default value only once.
                        box_try!(table::decode_col_value(
                            &mut col.get_default_val(),
                            ctx,
                            col
                        ))
                    }
                    None if col.as_accessor().flag().contains(FieldTypeFlag::NOT_NULL) => {
                        return Err(other_err!(
                            "column {} of {} is missing",
                            col_id,
                            self.handle
                        ));
                    }
                    None => Datum::Null,
                    Some(mut bs) => box_try!(table::decode_col_value(&mut bs, ctx, col)),
                };
                res[*offset] = value;
            }
        }
        Ok(res)
    }
}

pub trait Executor: Send {
    type StorageStats;

    fn next(&mut self) -> Result<Option<Row>>;

    fn collect_exec_stats(&mut self, dest: &mut ExecuteStats);

    fn collect_storage_stats(&mut self, dest: &mut Self::StorageStats);

    fn get_len_of_columns(&self) -> usize;

    fn take_eval_warnings(&mut self) -> Option<EvalWarnings>;

    fn take_scanned_range(&mut self) -> IntervalRange;

    fn with_summary_collector<C: ExecSummaryCollector>(
        self,
        summary_collector: C,
    ) -> WithSummaryCollector<C, Self>
    where
        Self: Sized,
    {
        WithSummaryCollector {
            summary_collector,
            inner: self,
        }
    }
}

impl<C: ExecSummaryCollector + Send, T: Executor> Executor for WithSummaryCollector<C, T> {
    type StorageStats = T::StorageStats;

    fn next(&mut self) -> Result<Option<Row>> {
        let timer = self.summary_collector.on_start_iterate();
        let ret = self.inner.next();
        if let Ok(Some(_)) = ret {
            self.summary_collector.on_finish_iterate(timer, 1);
        } else {
            self.summary_collector.on_finish_iterate(timer, 0);
        }
        ret
    }

    fn collect_exec_stats(&mut self, dest: &mut ExecuteStats) {
        self.summary_collector
            .collect(&mut dest.summary_per_executor);
        self.inner.collect_exec_stats(dest);
    }

    #[inline]
    fn collect_storage_stats(&mut self, dest: &mut Self::StorageStats) {
        self.inner.collect_storage_stats(dest);
    }

    #[inline]
    fn get_len_of_columns(&self) -> usize {
        self.inner.get_len_of_columns()
    }

    #[inline]
    fn take_eval_warnings(&mut self) -> Option<EvalWarnings> {
        self.inner.take_eval_warnings()
    }

    #[inline]
    fn take_scanned_range(&mut self) -> IntervalRange {
        self.inner.take_scanned_range()
    }
}

impl<T: Executor + ?Sized> Executor for Box<T> {
    type StorageStats = T::StorageStats;

    #[inline]
    fn next(&mut self) -> Result<Option<Row>> {
        (**self).next()
    }

    #[inline]
    fn collect_exec_stats(&mut self, dest: &mut ExecuteStats) {
        (**self).collect_exec_stats(dest);
    }

    #[inline]
    fn collect_storage_stats(&mut self, dest: &mut Self::StorageStats) {
        (**self).collect_storage_stats(dest);
    }

    #[inline]
    fn get_len_of_columns(&self) -> usize {
        (**self).get_len_of_columns()
    }

    #[inline]
    fn take_eval_warnings(&mut self) -> Option<EvalWarnings> {
        (**self).take_eval_warnings()
    }

    #[inline]
    fn take_scanned_range(&mut self) -> IntervalRange {
        (**self).take_scanned_range()
    }
}

#[cfg(test)]
pub mod tests {
    use super::{Executor, TableScanExecutor};
    use crate::codec::{datum, table, Datum};
    use crate::storage::fixture::FixtureStorage;
    use codec::prelude::NumberEncoder;
    use kvproto::coprocessor::KeyRange;
    use tidb_query_datatype::{FieldTypeAccessor, FieldTypeTp};
    use tikv_util::collections::HashMap;
    use tikv_util::map;
    use tipb::ColumnInfo;
    use tipb::TableScan;
    use tipb::{Expr, ExprType};

    pub fn build_expr(tp: ExprType, id: Option<i64>, child: Option<Expr>) -> Expr {
        let mut expr = Expr::default();
        expr.set_tp(tp);
        if tp == ExprType::ColumnRef {
            expr.mut_val().write_i64(id.unwrap()).unwrap();
        } else {
            expr.mut_children().push(child.unwrap());
        }
        expr
    }

    pub fn new_col_info(cid: i64, tp: FieldTypeTp) -> ColumnInfo {
        let mut col_info = ColumnInfo::default();
        col_info.as_mut_accessor().set_tp(tp);
        col_info.set_column_id(cid);
        col_info
    }

    // the first column should be i64 since it will be used as row handle
    pub fn gen_table_data(
        tid: i64,
        cis: &[ColumnInfo],
        rows: &[Vec<Datum>],
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut kv_data = Vec::new();
        let col_ids: Vec<i64> = cis.iter().map(|c| c.get_column_id()).collect();
        for cols in rows.iter() {
            let col_values: Vec<_> = cols.to_vec();
            let value = table::encode_row(col_values, &col_ids).unwrap();
            let key = table::encode_row_key(tid, cols[0].i64());
            kv_data.push((key, value));
        }
        kv_data
    }

    pub fn get_point_range(table_id: i64, handle: i64) -> KeyRange {
        let start_key = table::encode_row_key(table_id, handle);
        let mut end = start_key.clone();
        crate::util::convert_to_prefix_next(&mut end);
        let mut key_range = KeyRange::default();
        key_range.set_start(start_key);
        key_range.set_end(end);
        key_range
    }

    #[inline]
    pub fn get_range(table_id: i64, start: i64, end: i64) -> KeyRange {
        let mut key_range = KeyRange::default();
        key_range.set_start(table::encode_row_key(table_id, start));
        key_range.set_end(table::encode_row_key(table_id, end));
        key_range
    }

    pub fn generate_index_data(
        table_id: i64,
        index_id: i64,
        handle: i64,
        col_val: &Datum,
        unique: bool,
    ) -> (HashMap<i64, Vec<u8>>, Vec<u8>) {
        let indice = vec![(2, (*col_val).clone()), (3, Datum::Dec(handle.into()))];
        let mut expect_row = HashMap::default();
        let mut v: Vec<_> = indice
            .iter()
            .map(|&(ref cid, ref value)| {
                expect_row.insert(*cid, datum::encode_key(&[value.clone()]).unwrap());
                value.clone()
            })
            .collect();
        if !unique {
            v.push(Datum::I64(handle));
        }
        let encoded = datum::encode_key(&v).unwrap();
        let idx_key = table::encode_index_seek_key(table_id, index_id, &encoded);
        (expect_row, idx_key)
    }

    pub struct TableData {
        pub kv_data: Vec<(Vec<u8>, Vec<u8>)>,
        // expect_rows[row_id][column_id]=>value
        pub expect_rows: Vec<HashMap<i64, Vec<u8>>>,
        pub cols: Vec<ColumnInfo>,
    }

    impl TableData {
        pub fn prepare(key_number: usize, table_id: i64) -> TableData {
            let cols = vec![
                new_col_info(1, FieldTypeTp::LongLong),
                new_col_info(2, FieldTypeTp::VarChar),
                new_col_info(3, FieldTypeTp::NewDecimal),
            ];

            let mut kv_data = Vec::new();
            let mut expect_rows = Vec::new();

            for handle in 0..key_number {
                let row = map![
                    1 => Datum::I64(handle as i64),
                    2 => Datum::Bytes(b"abc".to_vec()),
                    3 => Datum::Dec(10.into())
                ];
                let mut expect_row = HashMap::default();
                let col_ids: Vec<_> = row.iter().map(|(&id, _)| id).collect();
                let col_values: Vec<_> = row
                    .iter()
                    .map(|(cid, v)| {
                        let f = table::flatten(v.clone()).unwrap();
                        let value = datum::encode_value(&[f]).unwrap();
                        expect_row.insert(*cid, value);
                        v.clone()
                    })
                    .collect();

                let value = table::encode_row(col_values, &col_ids).unwrap();
                let key = table::encode_row_key(table_id, handle as i64);
                expect_rows.push(expect_row);
                kv_data.push((key, value));
            }
            Self {
                kv_data,
                expect_rows,
                cols,
            }
        }

        pub fn get_prev_2_cols(&self) -> Vec<ColumnInfo> {
            let col1 = self.cols[0].clone();
            let col2 = self.cols[1].clone();
            vec![col1, col2]
        }

        pub fn get_col_pk(&self) -> ColumnInfo {
            let mut pk_col = new_col_info(0, FieldTypeTp::Long);
            pk_col.set_pk_handle(true);
            pk_col
        }
    }

    pub fn gen_table_scan_executor(
        tid: i64,
        cis: Vec<ColumnInfo>,
        raw_data: &[Vec<Datum>],
        key_ranges: Option<Vec<KeyRange>>,
    ) -> Box<dyn Executor<StorageStats = ()> + Send> {
        let table_data = gen_table_data(tid, &cis, raw_data);
        let storage = FixtureStorage::from(table_data);

        let mut table_scan = TableScan::default();
        table_scan.set_table_id(tid);
        table_scan.set_columns(cis.clone().into());

        let key_ranges = key_ranges.unwrap_or_else(|| vec![get_range(tid, 0, i64::max_value())]);
        Box::new(TableScanExecutor::table_scan(table_scan, key_ranges, storage, false).unwrap())
    }
}
