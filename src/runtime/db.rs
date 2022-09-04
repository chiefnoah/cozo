use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs, thread};

use miette::{miette, bail, ensure, Result, IntoDiagnostic};
use either::{Left, Right};
use itertools::Itertools;
use log::debug;
use serde_json::json;
use smartstring::SmartString;

use cozorocks::{DbBuilder, DbIter, RocksDb};

use crate::data::compare::{rusty_cmp, DB_KEY_PREFIX_LEN};
use crate::data::encode::{
    decode_ea_key, decode_value_from_key, decode_value_from_val, encode_eav_key, largest_key,
    smallest_key, StorageTag,
};
use crate::data::id::{AttrId, EntityId, TxId, Validity};
use crate::data::json::JsonValue;
use crate::data::symb::Symbol;
use crate::data::triple::StoreOp;
use crate::data::tuple::{rusty_scratch_cmp, EncodedTuple, Tuple, SCRATCH_DB_KEY_PREFIX_LEN};
use crate::data::value::{DataValue, LARGEST_UTF_CHAR};
use crate::parse::cozoscript::query::{parse_query_to_json, ScriptType};
use crate::parse::cozoscript::sys::{CompactTarget, SysOp};
use crate::parse::query::ViewOp;
use crate::parse::schema::AttrTxItem;
use crate::query::pull::CurrentPath;
use crate::runtime::transact::SessionTx;
use crate::runtime::view::{ViewRelId, ViewRelMetadata};
use crate::utils::swap_option_result;

struct RunningQueryHandle {
    started_at: Validity,
    poison: Poison,
}

struct RunningQueryCleanup {
    id: u64,
    running_queries: Arc<Mutex<BTreeMap<u64, RunningQueryHandle>>>,
}

impl Drop for RunningQueryCleanup {
    fn drop(&mut self) {
        let mut map = self.running_queries.lock().unwrap();
        if let Some(handle) = map.remove(&self.id) {
            handle.poison.0.store(true, Ordering::Relaxed);
        }
    }
}

pub struct Db {
    db: RocksDb,
    view_db: RocksDb,
    last_attr_id: Arc<AtomicU64>,
    last_ent_id: Arc<AtomicU64>,
    last_tx_id: Arc<AtomicU64>,
    view_store_id: Arc<AtomicU64>,
    n_sessions: Arc<AtomicUsize>,
    queries_count: Arc<AtomicU64>,
    running_queries: Arc<Mutex<BTreeMap<u64, RunningQueryHandle>>>,
    session_id: usize,
}

impl Debug for Db {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Db<session {}, attrs {:?}, entities {:?}, txs {:?}, sessions: {:?}>",
            self.session_id, self.last_tx_id, self.last_ent_id, self.last_tx_id, self.n_sessions
        )
    }
}

impl Db {
    pub fn build(builder: DbBuilder<'_>) -> Result<Self> {
        let path = builder.opts.db_path;
        fs::create_dir_all(path).into_diagnostic()?;
        let path_buf = PathBuf::from(path);
        let mut triple_path = path_buf.clone();
        triple_path.push("triple");
        let db_builder = builder
            .use_capped_prefix_extractor(true, DB_KEY_PREFIX_LEN)
            .optimistic(false)
            .use_custom_comparator("cozo_rusty_cmp", rusty_cmp, false)
            .path(triple_path.to_str().unwrap());
        let mut rel_path = path_buf;
        rel_path.push("rel");
        let view_db_builder = db_builder
            .clone()
            .optimistic(false)
            .path(rel_path.to_str().unwrap())
            .use_capped_prefix_extractor(true, SCRATCH_DB_KEY_PREFIX_LEN)
            .use_custom_comparator("cozo_rusty_scratch_cmp", rusty_scratch_cmp, false);

        let db = db_builder.build().into_diagnostic()?;
        let view_db = view_db_builder.build().into_diagnostic()?;

        let ret = Self {
            db,
            view_db,
            last_attr_id: Arc::new(Default::default()),
            last_ent_id: Arc::new(Default::default()),
            last_tx_id: Arc::new(Default::default()),
            view_store_id: Arc::new(Default::default()),
            n_sessions: Arc::new(Default::default()),
            queries_count: Arc::new(Default::default()),
            running_queries: Arc::new(Mutex::new(Default::default())),
            session_id: Default::default(),
        };
        ret.load_last_ids()?;
        Ok(ret)
    }

    pub fn compact_main(&self) -> Result<()> {
        let l = smallest_key();
        let u = largest_key();
        self.db.range_compact(&l, &u).into_diagnostic()?;
        Ok(())
    }

    pub fn compact_view(&self) -> Result<()> {
        let l = Tuple::default().encode_as_key(ViewRelId(0));
        let u = Tuple(vec![DataValue::Bot]).encode_as_key(ViewRelId(u64::MAX));
        self.db.range_compact(&l, &u).into_diagnostic()?;
        Ok(())
    }

    pub fn new_session(&self) -> Result<Self> {
        let old_count = self.n_sessions.fetch_add(1, Ordering::AcqRel);

        Ok(Self {
            db: self.db.clone(),
            view_db: self.view_db.clone(),
            last_attr_id: self.last_attr_id.clone(),
            last_ent_id: self.last_ent_id.clone(),
            last_tx_id: self.last_tx_id.clone(),
            view_store_id: self.view_store_id.clone(),
            n_sessions: self.n_sessions.clone(),
            queries_count: self.queries_count.clone(),
            running_queries: self.running_queries.clone(),
            session_id: old_count + 1,
        })
    }

    fn load_last_ids(&self) -> Result<()> {
        let mut tx = self.transact()?;
        self.last_tx_id
            .store(tx.load_last_tx_id()?.0, Ordering::Release);
        self.last_attr_id
            .store(tx.load_last_attr_id()?.0, Ordering::Release);
        self.last_ent_id
            .store(tx.load_last_entity_id()?.0, Ordering::Release);
        self.view_store_id
            .store(tx.load_last_view_store_id()?.0, Ordering::Release);
        Ok(())
    }
    pub fn transact(&self) -> Result<SessionTx> {
        let ret = SessionTx {
            tx: self.db.transact().set_snapshot(true).start(),
            view_db: self.view_db.clone(),
            mem_store_id: Default::default(),
            view_store_id: self.view_store_id.clone(),
            w_tx_id: None,
            last_attr_id: self.last_attr_id.clone(),
            last_ent_id: self.last_ent_id.clone(),
            last_tx_id: self.last_tx_id.clone(),
            attr_by_id_cache: Default::default(),
            attr_by_kw_cache: Default::default(),
            temp_entity_to_perm: Default::default(),
            eid_by_attr_val_cache: Default::default(),
            touched_eids: Default::default(),
        };
        Ok(ret)
    }
    pub fn transact_write(&self) -> Result<SessionTx> {
        let last_tx_id = self.last_tx_id.fetch_add(1, Ordering::AcqRel);
        let cur_tx_id = TxId(last_tx_id + 1);

        let ret = SessionTx {
            tx: self.db.transact().set_snapshot(true).start(),
            view_db: self.view_db.clone(),
            mem_store_id: Default::default(),
            view_store_id: self.view_store_id.clone(),
            w_tx_id: Some(cur_tx_id),
            last_attr_id: self.last_attr_id.clone(),
            last_ent_id: self.last_ent_id.clone(),
            last_tx_id: self.last_tx_id.clone(),
            attr_by_id_cache: Default::default(),
            attr_by_kw_cache: Default::default(),
            temp_entity_to_perm: Default::default(),
            eid_by_attr_val_cache: Default::default(),
            touched_eids: Default::default(),
        };
        Ok(ret)
    }
    pub fn total_iter(&self) -> DbIter {
        let mut it = self.db.transact().start().iterator().start();
        it.seek_to_start();
        it
    }
    pub fn pull(&self, eid: &JsonValue, payload: &JsonValue, vld: &JsonValue) -> Result<JsonValue> {
        let eid = EntityId::try_from(eid)?;
        let vld = match vld {
            JsonValue::Null => Validity::current(),
            v => Validity::try_from(v)?,
        };
        let mut tx = self.transact()?;
        let specs = tx.parse_pull(payload, 0)?;
        let mut collected = Default::default();
        let mut recursive_seen = Default::default();
        for (idx, spec) in specs.iter().enumerate() {
            tx.pull(
                eid,
                vld,
                spec,
                0,
                &specs,
                CurrentPath::new(idx)?,
                &mut collected,
                &mut recursive_seen,
            )?;
        }
        Ok(JsonValue::Object(collected))
    }
    // pub fn run_tx_triples(&self, payload: &str) -> Result<JsonValue> {
    //     let payload = parse_tx_to_json(payload)?;
    //     self.transact_triples(&payload)
    // }
    pub fn transact_triples(&self, payload: &JsonValue) -> Result<JsonValue> {
        let mut tx = self.transact_write()?;
        let (payloads, comment) = tx.parse_tx_requests(payload)?;
        let res: JsonValue = tx
            .tx_triples(payloads)?
            .iter()
            .map(|(eid, size)| json!([eid.0, size]))
            .collect();
        let tx_id = tx.get_write_tx_id()?;
        tx.commit_tx(&comment, false)?;
        Ok(json!({
            "tx_id": tx_id,
            "results": res
        }))
    }
    // pub fn run_tx_attributes(&self, payload: &str) -> Result<JsonValue> {
    //     let payload = parse_schema_to_json(payload)?;
    //     self.transact_attributes(&payload)
    // }
    pub fn transact_attributes(&self, payload: &JsonValue) -> Result<JsonValue> {
        let (attrs, comment) = AttrTxItem::parse_request(payload)?;
        let mut tx = self.transact_write()?;
        let res: JsonValue = tx
            .tx_attrs(attrs)?
            .iter()
            .map(|(op, aid)| json!([aid.0, op.to_string()]))
            .collect();
        let tx_id = tx.get_write_tx_id()?;
        tx.commit_tx(&comment, false)?;
        Ok(json!({
            "tx_id": tx_id,
            "results": res
        }))
    }
    pub fn current_schema(&self) -> Result<JsonValue> {
        let mut tx = self.transact()?;
        let rows: Vec<_> = tx
            .all_attrs()
            .map_ok(|v| {
                vec![
                    json!(v.id.0),
                    json!(v.name),
                    json!(v.val_type.to_string()),
                    json!(v.cardinality.to_string()),
                    json!(v.indexing.to_string()),
                    json!(v.with_history),
                ]
            })
            .try_collect()?;
        Ok(
            json!({"rows": rows, "headers": ["id", "name", "type", "cardinality", "index", "history"]}),
        )
    }
    pub fn entities_at(&self, vld: &JsonValue) -> Result<JsonValue> {
        let vld = match vld {
            JsonValue::Null => Validity::current(),
            v => Validity::try_from(v)?,
        };
        let tx = self.transact()?;
        let mut current = encode_eav_key(
            EntityId::MIN_PERM,
            AttrId::MIN_PERM,
            &DataValue::Null,
            Validity::MAX,
        );
        let upper_bound = encode_eav_key(
            EntityId::MAX_PERM,
            AttrId::MAX_PERM,
            &DataValue::Bot,
            Validity::MIN,
        );
        let mut it = tx
            .tx
            .iterator()
            .upper_bound(&upper_bound)
            .total_order_seek(true)
            .start();
        let mut collected: BTreeMap<EntityId, JsonValue> = BTreeMap::default();
        it.seek(&current);
        while let Some((k_slice, v_slice)) = it.pair().into_diagnostic()? {
            debug_assert_eq!(
                StorageTag::try_from(k_slice[0])?,
                StorageTag::TripleEntityAttrValue
            );
            let (e_found, a_found, vld_found) = decode_ea_key(k_slice)?;
            current.copy_from_slice(k_slice);

            if vld_found > vld {
                current.encoded_entity_amend_validity(vld);
                it.seek(&current);
                continue;
            }
            let op = StoreOp::try_from(v_slice[0])?;
            if op.is_retract() {
                current.encoded_entity_amend_validity_to_inf_past();
                it.seek(&current);
                continue;
            }
            let attr = tx.attr_by_id(a_found)?;
            if attr.is_none() {
                current.encoded_entity_amend_validity_to_inf_past();
                it.seek(&current);
                continue;
            }
            let attr = attr.unwrap();
            let value = if attr.cardinality.is_one() {
                decode_value_from_val(v_slice)?
            } else {
                decode_value_from_key(k_slice)?
            };
            let json_for_entry = collected.entry(e_found).or_insert_with(|| json!({}));
            let map_for_entry = json_for_entry.as_object_mut().unwrap();
            map_for_entry.insert("_id".to_string(), e_found.0.into());
            if attr.cardinality.is_many() {
                let arr = map_for_entry
                    .entry(attr.name.to_string())
                    .or_insert_with(|| json!([]));
                let arr = arr.as_array_mut().unwrap();
                arr.push(value.into());
            } else {
                map_for_entry.insert(attr.name.to_string(), value.into());
            }
            current.encoded_entity_amend_validity_to_inf_past();
            it.seek(&current);
        }
        let collected = collected.into_iter().map(|(_, v)| v).collect_vec();
        Ok(json!(collected))
    }
    pub fn run_script(&self, payload: &str) -> Result<JsonValue> {
        let (script_type, payload) = parse_query_to_json(payload)?;
        match script_type {
            ScriptType::Query => self.run_query(&payload),
            ScriptType::Schema => self.transact_attributes(&payload),
            ScriptType::Tx => self.transact_triples(&payload),
            ScriptType::Sys => self.run_sys_op(payload),
        }
    }
    pub fn convert_to_json_query(&self, payload: &str) -> Result<JsonValue> {
        let (script_type, payload) = parse_query_to_json(payload)?;
        let key = match script_type {
            ScriptType::Query => "query",
            ScriptType::Schema => "schema",
            ScriptType::Tx => "tx",
            ScriptType::Sys => "sys",
        };
        Ok(json!({ key: payload }))
    }
    pub fn run_json_query(&self, payload: &JsonValue) -> Result<JsonValue> {
        let (k, v) = payload
            .as_object()
            .ok_or_else(|| miette!("json query must be an object"))?
            .iter()
            .next()
            .ok_or_else(|| miette!("json query must be an object with keys"))?;
        match k as &str {
            "query" => self.run_query(v),
            "schema" => self.transact_attributes(v),
            "tx" => self.transact_triples(v),
            "sys" => self.run_sys_op(v.clone()),
            v => bail!("unexpected key in json query: {}", v),
        }
    }
    pub fn run_sys_op(&self, payload: JsonValue) -> Result<JsonValue> {
        let op: SysOp = serde_json::from_value(payload).into_diagnostic()?;
        match op {
            SysOp::Compact(opts) => {
                for opt in opts {
                    match opt {
                        CompactTarget::Triples => {
                            self.compact_main()?;
                        }
                        CompactTarget::Relations => {
                            self.compact_view()?;
                        }
                    }
                }
                Ok(json!({"status": "OK"}))
            }
            SysOp::ListSchema => self.current_schema(),
            SysOp::ListRelations => self.list_relations(),
            SysOp::RemoveRelations(rs) => {
                for r in rs.iter() {
                    self.remove_view(&r.0)?;
                }
                Ok(json!({"status": "OK"}))
            }
            SysOp::ListRunning => self.list_running(),
            SysOp::KillRunning(id) => {
                let queries = self.running_queries.lock().unwrap();
                Ok(match queries.get(&id) {
                    None => {
                        json!({"status": "NOT_FOUND"})
                    }
                    Some(handle) => {
                        handle.poison.0.store(true, Ordering::Relaxed);
                        json!({"status": "KILLING"})
                    }
                })
            }
        }
    }
    pub fn run_query(&self, payload: &JsonValue) -> Result<JsonValue> {
        let mut tx = self.transact()?;
        let input_program = tx.parse_query(payload, &Default::default())?;
        if let Some((meta, op)) = &input_program.out_opts.as_view {
            if *op == ViewOp::Create {
                ensure!(
                    !tx.view_exists(&meta.name)?,
                    "view '{}' exists but is required not to be",
                    meta.name
                )
            } else if *op != ViewOp::Rederive {
                ensure!(
                    tx.view_exists(&meta.name)?,
                    "view '{}' does not exist but is required to be",
                    meta.name
                )
            }
        };
        let program = input_program
            .to_normalized_program(&tx)?
            .stratify()?
            .magic_sets_rewrite(&tx)?;
        debug!("{:#?}", program);
        let (compiled, stores) =
            tx.stratified_magic_compile(&program, &input_program.const_rules)?;

        let poison = Poison::default();
        if let Some(secs) = input_program.out_opts.timeout {
            poison.set_timeout(secs);
        }
        let id = self.queries_count.fetch_add(1, Ordering::AcqRel);
        let handle = RunningQueryHandle {
            started_at: Validity::current(),
            poison: poison.clone(),
        };
        self.running_queries.lock().unwrap().insert(id, handle);
        let _guard = RunningQueryCleanup {
            id,
            running_queries: self.running_queries.clone(),
        };

        let result = tx.stratified_magic_evaluate(
            &compiled,
            &stores,
            if input_program.out_opts.sorters.is_empty() {
                input_program.out_opts.num_to_take()
            } else {
                None
            },
            poison,
        )?;
        let headers = match input_program.get_entry_head() {
            Err(_) => JsonValue::Null,
            Ok(headers) => headers.iter().map(|v| json!(v.0)).collect(),
        };
        if !input_program.out_opts.sorters.is_empty() {
            let entry_head = input_program.get_entry_head()?.to_vec();
            let sorted_result = tx.sort_and_collect(result, &input_program.out_opts.sorters, &entry_head)?;
            let sorted_iter = if let Some(offset) = input_program.out_opts.offset {
                Left(sorted_result.scan_sorted().skip(offset))
            } else {
                Right(sorted_result.scan_sorted())
            };
            let sorted_iter = if let Some(limit) = input_program.out_opts.limit {
                Left(sorted_iter.take(limit))
            } else {
                Right(sorted_iter)
            };
            if let Some((meta, view_op)) = input_program.out_opts.as_view {
                tx.execute_view(sorted_iter, view_op, &meta)?;
                Ok(json!({"view": "OK"}))
            } else {
                let ret: Vec<_> = tx
                    .run_pull_on_query_results(sorted_iter, input_program.out_opts)?
                    .try_collect()?;
                Ok(json!({ "rows": ret, "headers": headers }))
            }
        } else {
            if let Some((meta, view_op)) = input_program.out_opts.as_view {
                tx.execute_view(result.scan_all(), view_op, &meta)?;
                Ok(json!({"view": "OK"}))
            } else {
                let ret: Vec<_> = tx
                    .run_pull_on_query_results(result.scan_all(), input_program.out_opts)?
                    .try_collect()?;
                Ok(json!({ "rows": ret, "headers": headers }))
            }
        }
    }
    pub fn remove_view(&self, name: &str) -> Result<()> {
        let name = Symbol::from(name);
        let tx = self.transact()?;
        tx.destroy_view_rel(&name)
    }
    pub fn list_running(&self) -> Result<JsonValue> {
        let res = self
            .running_queries
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| json!([k, format!("{:?}", v.started_at)]))
            .collect_vec();
        Ok(json!({"rows": res, "headers": ["?id", "?started_at"]}))
    }
    pub fn put_meta_kv(&self, k: &[&str], v: &[u8]) -> Result<()> {
        let mut ks = vec![DataValue::Guard];
        for el in k {
            ks.push(DataValue::Str(SmartString::from(*el)));
        }
        let key = Tuple(ks).encode_as_key(ViewRelId::SYSTEM);
        let mut vtx = self.view_db.transact().start();
        vtx.put(&key, v).into_diagnostic()?;
        vtx.commit().into_diagnostic()?;
        Ok(())
    }
    pub fn remove_meta_kv(&self, k: &[&str]) -> Result<()> {
        let mut ks = vec![DataValue::Guard];
        for el in k {
            ks.push(DataValue::Str(SmartString::from(*el)));
        }
        let key = Tuple(ks).encode_as_key(ViewRelId::SYSTEM);
        let mut vtx = self.view_db.transact().start();
        vtx.del(&key).into_diagnostic()?;
        vtx.commit().into_diagnostic()?;
        Ok(())
    }
    pub fn get_meta_kv(&self, k: &[&str]) -> Result<Option<Vec<u8>>> {
        let mut ks = vec![DataValue::Guard];
        for el in k {
            ks.push(DataValue::Str(SmartString::from(*el)));
        }
        let key = Tuple(ks).encode_as_key(ViewRelId::SYSTEM);
        let vtx = self.view_db.transact().start();
        Ok(match vtx.get(&key, false).into_diagnostic()? {
            None => None,
            Some(slice) => Some(slice.to_vec()),
        })
    }
    pub fn meta_range_scan(
        &self,
        prefix: &[&str],
    ) -> impl Iterator<Item = Result<(Vec<String>, Vec<u8>)>> {
        let mut lower_bound = Tuple(vec![DataValue::Guard]);
        for p in prefix {
            lower_bound.0.push(DataValue::Str(SmartString::from(*p)));
        }
        let upper_bound = Tuple(vec![DataValue::Bot]);
        let mut it = self
            .view_db
            .transact()
            .start()
            .iterator()
            .upper_bound(&upper_bound.encode_as_key(ViewRelId::SYSTEM))
            .start();
        it.seek(&lower_bound.encode_as_key(ViewRelId::SYSTEM));

        struct CustomIter {
            it: DbIter,
            started: bool,
        }

        impl CustomIter {
            fn next_inner(&mut self) -> Result<Option<(Vec<String>, Vec<u8>)>> {
                if self.started {
                    self.it.next()
                } else {
                    self.started = true;
                }
                match self.it.pair().into_diagnostic()? {
                    None => Ok(None),
                    Some((k_slice, v_slice)) => {
                        let encoded = EncodedTuple(k_slice).decode()?;
                        let ks: Vec<_> = encoded
                            .0
                            .into_iter()
                            .skip(1)
                            .map(|v| {
                                v.get_string()
                                    .map(|s| s.to_string())
                                    .ok_or_else(|| miette!("bad key in meta store"))
                            })
                            .try_collect()?;
                        Ok(Some((ks, v_slice.to_vec())))
                    }
                }
            }
        }

        impl Iterator for CustomIter {
            type Item = Result<(Vec<String>, Vec<u8>)>;

            fn next(&mut self) -> Option<Self::Item> {
                swap_option_result(self.next_inner())
            }
        }

        CustomIter { it, started: false }
    }
    pub fn list_relations(&self) -> Result<JsonValue> {
        let lower =
            Tuple(vec![DataValue::Str(SmartString::from(""))]).encode_as_key(ViewRelId::SYSTEM);
        let upper = Tuple(vec![DataValue::Str(SmartString::from(String::from(
            LARGEST_UTF_CHAR,
        )))])
        .encode_as_key(ViewRelId::SYSTEM);
        let mut it = self
            .view_db
            .transact()
            .start()
            .iterator()
            .upper_bound(&upper)
            .start();
        it.seek(&lower);
        let mut collected = vec![];
        while let Some(v_slice) = it.val().into_diagnostic()? {
            let meta: ViewRelMetadata = rmp_serde::from_slice(v_slice).into_diagnostic()?;
            let name = meta.name.0;
            let arity = meta.arity;
            collected.push(json!([name, arity]));
            it.next();
        }
        Ok(json!({"rows": collected, "headers": ["name", "arity"]}))
    }
}

#[derive(Clone, Default)]
pub(crate) struct Poison(pub(crate) Arc<AtomicBool>);

impl Poison {
    #[inline(always)]
    pub(crate) fn check(&self) -> Result<()> {
        if self.0.load(Ordering::Relaxed) {
            bail!("killed")
        }
        Ok(())
    }
    pub(crate) fn set_timeout(&self, secs: u64) {
        let pill = self.0.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(secs));
            pill.store(true, Ordering::Relaxed);
        });
    }
}
