use std::collections::{BTreeMap, HashMap, HashSet, LinkedList};
use std::mem::size_of_val;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use datafusion::arrow::datatypes::TimeUnit;
use flatbuffers::{ForwardsUOffset, Vector};
use memory_pool::{MemoryConsumer, MemoryPoolRef, MemoryReservation};
use minivec::MiniVec;
use models::field_value::{DataType, FieldVal};
use models::predicate::domain::TimeRange;
use models::schema::{
    timestamp_convert, ColumnType, Precision, TableColumn, TskvTableSchema, TskvTableSchemaRef,
};
use models::utils::split_id;
use models::{ColumnId, FieldId, RwLockRef, SchemaId, SeriesId, Timestamp};
use parking_lot::RwLock;
use protos::models::{Column, FieldType};
use trace::error;
use utils::bitset::ImmutBitSet;

use crate::error::Result;
use crate::tseries_family::Version;
use crate::tsm2::writer::{Column as ColumnData, DataBlock2};
use crate::tsm2::TsmWriteData;
use crate::{Error, TseriesFamilyId};

// use skiplist::ordered_skiplist::OrderedSkipList;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RowData {
    pub ts: i64,
    pub fields: Vec<Option<FieldVal>>,
}

impl RowData {
    pub fn point_to_row_data(
        schema: &TskvTableSchema,
        from_precision: Precision,
        columns: &Vector<ForwardsUOffset<Column>>,
        fields_idx: &[usize],
        ts_idx: usize,
        row_count: usize,
    ) -> Result<RowData> {
        let mut has_fields = false;
        let mut fields = vec![None; schema.field_num()];
        let fields_id = schema.fields_id();
        for field_id in fields_idx {
            let column = columns.get(*field_id);
            let column_name = column.name_ext()?;
            let column_nullbit = column.nullbit_ext()?;
            match column.field_type() {
                FieldType::Integer => {
                    let len = column.int_values_len()?;
                    let column_nullbits =
                        ImmutBitSet::new_without_check(len, column_nullbit.bytes());
                    if !column_nullbits.get(row_count) {
                        continue;
                    }
                    let val = column.int_values()?.get(row_count);
                    match schema.column(column_name) {
                        None => {
                            error!("column {} not found in schema", column_name);
                        }
                        Some(column) => {
                            let field_id = column.id;
                            let field_idx = fields_id.get(&field_id).unwrap();
                            fields[*field_idx] = Some(FieldVal::Integer(val));
                            has_fields = true;
                        }
                    }
                }
                FieldType::Float => {
                    let len = column.float_values_len()?;
                    let column_nullbits =
                        ImmutBitSet::new_without_check(len, column_nullbit.bytes());
                    if !column_nullbits.get(row_count) {
                        continue;
                    }
                    let val = column.float_values()?.get(row_count);
                    match schema.column(column_name) {
                        None => {
                            error!("column {} not found in schema", column_name);
                        }
                        Some(column) => {
                            let field_id = column.id;
                            let field_idx = fields_id.get(&field_id).unwrap();
                            fields[*field_idx] = Some(FieldVal::Float(val));
                            has_fields = true;
                        }
                    }
                }
                FieldType::Unsigned => {
                    let len = column.uint_values_len()?;
                    let column_nullbits =
                        ImmutBitSet::new_without_check(len, column_nullbit.bytes());
                    if !column_nullbits.get(row_count) {
                        continue;
                    }
                    let val = column.uint_values()?.get(row_count);
                    match schema.column(column_name) {
                        None => {
                            error!("column {} not found in schema", column_name);
                        }
                        Some(column) => {
                            let field_id = column.id;
                            let field_idx = fields_id.get(&field_id).unwrap();
                            fields[*field_idx] = Some(FieldVal::Unsigned(val));
                            has_fields = true;
                        }
                    }
                }
                FieldType::Boolean => {
                    let len = column.bool_values_len()?;
                    let column_nullbits =
                        ImmutBitSet::new_without_check(len, column_nullbit.bytes());
                    if !column_nullbits.get(row_count) {
                        continue;
                    }
                    let val = column.bool_values()?.get(row_count);
                    match schema.column(column_name) {
                        None => {
                            error!("column {} not found in schema", column_name);
                        }
                        Some(column) => {
                            let field_id = column.id;
                            let field_idx = fields_id.get(&field_id).unwrap();
                            fields[*field_idx] = Some(FieldVal::Boolean(val));
                            has_fields = true;
                        }
                    }
                }
                FieldType::String => {
                    let len = column.string_values_len()?;
                    let column_nullbits =
                        ImmutBitSet::new_without_check(len, column_nullbit.bytes());
                    if !column_nullbits.get(row_count) {
                        continue;
                    }
                    let val = column.string_values()?.get(row_count);
                    match schema.column(column_name) {
                        None => {
                            error!("column {} not found in schema", column_name);
                        }
                        Some(column) => {
                            let field_id = column.id;
                            let field_idx = fields_id.get(&field_id).unwrap();
                            fields[*field_idx] =
                                Some(FieldVal::Bytes(MiniVec::from(val.as_bytes())));
                            has_fields = true;
                        }
                    }
                }
                _ => {
                    error!("unsupported field type");
                }
            }
        }

        if !has_fields {
            return Err(Error::InvalidPoint);
        }

        let ts_column = columns.get(ts_idx);
        let ts = ts_column.int_values()?.get(row_count);
        let to_precision = schema.time_column_precision();
        let ts = timestamp_convert(from_precision, to_precision, ts).ok_or(Error::CommonError {
            reason: "timestamp overflow".to_string(),
        })?;

        Ok(RowData { ts, fields })
    }

    pub fn size(&self) -> usize {
        let mut size = 0;
        for i in self.fields.iter() {
            match i {
                None => {
                    size += size_of_val(i);
                }
                Some(v) => {
                    size += size_of_val(i) + v.heap_size();
                }
            }
        }
        size += size_of_val(&self.ts);
        size += size_of_val(&self.fields);
        size
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct RowGroup {
    pub schema: Arc<TskvTableSchema>,
    pub range: TimeRange,
    pub rows: LinkedList<RowData>,
    // pub rows: OrderedSkipList<RowData>,
    /// total size in stack and heap
    pub size: usize,
}

#[derive(Debug)]
pub struct SeriesData {
    pub series_id: SeriesId,
    pub range: TimeRange,
    pub groups: LinkedList<RowGroup>,
}

impl SeriesData {
    fn new(series_id: SeriesId) -> Self {
        Self {
            series_id,
            range: TimeRange {
                min_ts: i64::MAX,
                max_ts: i64::MIN,
            },
            groups: LinkedList::new(),
        }
    }

    pub fn write(&mut self, mut group: RowGroup) {
        self.range.merge(&group.range);

        for item in self.groups.iter_mut() {
            if item.schema.schema_id == group.schema.schema_id {
                item.range.merge(&group.range);
                item.rows.append(&mut group.rows);
                item.schema = group.schema;
                return;
            }
        }

        self.groups.push_back(group);
    }

    pub fn drop_column(&mut self, column_id: ColumnId) {
        for item in self.groups.iter_mut() {
            let name = match item.schema.column_name(column_id) {
                None => continue,
                Some(name) => name.to_string(),
            };
            let index = match item.schema.fields_id().get(&column_id) {
                None => continue,
                Some(index) => *index,
            };
            for row in item.rows.iter_mut() {
                row.fields.remove(index);
            }
            let mut schema_t = item.schema.as_ref().clone();
            schema_t.drop_column(&name);
            //schema_t.schema_id += 1;
            item.schema = Arc::new(schema_t)
        }
    }

    pub fn change_column(&mut self, column_name: &str, new_column: &TableColumn) {
        for item in self.groups.iter_mut() {
            let mut schema_t = item.schema.as_ref().clone();
            schema_t.change_column(column_name, new_column.clone());
            schema_t.schema_id += 1;
            item.schema = Arc::new(schema_t)
        }
    }

    pub fn add_column(&mut self, new_column: &TableColumn) {
        for item in self.groups.iter_mut() {
            let mut schema_t = item.schema.as_ref().clone();
            schema_t.add_column(new_column.clone());
            schema_t.schema_id += 1;
            item.schema = Arc::new(schema_t)
        }
    }

    pub fn delete_series(&mut self, range: &TimeRange) {
        if range.max_ts < self.range.min_ts || range.min_ts > self.range.max_ts {
            return;
        }

        for item in self.groups.iter_mut() {
            item.rows = item
                .rows
                .iter()
                .filter(|row| row.ts < range.min_ts || row.ts > range.max_ts)
                .cloned()
                .collect();
        }
    }

    pub fn read_data(
        &self,
        column_id: ColumnId,
        mut time_predicate: impl FnMut(Timestamp) -> bool,
        mut value_predicate: impl FnMut(&FieldVal) -> bool,
        mut handle_data: impl FnMut(DataType),
    ) {
        for group in self.groups.iter() {
            let field_index = group.schema.fields_id();
            let index = match field_index.get(&column_id) {
                None => continue,
                Some(v) => v,
            };
            group
                .rows
                .iter()
                .filter(|row| time_predicate(row.ts))
                .for_each(|row| {
                    if let Some(Some(field)) = row.fields.get(*index) {
                        if value_predicate(field) {
                            handle_data(field.data_value(row.ts))
                        }
                    }
                });
        }
    }

    pub fn read_timestamps(
        &self,
        mut time_predicate: impl FnMut(Timestamp) -> bool,
        mut handle_data: impl FnMut(Timestamp),
    ) {
        for group in self.groups.iter() {
            group
                .rows
                .iter()
                .filter(|row| time_predicate(row.ts))
                .for_each(|row| handle_data(row.ts));
        }
    }

    pub fn flat_groups(&self) -> Vec<(SchemaId, TskvTableSchemaRef, &LinkedList<RowData>)> {
        self.groups
            .iter()
            .map(|g| (g.schema.schema_id, g.schema.clone(), &g.rows))
            .collect()
    }
    pub fn get_schema(&self) -> Option<Arc<TskvTableSchema>> {
        if let Some(item) = self.groups.back() {
            return Some(item.schema.clone());
        }
        None
    }
    pub fn build_data_block(
        &self,
        version: Arc<Version>,
    ) -> Result<Option<(String, DataBlock2, DataBlock2)>> {
        if let Some(schema) = self.get_schema() {
            let field_ids = schema.fields_id();

            let mut cols: Vec<ColumnData> = schema
                .fields()
                .iter()
                .map(|col| ColumnData::empty(col.column_type.clone()))
                .collect();
            let mut delta_cols = cols.clone();
            let mut time_array = ColumnData::empty(ColumnType::Time(TimeUnit::from(
                schema.time_column_precision(),
            )));

            let mut delta_time_array = time_array.clone();
            let mut cols_desc = vec![None; schema.field_num()];
            for (_schema_id, schema, rows) in self.flat_groups() {
                let values = dedup_and_sort_row_data(rows);
                for row in values {
                    if row.ts < version.max_level_ts {
                        delta_time_array.push(Some(FieldVal::Integer(row.ts)));
                    }
                    time_array.push(Some(FieldVal::Integer(row.ts)));
                    for col in schema.fields().iter() {
                        if let Some(index) = field_ids.get(&col.id) {
                            let field = row.fields.get(*index);
                            if row.ts < version.max_level_ts {
                                if let Some(val) = field {
                                    delta_cols[*index].push(val.clone());
                                } else {
                                    delta_cols[*index].push(None);
                                }
                            } else if let Some(val) = field {
                                cols[*index].push(val.clone());
                            } else {
                                cols[*index].push(None);
                            }
                            if cols_desc[*index].is_none() {
                                cols_desc[*index] = Some(col.clone());
                            }
                        }
                    }
                }
            }

            let cols_desc = cols_desc.into_iter().flatten().collect::<Vec<_>>();
            if cols_desc.len() != cols.len() {
                return Err(Error::CommonError {
                    reason: "Invalid cols_desc".to_string(),
                });
            }

            if !time_array.is_all_set() || !delta_time_array.is_all_set() {
                return Err(Error::CommonError {
                    reason: "Invalid time array in DataBlock".to_string(),
                });
            }
            return Ok(Some((
                schema.name.clone(),
                DataBlock2::new(
                    schema.clone(),
                    time_array,
                    schema.time_column(),
                    cols,
                    cols_desc.clone(),
                ),
                DataBlock2::new(
                    schema.clone(),
                    delta_time_array,
                    schema.time_column(),
                    delta_cols,
                    cols_desc,
                ),
            )));
        }
        Ok(None)
    }
}

pub struct MemCacheStatistics {
    tf_id: TseriesFamilyId,
    /// greater seq mean the last write
    seq_no: u64,
    statistics: HashMap<SeriesId, TimeRange>,
}

impl MemCacheStatistics {
    pub fn seq_no(&self) -> u64 {
        self.seq_no
    }
}

#[derive(Debug)]
pub struct MemCache {
    tf_id: TseriesFamilyId,

    flushing: AtomicBool,

    max_size: u64,
    min_seq_no: u64,

    // wal seq number
    seq_no: AtomicU64,
    memory: RwLock<MemoryReservation>,

    part_count: usize,
    partions: Vec<RwLock<HashMap<SeriesId, RwLockRef<SeriesData>>>>,
}

impl MemCache {
    pub fn to_chunk_group(&self, version: Arc<Version>) -> Result<(TsmWriteData, TsmWriteData)> {
        let partions: HashMap<SeriesId, Arc<RwLock<SeriesData>>> = self
            .partions
            .iter()
            .flat_map(|lock| {
                let inner_map = lock.read();
                let values = inner_map
                    .iter()
                    .map(|(id, rw_lock_ref)| (*id, rw_lock_ref.clone()))
                    .collect::<Vec<_>>();
                values
            })
            .collect();

        let mut chunk_group: TsmWriteData = BTreeMap::new();
        let mut delta_chunk_group: TsmWriteData = BTreeMap::new();
        partions
            .iter()
            .try_for_each(|(series_id, v)| -> Result<()> {
                let data = v.read();
                if let Some((table, datablock, delta_datablock)) =
                    data.build_data_block(version.clone())?
                {
                    if datablock.len() != 0 {
                        if let Some(chunk) = chunk_group.get_mut(&table) {
                            chunk.insert(*series_id, datablock);
                        } else {
                            let mut chunk = BTreeMap::new();
                            chunk.insert(*series_id, datablock);
                            chunk_group.insert(table.clone(), chunk);
                        }
                    }

                    if delta_datablock.len() != 0 {
                        if let Some(chunk) = delta_chunk_group.get_mut(&table) {
                            chunk.insert(*series_id, delta_datablock);
                        } else {
                            let mut chunk = BTreeMap::new();
                            chunk.insert(*series_id, delta_datablock);
                            chunk_group.insert(table, chunk);
                        }
                    }
                }
                Ok(())
            })?;
        Ok((chunk_group, delta_chunk_group))
    }
    pub fn new(
        tf_id: TseriesFamilyId,
        max_size: u64,
        part_count: usize,
        seq: u64,
        pool: &MemoryPoolRef,
    ) -> Self {
        let mut partions = Vec::with_capacity(part_count);
        for _i in 0..part_count {
            partions.push(RwLock::new(HashMap::new()));
        }
        let res =
            RwLock::new(MemoryConsumer::new(format!("memcache-{}-{}", tf_id, seq)).register(pool));
        Self {
            tf_id,
            flushing: AtomicBool::new(false),

            max_size,
            min_seq_no: seq,

            part_count,
            partions,

            seq_no: AtomicU64::new(seq),
            memory: res,
        }
    }

    pub fn write_group(&self, sid: SeriesId, seq: u64, group: RowGroup) -> Result<()> {
        self.seq_no.store(seq, Ordering::Relaxed);
        self.memory
            .write()
            .try_grow(group.size)
            .map_err(|_| Error::MemoryExhausted)?;
        let index = (sid as usize) % self.part_count;
        let mut series_map = self.partions[index].write();
        if let Some(series_data) = series_map.get(&sid) {
            let series_data_ptr = series_data.clone();
            let mut series_data_ptr_w = series_data_ptr.write();
            drop(series_map);
            series_data_ptr_w.write(group);
        } else {
            let mut series_data = SeriesData::new(sid);
            series_data.write(group);
            series_map.insert(sid, Arc::new(RwLock::new(series_data)));
        }
        Ok(())
    }

    pub fn read_field_data(
        &self,
        field_id: FieldId,
        time_predicate: impl FnMut(Timestamp) -> bool,
        value_predicate: impl FnMut(&FieldVal) -> bool,
        handle_data: impl FnMut(DataType),
    ) {
        let (column_id, sid) = split_id(field_id);
        let index = (sid as usize) % self.part_count;
        let series_data = self.partions[index].read().get(&sid).cloned();
        if let Some(series_data) = series_data {
            series_data
                .read()
                .read_data(column_id, time_predicate, value_predicate, handle_data)
        }
    }

    pub fn read_series_timestamps(
        &self,
        series_ids: &[SeriesId],
        mut time_predicate: impl FnMut(Timestamp) -> bool,
        mut handle_data: impl FnMut(Timestamp),
    ) {
        for sid in series_ids.iter() {
            let index = (*sid as usize) % self.part_count;
            let series_data = self.partions[index].read().get(sid).cloned();
            if let Some(series_data) = series_data {
                series_data
                    .read()
                    .read_timestamps(&mut time_predicate, &mut handle_data);
            }
        }
    }

    pub fn statistics(
        &self,
        series_ids: &[SeriesId],
        time_predicate: TimeRange,
    ) -> MemCacheStatistics {
        let mut statistics = HashMap::new();
        for sid in series_ids {
            let index = (*sid as usize) % self.part_count;
            let range = match self.partions[index].read().get(sid) {
                None => continue,
                Some(series_data) => series_data.read().range,
            };
            let time_predicate = match time_predicate.intersect(&range) {
                None => continue,
                Some(time_predicate) => time_predicate,
            };
            statistics.insert(*sid, time_predicate);
        }
        MemCacheStatistics {
            tf_id: self.tf_id,
            seq_no: self.min_seq_no,
            statistics,
        }
    }

    pub fn is_empty(&self) -> bool {
        for part in self.partions.iter() {
            if !part.read().is_empty() {
                return false;
            }
        }

        true
    }

    pub fn drop_columns(&self, field_ids: &[FieldId]) {
        for fid in field_ids {
            let (column_id, sid) = split_id(*fid);
            let index = (sid as usize) % self.part_count;
            let series_data = self.partions[index].read().get(&sid).cloned();
            if let Some(series_data) = series_data {
                series_data.write().drop_column(column_id);
            }
        }
    }

    pub fn change_column(&self, sids: &[SeriesId], column_name: &str, new_column: &TableColumn) {
        for sid in sids {
            let index = (*sid as usize) % self.part_count;
            let series_data = self.partions[index].read().get(sid).cloned();
            if let Some(series_data) = series_data {
                series_data.write().change_column(column_name, new_column);
            }
        }
    }

    pub fn add_column(&self, sids: &[SeriesId], new_column: &TableColumn) {
        for sid in sids {
            let index = (*sid as usize) % self.part_count;
            let series_data = self.partions[index].read().get(sid).cloned();
            if let Some(series_data) = series_data {
                series_data.write().add_column(new_column);
            }
        }
    }

    pub fn delete_series(&self, sids: &[SeriesId], range: &TimeRange) {
        for sid in sids {
            let index = (*sid as usize) % self.part_count;
            let series_data = self.partions[index].read().get(sid).cloned();
            if let Some(series_data) = series_data {
                series_data.write().delete_series(range);
            }
        }
    }

    pub fn read_series_data(&self) -> Vec<(SeriesId, Arc<RwLock<SeriesData>>)> {
        let mut ret = Vec::new();
        self.partions.iter().for_each(|p| {
            let p_rlock = p.read();
            for (k, v) in p_rlock.iter() {
                ret.push((*k, v.clone()));
            }
        });
        ret
    }

    pub fn mark_flushing(&self) -> bool {
        self.flushing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn is_flushing(&self) -> bool {
        self.flushing.load(Ordering::Relaxed)
    }

    pub fn is_full(&self) -> bool {
        self.memory.read().size() >= self.max_size as usize
    }

    pub fn tf_id(&self) -> TseriesFamilyId {
        self.tf_id
    }

    pub fn seq_no(&self) -> u64 {
        self.seq_no.load(Ordering::Relaxed)
    }

    pub fn min_seq_no(&self) -> u64 {
        self.min_seq_no
    }

    pub fn max_buf_size(&self) -> u64 {
        self.max_size
    }

    pub fn cache_size(&self) -> u64 {
        self.memory.read().size() as u64
    }
}

pub(crate) mod test {
    use std::collections::{HashMap, LinkedList};
    use std::mem::size_of;
    use std::sync::Arc;

    use models::field_value::FieldVal;
    use models::predicate::domain::TimeRange;
    use models::schema::TskvTableSchema;
    use models::{SchemaId, SeriesId, Timestamp};
    use parking_lot::RwLock;

    use super::{MemCache, RowData, RowGroup};

    pub fn put_rows_to_cache(
        cache: &MemCache,
        series_id: SeriesId,
        schema_id: SchemaId,
        mut schema: TskvTableSchema,
        time_range: (Timestamp, Timestamp),
        put_none: bool,
    ) {
        let mut rows = LinkedList::new();
        let mut size: usize = schema.size();
        for ts in time_range.0..=time_range.1 {
            let mut fields = Vec::new();
            for _ in 0..schema.columns().len() {
                size += size_of::<Option<FieldVal>>();
                if put_none {
                    fields.push(None);
                } else {
                    fields.push(Some(FieldVal::Float(ts as f64)));
                    size += 8;
                }
            }
            size += 8;
            rows.push_back(RowData { ts, fields });
        }

        schema.schema_id = schema_id;
        let row_group = RowGroup {
            schema: schema.into(),
            range: TimeRange::from(time_range),
            rows,
            size: size_of::<RowGroup>() + size,
        };
        cache.write_group(series_id, 1, row_group).unwrap();
    }

    pub fn get_one_series_cache_data(
        cache: Arc<RwLock<MemCache>>,
    ) -> HashMap<String, Vec<(Timestamp, FieldVal)>> {
        let mut fname_vals_map: HashMap<String, Vec<(Timestamp, FieldVal)>> = HashMap::new();
        let series_data = cache.read().read_series_data();
        for (_sid, sdata) in series_data {
            let sdata_rlock = sdata.read();
            let schema_groups = sdata_rlock.flat_groups();
            for (_sch_id, sch, row) in schema_groups {
                let fields = sch.fields();
                for r in row {
                    for (i, f) in r.fields.iter().enumerate() {
                        if let Some(fv) = f {
                            if let Some(c) = fields.get(i) {
                                if &c.name != "time" {
                                    fname_vals_map
                                        .entry(c.name.clone())
                                        .or_default()
                                        .push((r.ts, fv.clone()))
                                }
                            };
                        }
                    }
                }
            }
        }

        fname_vals_map
    }
}

pub fn dedup_and_sort_row_data(data: &LinkedList<RowData>) -> Vec<RowData> {
    let mut data = data.iter().cloned().collect::<Vec<_>>();
    data.sort_by(|a, b| a.ts.cmp(&b.ts));
    let mut dedup_ts = HashSet::new();
    data.iter().for_each(|row_data| {
        if !dedup_ts.contains(&row_data.ts) {
            dedup_ts.insert(row_data.ts);
        }
    });

    let mut result: Vec<RowData> = Vec::with_capacity(data.len());
    for row_data in data {
        if let Some(existing_row) = result.last_mut() {
            if existing_row.ts == row_data.ts {
                for (index, field) in row_data.fields.iter().enumerate() {
                    if let Some(field) = field {
                        existing_row.fields[index] = Some(field.clone());
                    }
                }
            } else {
                result.push(row_data);
            }
        } else {
            result.push(row_data);
        }
    }
    result
}

#[cfg(test)]
mod test_memcache {
    use std::collections::LinkedList;
    use std::sync::Arc;

    use datafusion::arrow::datatypes::TimeUnit;
    use memory_pool::{GreedyMemoryPool, MemoryPool};
    use models::field_value::FieldVal;
    use models::predicate::domain::TimeRange;
    use models::schema::{ColumnType, TableColumn, TskvTableSchema};
    use models::{SeriesId, ValueType};

    use super::{MemCache, RowData, RowGroup};

    #[test]
    fn test_write_group() {
        let sid: SeriesId = 1;

        let memory_pool: Arc<dyn MemoryPool> = Arc::new(GreedyMemoryPool::new(1024 * 1024 * 1024));
        let mem_cache = MemCache::new(1, 1000, 2, 1, &memory_pool);
        {
            let series_part = &mem_cache.partions[sid as usize].read();
            let series_data = series_part.get(&sid);
            assert!(series_data.is_none());
        }

        #[rustfmt::skip]
            let mut schema_1 = TskvTableSchema::new(
            "test_tenant".to_string(), "test_db".to_string(), "test_table".to_string(),
            vec![
                TableColumn::new_time_column(1, TimeUnit::Nanosecond),
                TableColumn::new_tag_column(2, "tag_col_1".to_string()),
                TableColumn::new_tag_column(3, "tag_col_2".to_string()),
                TableColumn::new(4, "f_col_1".to_string(), ColumnType::Field(ValueType::Float), Default::default()),
            ],
        );
        schema_1.schema_id = 1;
        #[rustfmt::skip]
            let row_group_1 = RowGroup {
            schema: Arc::new(schema_1),
            range: TimeRange::new(1, 3),
            rows: LinkedList::from([
                RowData { ts: 1, fields: vec![Some(FieldVal::Float(1.0))] },
                RowData { ts: 3, fields: vec![Some(FieldVal::Float(3.0))] },
            ]),
            size: 10,
        };
        mem_cache.write_group(sid, 1, row_group_1.clone()).unwrap();
        {
            let series_part = &mem_cache.partions[sid as usize].read();
            let series_data = series_part.get(&sid);
            assert!(series_data.is_some());
            let series_data = series_data.unwrap().read();
            assert_eq!(sid, series_data.series_id);
            assert_eq!(TimeRange::new(1, 3), series_data.range);
            assert_eq!(1, series_data.groups.len());
            assert_eq!(row_group_1, series_data.groups.front().unwrap().clone());
        }

        #[rustfmt::skip]
            let mut schema_2 = TskvTableSchema::new(
            "test_tenant".to_string(), "test_db".to_string(), "test_table".to_string(),
            vec![
                TableColumn::new_time_column(1, TimeUnit::Nanosecond),
                TableColumn::new_tag_column(2, "tag_col_1".to_string()),
                TableColumn::new_tag_column(3, "tag_col_2".to_string()),
                TableColumn::new(4, "f_col_1".to_string(), ColumnType::Field(ValueType::Float), Default::default()),
                TableColumn::new(5, "f_col_2".to_string(), ColumnType::Field(ValueType::Integer), Default::default()),
            ],
        );
        schema_2.schema_id = 2;
        #[rustfmt::skip]
            let row_group_2 = RowGroup {
            schema: Arc::new(schema_2),
            range: TimeRange::new(3, 5),
            rows: LinkedList::from([
                RowData { ts: 3, fields: vec![None, Some(FieldVal::Integer(3))] },
                RowData { ts: 5, fields: vec![Some(FieldVal::Float(5.0)), Some(FieldVal::Integer(5))] }
            ]),
            size: 10,
        };
        mem_cache.write_group(sid, 2, row_group_2.clone()).unwrap();
        {
            let series_part = &mem_cache.partions[sid as usize].read();
            let series_data = series_part.get(&sid);
            assert!(series_data.is_some());
            let series_data = series_data.unwrap().read();
            assert_eq!(sid, series_data.series_id);
            assert_eq!(TimeRange::new(1, 5), series_data.range);
            assert_eq!(2, series_data.groups.len());
            assert_eq!(row_group_2, series_data.groups.back().unwrap().clone());
        }
    }
}
