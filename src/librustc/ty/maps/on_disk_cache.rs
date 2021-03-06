// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use dep_graph::{DepNodeIndex, SerializedDepNodeIndex};
use errors::Diagnostic;
use hir;
use hir::def_id::{CrateNum, DefIndex, DefId, LocalDefId,
                  RESERVED_FOR_INCR_COMP_CACHE, LOCAL_CRATE};
use hir::map::definitions::DefPathHash;
use ich::{CachingCodemapView, Fingerprint};
use mir::{self, interpret};
use rustc_data_structures::fx::FxHashMap;
use rustc_data_structures::sync::{Lrc, Lock, HashMapExt, Once};
use rustc_data_structures::indexed_vec::{IndexVec, Idx};
use rustc_serialize::{Decodable, Decoder, Encodable, Encoder, opaque,
                      SpecializedDecoder, SpecializedEncoder,
                      UseSpecializedDecodable, UseSpecializedEncodable};
use session::{CrateDisambiguator, Session};
use std::cell::RefCell;
use std::mem;
use syntax::ast::NodeId;
use syntax::codemap::{CodeMap, StableFilemapId};
use syntax_pos::{BytePos, Span, DUMMY_SP, FileMap};
use syntax_pos::hygiene::{Mark, SyntaxContext, ExpnInfo};
use ty;
use ty::maps::job::QueryResult;
use ty::codec::{self as ty_codec, TyDecoder, TyEncoder};
use ty::context::TyCtxt;
use util::common::time;

const TAG_FILE_FOOTER: u128 = 0xC0FFEE_C0FFEE_C0FFEE_C0FFEE_C0FFEE;

const TAG_CLEAR_CROSS_CRATE_CLEAR: u8 = 0;
const TAG_CLEAR_CROSS_CRATE_SET: u8 = 1;

const TAG_NO_EXPANSION_INFO: u8 = 0;
const TAG_EXPANSION_INFO_SHORTHAND: u8 = 1;
const TAG_EXPANSION_INFO_INLINE: u8 = 2;

const TAG_VALID_SPAN: u8 = 0;
const TAG_INVALID_SPAN: u8 = 1;

/// `OnDiskCache` provides an interface to incr. comp. data cached from the
/// previous compilation session. This data will eventually include the results
/// of a few selected queries (like `typeck_tables_of` and `mir_optimized`) and
/// any diagnostics that have been emitted during a query.
pub struct OnDiskCache<'sess> {

    // The complete cache data in serialized form.
    serialized_data: Vec<u8>,

    // This field collects all Diagnostics emitted during the current
    // compilation session.
    current_diagnostics: Lock<FxHashMap<DepNodeIndex, Vec<Diagnostic>>>,

    prev_cnums: Vec<(u32, String, CrateDisambiguator)>,
    cnum_map: Once<IndexVec<CrateNum, Option<CrateNum>>>,

    codemap: &'sess CodeMap,
    file_index_to_stable_id: FxHashMap<FileMapIndex, StableFilemapId>,

    // These two fields caches that are populated lazily during decoding.
    file_index_to_file: Lock<FxHashMap<FileMapIndex, Lrc<FileMap>>>,
    synthetic_expansion_infos: Lock<FxHashMap<AbsoluteBytePos, SyntaxContext>>,

    // A map from dep-node to the position of the cached query result in
    // `serialized_data`.
    query_result_index: FxHashMap<SerializedDepNodeIndex, AbsoluteBytePos>,

    // A map from dep-node to the position of any associated diagnostics in
    // `serialized_data`.
    prev_diagnostics_index: FxHashMap<SerializedDepNodeIndex, AbsoluteBytePos>,

    // Alloc indices to memory location map
    prev_interpret_alloc_index: Vec<AbsoluteBytePos>,

    /// Deserialization: A cache to ensure we don't read allocations twice
    interpret_alloc_cache: RefCell<FxHashMap<usize, interpret::AllocId>>,
}

// This type is used only for (de-)serialization.
#[derive(RustcEncodable, RustcDecodable)]
struct Footer {
    file_index_to_stable_id: FxHashMap<FileMapIndex, StableFilemapId>,
    prev_cnums: Vec<(u32, String, CrateDisambiguator)>,
    query_result_index: EncodedQueryResultIndex,
    diagnostics_index: EncodedQueryResultIndex,
    // the location of all allocations
    interpret_alloc_index: Vec<AbsoluteBytePos>,
}

type EncodedQueryResultIndex = Vec<(SerializedDepNodeIndex, AbsoluteBytePos)>;
type EncodedDiagnosticsIndex = Vec<(SerializedDepNodeIndex, AbsoluteBytePos)>;
type EncodedDiagnostics = Vec<Diagnostic>;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, RustcEncodable, RustcDecodable)]
struct FileMapIndex(u32);

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq, RustcEncodable, RustcDecodable)]
struct AbsoluteBytePos(u32);

impl AbsoluteBytePos {
    fn new(pos: usize) -> AbsoluteBytePos {
        debug_assert!(pos <= ::std::u32::MAX as usize);
        AbsoluteBytePos(pos as u32)
    }

    fn to_usize(self) -> usize {
        self.0 as usize
    }
}

impl<'sess> OnDiskCache<'sess> {
    /// Create a new OnDiskCache instance from the serialized data in `data`.
    pub fn new(sess: &'sess Session, data: Vec<u8>, start_pos: usize) -> OnDiskCache<'sess> {
        debug_assert!(sess.opts.incremental.is_some());

        // Wrapping in a scope so we can borrow `data`
        let footer: Footer = {
            let mut decoder = opaque::Decoder::new(&data[..], start_pos);

            // Decode the *position* of the footer which can be found in the
            // last 8 bytes of the file.
            decoder.set_position(data.len() - IntEncodedWithFixedSize::ENCODED_SIZE);
            let query_result_index_pos = IntEncodedWithFixedSize::decode(&mut decoder)
                .expect("Error while trying to decode query result index position.")
                .0 as usize;

            // Decoder the file footer which contains all the lookup tables, etc.
            decoder.set_position(query_result_index_pos);
            decode_tagged(&mut decoder, TAG_FILE_FOOTER)
                .expect("Error while trying to decode query result index position.")
        };

        OnDiskCache {
            serialized_data: data,
            file_index_to_stable_id: footer.file_index_to_stable_id,
            file_index_to_file: Lock::new(FxHashMap()),
            prev_cnums: footer.prev_cnums,
            cnum_map: Once::new(),
            codemap: sess.codemap(),
            current_diagnostics: Lock::new(FxHashMap()),
            query_result_index: footer.query_result_index.into_iter().collect(),
            prev_diagnostics_index: footer.diagnostics_index.into_iter().collect(),
            synthetic_expansion_infos: Lock::new(FxHashMap()),
            prev_interpret_alloc_index: footer.interpret_alloc_index,
            interpret_alloc_cache: RefCell::new(FxHashMap::default()),
        }
    }

    pub fn new_empty(codemap: &'sess CodeMap) -> OnDiskCache<'sess> {
        OnDiskCache {
            serialized_data: Vec::new(),
            file_index_to_stable_id: FxHashMap(),
            file_index_to_file: Lock::new(FxHashMap()),
            prev_cnums: vec![],
            cnum_map: Once::new(),
            codemap,
            current_diagnostics: Lock::new(FxHashMap()),
            query_result_index: FxHashMap(),
            prev_diagnostics_index: FxHashMap(),
            synthetic_expansion_infos: Lock::new(FxHashMap()),
            prev_interpret_alloc_index: Vec::new(),
            interpret_alloc_cache: RefCell::new(FxHashMap::default()),
        }
    }

    pub fn serialize<'a, 'tcx, E>(&self,
                                  tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                  encoder: &mut E)
                                  -> Result<(), E::Error>
        where E: ty_codec::TyEncoder
     {
        // Serializing the DepGraph should not modify it:
        tcx.dep_graph.with_ignore(|| {
            // Allocate FileMapIndices
            let (file_to_file_index, file_index_to_stable_id) = {
                let mut file_to_file_index = FxHashMap();
                let mut file_index_to_stable_id = FxHashMap();

                for (index, file) in tcx.sess.codemap().files().iter().enumerate() {
                    let index = FileMapIndex(index as u32);
                    let file_ptr: *const FileMap = &**file as *const _;
                    file_to_file_index.insert(file_ptr, index);
                    file_index_to_stable_id.insert(index, StableFilemapId::new(&file));
                }

                (file_to_file_index, file_index_to_stable_id)
            };

            let mut encoder = CacheEncoder {
                tcx,
                encoder,
                type_shorthands: FxHashMap(),
                predicate_shorthands: FxHashMap(),
                expn_info_shorthands: FxHashMap(),
                interpret_allocs: FxHashMap(),
                interpret_allocs_inverse: Vec::new(),
                codemap: CachingCodemapView::new(tcx.sess.codemap()),
                file_to_file_index,
            };

            // Load everything into memory so we can write it out to the on-disk
            // cache. The vast majority of cacheable query results should already
            // be in memory, so this should be a cheap operation.
            tcx.dep_graph.exec_cache_promotions(tcx);

            // Encode query results
            let mut query_result_index = EncodedQueryResultIndex::new();

            time(tcx.sess, "encode query results", || {
                use ty::maps::queries::*;
                let enc = &mut encoder;
                let qri = &mut query_result_index;

                encode_query_results::<type_of, _>(tcx, enc, qri)?;
                encode_query_results::<generics_of, _>(tcx, enc, qri)?;
                encode_query_results::<predicates_of, _>(tcx, enc, qri)?;
                encode_query_results::<used_trait_imports, _>(tcx, enc, qri)?;
                encode_query_results::<typeck_tables_of, _>(tcx, enc, qri)?;
                encode_query_results::<trans_fulfill_obligation, _>(tcx, enc, qri)?;
                encode_query_results::<optimized_mir, _>(tcx, enc, qri)?;
                encode_query_results::<unsafety_check_result, _>(tcx, enc, qri)?;
                encode_query_results::<borrowck, _>(tcx, enc, qri)?;
                encode_query_results::<mir_borrowck, _>(tcx, enc, qri)?;
                encode_query_results::<mir_const_qualif, _>(tcx, enc, qri)?;
                encode_query_results::<def_symbol_name, _>(tcx, enc, qri)?;
                encode_query_results::<const_is_rvalue_promotable_to_static, _>(tcx, enc, qri)?;
                encode_query_results::<symbol_name, _>(tcx, enc, qri)?;
                encode_query_results::<check_match, _>(tcx, enc, qri)?;
                encode_query_results::<trans_fn_attrs, _>(tcx, enc, qri)?;
                encode_query_results::<specialization_graph_of, _>(tcx, enc, qri)?;

                // const eval is special, it only encodes successfully evaluated constants
                use ty::maps::plumbing::GetCacheInternal;
                for (key, entry) in const_eval::get_cache_internal(tcx).map.iter() {
                    use ty::maps::config::QueryDescription;
                    if const_eval::cache_on_disk(key.clone()) {
                        let entry = match *entry {
                            QueryResult::Complete(ref v) => v,
                            _ => panic!("incomplete query"),
                        };
                        if let Ok(ref value) = entry.value {
                            let dep_node = SerializedDepNodeIndex::new(entry.index.index());

                            // Record position of the cache entry
                            qri.push((dep_node, AbsoluteBytePos::new(enc.position())));

                            // Encode the type check tables with the SerializedDepNodeIndex
                            // as tag.
                            enc.encode_tagged(dep_node, value)?;
                        }
                    }
                }

                Ok(())
            })?;

            // Encode diagnostics
            let diagnostics_index = {
                let mut diagnostics_index = EncodedDiagnosticsIndex::new();

                for (dep_node_index, diagnostics) in self.current_diagnostics
                                                        .borrow()
                                                        .iter() {
                    let pos = AbsoluteBytePos::new(encoder.position());
                    // Let's make sure we get the expected type here:
                    let diagnostics: &EncodedDiagnostics = diagnostics;
                    let dep_node_index =
                        SerializedDepNodeIndex::new(dep_node_index.index());
                    encoder.encode_tagged(dep_node_index, diagnostics)?;
                    diagnostics_index.push((dep_node_index, pos));
                }

                diagnostics_index
            };

            let interpret_alloc_index = {
                let mut interpret_alloc_index = Vec::new();
                let mut n = 0;
                loop {
                    let new_n = encoder.interpret_allocs_inverse.len();
                    // if we have found new ids, serialize those, too
                    if n == new_n {
                        // otherwise, abort
                        break;
                    }
                    for idx in n..new_n {
                        let id = encoder.interpret_allocs_inverse[idx];
                        let pos = AbsoluteBytePos::new(encoder.position());
                        interpret_alloc_index.push(pos);
                        interpret::specialized_encode_alloc_id(
                            &mut encoder,
                            tcx,
                            id,
                        )?;
                    }
                    n = new_n;
                }
                interpret_alloc_index
            };

            let sorted_cnums = sorted_cnums_including_local_crate(tcx);
            let prev_cnums: Vec<_> = sorted_cnums.iter().map(|&cnum| {
                let crate_name = tcx.original_crate_name(cnum).as_str().to_string();
                let crate_disambiguator = tcx.crate_disambiguator(cnum);
                (cnum.as_u32(), crate_name, crate_disambiguator)
            }).collect();

            // Encode the file footer
            let footer_pos = encoder.position() as u64;
            encoder.encode_tagged(TAG_FILE_FOOTER, &Footer {
                file_index_to_stable_id,
                prev_cnums,
                query_result_index,
                diagnostics_index,
                interpret_alloc_index,
            })?;

            // Encode the position of the footer as the last 8 bytes of the
            // file so we know where to look for it.
            IntEncodedWithFixedSize(footer_pos).encode(encoder.encoder)?;

            // DO NOT WRITE ANYTHING TO THE ENCODER AFTER THIS POINT! The address
            // of the footer must be the last thing in the data stream.

            return Ok(());

            fn sorted_cnums_including_local_crate(tcx: TyCtxt) -> Vec<CrateNum> {
                let mut cnums = vec![LOCAL_CRATE];
                cnums.extend_from_slice(&tcx.crates()[..]);
                cnums.sort_unstable();
                // Just to be sure...
                cnums.dedup();
                cnums
            }
        })
    }

    /// Load a diagnostic emitted during the previous compilation session.
    pub fn load_diagnostics<'a, 'tcx>(&self,
                                      tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                      dep_node_index: SerializedDepNodeIndex)
                                      -> Vec<Diagnostic> {
        let diagnostics: Option<EncodedDiagnostics> = self.load_indexed(
            tcx,
            dep_node_index,
            &self.prev_diagnostics_index,
            "diagnostics");

        diagnostics.unwrap_or(Vec::new())
    }

    /// Store a diagnostic emitted during the current compilation session.
    /// Anything stored like this will be available via `load_diagnostics` in
    /// the next compilation session.
    pub fn store_diagnostics(&self,
                             dep_node_index: DepNodeIndex,
                             diagnostics: Vec<Diagnostic>) {
        let mut current_diagnostics = self.current_diagnostics.borrow_mut();
        let prev = current_diagnostics.insert(dep_node_index, diagnostics);
        debug_assert!(prev.is_none());
    }

    /// Returns the cached query result if there is something in the cache for
    /// the given SerializedDepNodeIndex. Otherwise returns None.
    pub fn try_load_query_result<'tcx, T>(&self,
                                          tcx: TyCtxt<'_, 'tcx, 'tcx>,
                                          dep_node_index: SerializedDepNodeIndex)
                                          -> Option<T>
        where T: Decodable
    {
        self.load_indexed(tcx,
                          dep_node_index,
                          &self.query_result_index,
                          "query result")
    }

    /// Store a diagnostic emitted during computation of an anonymous query.
    /// Since many anonymous queries can share the same `DepNode`, we aggregate
    /// them -- as opposed to regular queries where we assume that there is a
    /// 1:1 relationship between query-key and `DepNode`.
    pub fn store_diagnostics_for_anon_node(&self,
                                           dep_node_index: DepNodeIndex,
                                           mut diagnostics: Vec<Diagnostic>) {
        let mut current_diagnostics = self.current_diagnostics.borrow_mut();

        let x = current_diagnostics.entry(dep_node_index).or_insert_with(|| {
            mem::replace(&mut diagnostics, Vec::new())
        });

        x.extend(diagnostics.into_iter());
    }

    fn load_indexed<'tcx, T>(&self,
                             tcx: TyCtxt<'_, 'tcx, 'tcx>,
                             dep_node_index: SerializedDepNodeIndex,
                             index: &FxHashMap<SerializedDepNodeIndex,
                                               AbsoluteBytePos>,
                             debug_tag: &'static str)
                             -> Option<T>
        where T: Decodable
    {
        let pos = if let Some(&pos) = index.get(&dep_node_index) {
            pos
        } else {
            return None
        };

        // Initialize the cnum_map using the value from the thread which finishes the closure first
        self.cnum_map.init_nonlocking_same(|| {
            Self::compute_cnum_map(tcx, &self.prev_cnums[..])
        });

        let mut decoder = CacheDecoder {
            tcx,
            opaque: opaque::Decoder::new(&self.serialized_data[..], pos.to_usize()),
            codemap: self.codemap,
            cnum_map: self.cnum_map.get(),
            file_index_to_file: &self.file_index_to_file,
            file_index_to_stable_id: &self.file_index_to_stable_id,
            synthetic_expansion_infos: &self.synthetic_expansion_infos,
            prev_interpret_alloc_index: &self.prev_interpret_alloc_index,
            interpret_alloc_cache: &self.interpret_alloc_cache,
        };

        match decode_tagged(&mut decoder, dep_node_index) {
            Ok(value) => {
                Some(value)
            }
            Err(e) => {
                bug!("Could not decode cached {}: {}", debug_tag, e)
            }
        }
    }

    // This function builds mapping from previous-session-CrateNum to
    // current-session-CrateNum. There might be CrateNums from the previous
    // Session that don't occur in the current one. For these, the mapping
    // maps to None.
    fn compute_cnum_map(tcx: TyCtxt,
                        prev_cnums: &[(u32, String, CrateDisambiguator)])
                        -> IndexVec<CrateNum, Option<CrateNum>>
    {
        tcx.dep_graph.with_ignore(|| {
            let current_cnums = tcx.all_crate_nums(LOCAL_CRATE).iter().map(|&cnum| {
                let crate_name = tcx.original_crate_name(cnum)
                                    .as_str()
                                    .to_string();
                let crate_disambiguator = tcx.crate_disambiguator(cnum);
                ((crate_name, crate_disambiguator), cnum)
            }).collect::<FxHashMap<_,_>>();

            let map_size = prev_cnums.iter()
                                    .map(|&(cnum, ..)| cnum)
                                    .max()
                                    .unwrap_or(0) + 1;
            let mut map = IndexVec::new();
            map.resize(map_size as usize, None);

            for &(prev_cnum, ref crate_name, crate_disambiguator) in prev_cnums {
                let key = (crate_name.clone(), crate_disambiguator);
                map[CrateNum::from_u32(prev_cnum)] = current_cnums.get(&key).cloned();
            }

            map[LOCAL_CRATE] = Some(LOCAL_CRATE);
            map
        })
    }
}


//- DECODING -------------------------------------------------------------------

/// A decoder that can read the incr. comp. cache. It is similar to the one
/// we use for crate metadata decoding in that it can rebase spans and
/// eventually will also handle things that contain `Ty` instances.
struct CacheDecoder<'a, 'tcx: 'a, 'x> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    opaque: opaque::Decoder<'x>,
    codemap: &'x CodeMap,
    cnum_map: &'x IndexVec<CrateNum, Option<CrateNum>>,
    synthetic_expansion_infos: &'x Lock<FxHashMap<AbsoluteBytePos, SyntaxContext>>,
    file_index_to_file: &'x Lock<FxHashMap<FileMapIndex, Lrc<FileMap>>>,
    file_index_to_stable_id: &'x FxHashMap<FileMapIndex, StableFilemapId>,
    interpret_alloc_cache: &'x RefCell<FxHashMap<usize, interpret::AllocId>>,
    /// maps from index in the cache file to location in the cache file
    prev_interpret_alloc_index: &'x [AbsoluteBytePos],
}

impl<'a, 'tcx, 'x> CacheDecoder<'a, 'tcx, 'x> {
    fn file_index_to_file(&self, index: FileMapIndex) -> Lrc<FileMap> {
        let CacheDecoder {
            ref file_index_to_file,
            ref file_index_to_stable_id,
            ref codemap,
            ..
        } = *self;

        file_index_to_file.borrow_mut().entry(index).or_insert_with(|| {
            let stable_id = file_index_to_stable_id[&index];
            codemap.filemap_by_stable_id(stable_id)
                   .expect("Failed to lookup FileMap in new context.")
        }).clone()
    }
}

trait DecoderWithPosition: Decoder {
    fn position(&self) -> usize;
}

impl<'enc> DecoderWithPosition for opaque::Decoder<'enc> {
    fn position(&self) -> usize {
        self.position()
    }
}

impl<'a, 'tcx, 'x> DecoderWithPosition for CacheDecoder<'a, 'tcx, 'x> {
    fn position(&self) -> usize {
        self.opaque.position()
    }
}

// Decode something that was encoded with encode_tagged() and verify that the
// tag matches and the correct amount of bytes was read.
fn decode_tagged<'a, 'tcx, D, T, V>(decoder: &mut D,
                                    expected_tag: T)
                                    -> Result<V, D::Error>
    where T: Decodable + Eq + ::std::fmt::Debug,
          V: Decodable,
          D: DecoderWithPosition,
          'tcx: 'a,
{
    let start_pos = decoder.position();

    let actual_tag = T::decode(decoder)?;
    assert_eq!(actual_tag, expected_tag);
    let value = V::decode(decoder)?;
    let end_pos = decoder.position();

    let expected_len: u64 = Decodable::decode(decoder)?;
    assert_eq!((end_pos - start_pos) as u64, expected_len);

    Ok(value)
}


impl<'a, 'tcx: 'a, 'x> ty_codec::TyDecoder<'a, 'tcx> for CacheDecoder<'a, 'tcx, 'x> {

    #[inline]
    fn tcx(&self) -> TyCtxt<'a, 'tcx, 'tcx> {
        self.tcx
    }

    #[inline]
    fn position(&self) -> usize {
        self.opaque.position()
    }

    #[inline]
    fn peek_byte(&self) -> u8 {
        self.opaque.data[self.opaque.position()]
    }

    fn cached_ty_for_shorthand<F>(&mut self,
                                  shorthand: usize,
                                  or_insert_with: F)
                                  -> Result<ty::Ty<'tcx>, Self::Error>
        where F: FnOnce(&mut Self) -> Result<ty::Ty<'tcx>, Self::Error>
    {
        let tcx = self.tcx();

        let cache_key = ty::CReaderCacheKey {
            cnum: RESERVED_FOR_INCR_COMP_CACHE,
            pos: shorthand,
        };

        if let Some(&ty) = tcx.rcache.borrow().get(&cache_key) {
            return Ok(ty);
        }

        let ty = or_insert_with(self)?;
        // This may overwrite the entry, but it should overwrite with the same value
        tcx.rcache.borrow_mut().insert_same(cache_key, ty);
        Ok(ty)
    }

    fn with_position<F, R>(&mut self, pos: usize, f: F) -> R
        where F: FnOnce(&mut Self) -> R
    {
        debug_assert!(pos < self.opaque.data.len());

        let new_opaque = opaque::Decoder::new(self.opaque.data, pos);
        let old_opaque = mem::replace(&mut self.opaque, new_opaque);
        let r = f(self);
        self.opaque = old_opaque;
        r
    }

    fn map_encoded_cnum_to_current(&self, cnum: CrateNum) -> CrateNum {
        self.cnum_map[cnum].unwrap_or_else(|| {
            bug!("Could not find new CrateNum for {:?}", cnum)
        })
    }
}

implement_ty_decoder!( CacheDecoder<'a, 'tcx, 'x> );

impl<'a, 'tcx, 'x> SpecializedDecoder<interpret::AllocId> for CacheDecoder<'a, 'tcx, 'x> {
    fn specialized_decode(&mut self) -> Result<interpret::AllocId, Self::Error> {
        let tcx = self.tcx;
        let idx = usize::decode(self)?;
        trace!("loading index {}", idx);

        if let Some(cached) = self.interpret_alloc_cache.borrow().get(&idx).cloned() {
            trace!("loading alloc id {:?} from alloc_cache", cached);
            return Ok(cached);
        }
        let pos = self.prev_interpret_alloc_index[idx].to_usize();
        trace!("loading position {}", pos);
        self.with_position(pos, |this| {
            interpret::specialized_decode_alloc_id(
                this,
                tcx,
                |this, alloc_id| {
                    trace!("caching idx {} for alloc id {} at position {}", idx, alloc_id, pos);
                    assert!(this
                        .interpret_alloc_cache
                        .borrow_mut()
                        .insert(idx, alloc_id)
                        .is_none());
                },
            )
        })
    }
}
impl<'a, 'tcx, 'x> SpecializedDecoder<Span> for CacheDecoder<'a, 'tcx, 'x> {
    fn specialized_decode(&mut self) -> Result<Span, Self::Error> {
        let tag: u8 = Decodable::decode(self)?;

        if tag == TAG_INVALID_SPAN {
            return Ok(DUMMY_SP);
        } else {
            debug_assert_eq!(tag, TAG_VALID_SPAN);
        }

        let file_lo_index = FileMapIndex::decode(self)?;
        let line_lo = usize::decode(self)?;
        let col_lo = BytePos::decode(self)?;
        let len = BytePos::decode(self)?;

        let file_lo = self.file_index_to_file(file_lo_index);
        let lo = file_lo.lines.borrow()[line_lo - 1] + col_lo;
        let hi = lo + len;

        let expn_info_tag = u8::decode(self)?;

        let ctxt = match expn_info_tag {
            TAG_NO_EXPANSION_INFO => {
                SyntaxContext::empty()
            }
            TAG_EXPANSION_INFO_INLINE => {
                let pos = AbsoluteBytePos::new(self.opaque.position());
                let expn_info: ExpnInfo = Decodable::decode(self)?;
                let ctxt = SyntaxContext::allocate_directly(expn_info);
                self.synthetic_expansion_infos.borrow_mut().insert(pos, ctxt);
                ctxt
            }
            TAG_EXPANSION_INFO_SHORTHAND => {
                let pos = AbsoluteBytePos::decode(self)?;
                let cached_ctxt = self.synthetic_expansion_infos
                                      .borrow()
                                      .get(&pos)
                                      .cloned();

                if let Some(ctxt) = cached_ctxt {
                    ctxt
                } else {
                    let expn_info = self.with_position(pos.to_usize(), |this| {
                         ExpnInfo::decode(this)
                    })?;
                    let ctxt = SyntaxContext::allocate_directly(expn_info);
                    self.synthetic_expansion_infos.borrow_mut().insert(pos, ctxt);
                    ctxt
                }
            }
            _ => {
                unreachable!()
            }
        };

        Ok(Span::new(lo, hi, ctxt))
    }
}

// This impl makes sure that we get a runtime error when we try decode a
// DefIndex that is not contained in a DefId. Such a case would be problematic
// because we would not know how to transform the DefIndex to the current
// context.
impl<'a, 'tcx, 'x> SpecializedDecoder<DefIndex> for CacheDecoder<'a, 'tcx, 'x> {
    fn specialized_decode(&mut self) -> Result<DefIndex, Self::Error> {
        bug!("Trying to decode DefIndex outside the context of a DefId")
    }
}

// Both the CrateNum and the DefIndex of a DefId can change in between two
// compilation sessions. We use the DefPathHash, which is stable across
// sessions, to map the old DefId to the new one.
impl<'a, 'tcx, 'x> SpecializedDecoder<DefId> for CacheDecoder<'a, 'tcx, 'x> {
    #[inline]
    fn specialized_decode(&mut self) -> Result<DefId, Self::Error> {
        // Load the DefPathHash which is was we encoded the DefId as.
        let def_path_hash = DefPathHash::decode(self)?;

        // Using the DefPathHash, we can lookup the new DefId
        Ok(self.tcx().def_path_hash_to_def_id.as_ref().unwrap()[&def_path_hash])
    }
}

impl<'a, 'tcx, 'x> SpecializedDecoder<LocalDefId> for CacheDecoder<'a, 'tcx, 'x> {
    #[inline]
    fn specialized_decode(&mut self) -> Result<LocalDefId, Self::Error> {
        Ok(LocalDefId::from_def_id(DefId::decode(self)?))
    }
}

impl<'a, 'tcx, 'x> SpecializedDecoder<hir::HirId> for CacheDecoder<'a, 'tcx, 'x> {
    fn specialized_decode(&mut self) -> Result<hir::HirId, Self::Error> {
        // Load the DefPathHash which is was we encoded the DefIndex as.
        let def_path_hash = DefPathHash::decode(self)?;

        // Use the DefPathHash to map to the current DefId.
        let def_id = self.tcx()
                         .def_path_hash_to_def_id
                         .as_ref()
                         .unwrap()[&def_path_hash];

        debug_assert!(def_id.is_local());

        // The ItemLocalId needs no remapping.
        let local_id = hir::ItemLocalId::decode(self)?;

        // Reconstruct the HirId and look up the corresponding NodeId in the
        // context of the current session.
        Ok(hir::HirId {
            owner: def_id.index,
            local_id
        })
    }
}

// NodeIds are not stable across compilation sessions, so we store them in their
// HirId representation. This allows use to map them to the current NodeId.
impl<'a, 'tcx, 'x> SpecializedDecoder<NodeId> for CacheDecoder<'a, 'tcx, 'x> {
    #[inline]
    fn specialized_decode(&mut self) -> Result<NodeId, Self::Error> {
        let hir_id = hir::HirId::decode(self)?;
        Ok(self.tcx().hir.hir_to_node_id(hir_id))
    }
}

impl<'a, 'tcx, 'x> SpecializedDecoder<Fingerprint> for CacheDecoder<'a, 'tcx, 'x> {
    fn specialized_decode(&mut self) -> Result<Fingerprint, Self::Error> {
        Fingerprint::decode_opaque(&mut self.opaque)
    }
}

impl<'a, 'tcx, 'x, T: Decodable> SpecializedDecoder<mir::ClearCrossCrate<T>>
for CacheDecoder<'a, 'tcx, 'x> {
    #[inline]
    fn specialized_decode(&mut self) -> Result<mir::ClearCrossCrate<T>, Self::Error> {
        let discr = u8::decode(self)?;

        match discr {
            TAG_CLEAR_CROSS_CRATE_CLEAR => Ok(mir::ClearCrossCrate::Clear),
            TAG_CLEAR_CROSS_CRATE_SET => {
                let val = T::decode(self)?;
                Ok(mir::ClearCrossCrate::Set(val))
            }
            _ => {
                unreachable!()
            }
        }
    }
}

//- ENCODING -------------------------------------------------------------------

struct CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder,
          'tcx: 'a,
{
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    encoder: &'enc mut E,
    type_shorthands: FxHashMap<ty::Ty<'tcx>, usize>,
    predicate_shorthands: FxHashMap<ty::Predicate<'tcx>, usize>,
    expn_info_shorthands: FxHashMap<Mark, AbsoluteBytePos>,
    interpret_allocs: FxHashMap<interpret::AllocId, usize>,
    interpret_allocs_inverse: Vec<interpret::AllocId>,
    codemap: CachingCodemapView<'tcx>,
    file_to_file_index: FxHashMap<*const FileMap, FileMapIndex>,
}

impl<'enc, 'a, 'tcx, E> CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder
{
    fn filemap_index(&mut self, filemap: Lrc<FileMap>) -> FileMapIndex {
        self.file_to_file_index[&(&*filemap as *const FileMap)]
    }

    /// Encode something with additional information that allows to do some
    /// sanity checks when decoding the data again. This method will first
    /// encode the specified tag, then the given value, then the number of
    /// bytes taken up by tag and value. On decoding, we can then verify that
    /// we get the expected tag and read the expected number of bytes.
    fn encode_tagged<T: Encodable, V: Encodable>(&mut self,
                                                 tag: T,
                                                 value: &V)
                                                 -> Result<(), E::Error>
    {
        use ty::codec::TyEncoder;
        let start_pos = self.position();

        tag.encode(self)?;
        value.encode(self)?;

        let end_pos = self.position();
        ((end_pos - start_pos) as u64).encode(self)
    }
}

impl<'enc, 'a, 'tcx, E> SpecializedEncoder<interpret::AllocId> for CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder
{
    fn specialized_encode(&mut self, alloc_id: &interpret::AllocId) -> Result<(), Self::Error> {
        use std::collections::hash_map::Entry;
        let index = match self.interpret_allocs.entry(*alloc_id) {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(e) => {
                let idx = self.interpret_allocs_inverse.len();
                self.interpret_allocs_inverse.push(*alloc_id);
                e.insert(idx);
                idx
            },
        };

        index.encode(self)
    }
}

impl<'enc, 'a, 'tcx, E> SpecializedEncoder<Span> for CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder
{
    fn specialized_encode(&mut self, span: &Span) -> Result<(), Self::Error> {

        if *span == DUMMY_SP {
            return TAG_INVALID_SPAN.encode(self);
        }

        let span_data = span.data();

        if span_data.hi < span_data.lo {
            return TAG_INVALID_SPAN.encode(self);
        }

        let (file_lo, line_lo, col_lo) = match self.codemap
                                                   .byte_pos_to_line_and_col(span_data.lo) {
            Some(pos) => pos,
            None => {
                return TAG_INVALID_SPAN.encode(self);
            }
        };

        if !file_lo.contains(span_data.hi) {
            return TAG_INVALID_SPAN.encode(self);
        }

        let len = span_data.hi - span_data.lo;

        let filemap_index = self.filemap_index(file_lo);

        TAG_VALID_SPAN.encode(self)?;
        filemap_index.encode(self)?;
        line_lo.encode(self)?;
        col_lo.encode(self)?;
        len.encode(self)?;

        if span_data.ctxt == SyntaxContext::empty() {
            TAG_NO_EXPANSION_INFO.encode(self)
        } else {
            let mark = span_data.ctxt.outer();

            if let Some(expn_info) = mark.expn_info() {
                if let Some(pos) = self.expn_info_shorthands.get(&mark).cloned() {
                    TAG_EXPANSION_INFO_SHORTHAND.encode(self)?;
                    pos.encode(self)
                } else {
                    TAG_EXPANSION_INFO_INLINE.encode(self)?;
                    let pos = AbsoluteBytePos::new(self.position());
                    self.expn_info_shorthands.insert(mark, pos);
                    expn_info.encode(self)
                }
            } else {
                TAG_NO_EXPANSION_INFO.encode(self)
            }
        }
    }
}

impl<'enc, 'a, 'tcx, E> ty_codec::TyEncoder for CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder
{
    #[inline]
    fn position(&self) -> usize {
        self.encoder.position()
    }
}

impl<'enc, 'a, 'tcx, E> SpecializedEncoder<CrateNum> for CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder
{
    #[inline]
    fn specialized_encode(&mut self, cnum: &CrateNum) -> Result<(), Self::Error> {
        self.emit_u32(cnum.as_u32())
    }
}

impl<'enc, 'a, 'tcx, E> SpecializedEncoder<ty::Ty<'tcx>> for CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder
{
    #[inline]
    fn specialized_encode(&mut self, ty: &ty::Ty<'tcx>) -> Result<(), Self::Error> {
        ty_codec::encode_with_shorthand(self, ty,
            |encoder| &mut encoder.type_shorthands)
    }
}

impl<'enc, 'a, 'tcx, E> SpecializedEncoder<ty::GenericPredicates<'tcx>>
    for CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder
{
    #[inline]
    fn specialized_encode(&mut self,
                          predicates: &ty::GenericPredicates<'tcx>)
                          -> Result<(), Self::Error> {
        ty_codec::encode_predicates(self, predicates,
            |encoder| &mut encoder.predicate_shorthands)
    }
}

impl<'enc, 'a, 'tcx, E> SpecializedEncoder<hir::HirId> for CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder
{
    #[inline]
    fn specialized_encode(&mut self, id: &hir::HirId) -> Result<(), Self::Error> {
        let hir::HirId {
            owner,
            local_id,
        } = *id;

        let def_path_hash = self.tcx.hir.definitions().def_path_hash(owner);

        def_path_hash.encode(self)?;
        local_id.encode(self)
    }
}


impl<'enc, 'a, 'tcx, E> SpecializedEncoder<DefId> for CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder
{
    #[inline]
    fn specialized_encode(&mut self, id: &DefId) -> Result<(), Self::Error> {
        let def_path_hash = self.tcx.def_path_hash(*id);
        def_path_hash.encode(self)
    }
}

impl<'enc, 'a, 'tcx, E> SpecializedEncoder<LocalDefId> for CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder
{
    #[inline]
    fn specialized_encode(&mut self, id: &LocalDefId) -> Result<(), Self::Error> {
        id.to_def_id().encode(self)
    }
}

impl<'enc, 'a, 'tcx, E> SpecializedEncoder<DefIndex> for CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder
{
    fn specialized_encode(&mut self, _: &DefIndex) -> Result<(), Self::Error> {
        bug!("Encoding DefIndex without context.")
    }
}

// NodeIds are not stable across compilation sessions, so we store them in their
// HirId representation. This allows use to map them to the current NodeId.
impl<'enc, 'a, 'tcx, E> SpecializedEncoder<NodeId> for CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder
{
    #[inline]
    fn specialized_encode(&mut self, node_id: &NodeId) -> Result<(), Self::Error> {
        let hir_id = self.tcx.hir.node_to_hir_id(*node_id);
        hir_id.encode(self)
    }
}

impl<'enc, 'a, 'tcx> SpecializedEncoder<Fingerprint>
for CacheEncoder<'enc, 'a, 'tcx, opaque::Encoder<'enc>>
{
    fn specialized_encode(&mut self, f: &Fingerprint) -> Result<(), Self::Error> {
        f.encode_opaque(&mut self.encoder)
    }
}

impl<'enc, 'a, 'tcx, E, T> SpecializedEncoder<mir::ClearCrossCrate<T>>
for CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder,
          T: Encodable,
{
    #[inline]
    fn specialized_encode(&mut self,
                          val: &mir::ClearCrossCrate<T>)
                          -> Result<(), Self::Error> {
        match *val {
            mir::ClearCrossCrate::Clear => {
                TAG_CLEAR_CROSS_CRATE_CLEAR.encode(self)
            }
            mir::ClearCrossCrate::Set(ref val) => {
                TAG_CLEAR_CROSS_CRATE_SET.encode(self)?;
                val.encode(self)
            }
        }
    }
}

macro_rules! encoder_methods {
    ($($name:ident($ty:ty);)*) => {
        $(fn $name(&mut self, value: $ty) -> Result<(), Self::Error> {
            self.encoder.$name(value)
        })*
    }
}

impl<'enc, 'a, 'tcx, E> Encoder for CacheEncoder<'enc, 'a, 'tcx, E>
    where E: 'enc + ty_codec::TyEncoder
{
    type Error = E::Error;

    fn emit_nil(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    encoder_methods! {
        emit_usize(usize);
        emit_u128(u128);
        emit_u64(u64);
        emit_u32(u32);
        emit_u16(u16);
        emit_u8(u8);

        emit_isize(isize);
        emit_i128(i128);
        emit_i64(i64);
        emit_i32(i32);
        emit_i16(i16);
        emit_i8(i8);

        emit_bool(bool);
        emit_f64(f64);
        emit_f32(f32);
        emit_char(char);
        emit_str(&str);
    }
}

// An integer that will always encode to 8 bytes.
struct IntEncodedWithFixedSize(u64);

impl IntEncodedWithFixedSize {
    pub const ENCODED_SIZE: usize = 8;
}

impl UseSpecializedEncodable for IntEncodedWithFixedSize {}
impl UseSpecializedDecodable for IntEncodedWithFixedSize {}

impl<'enc> SpecializedEncoder<IntEncodedWithFixedSize> for opaque::Encoder<'enc> {
    fn specialized_encode(&mut self, x: &IntEncodedWithFixedSize) -> Result<(), Self::Error> {
        let start_pos = self.position();
        for i in 0 .. IntEncodedWithFixedSize::ENCODED_SIZE {
            ((x.0 >> i * 8) as u8).encode(self)?;
        }
        let end_pos = self.position();
        assert_eq!((end_pos - start_pos), IntEncodedWithFixedSize::ENCODED_SIZE);
        Ok(())
    }
}

impl<'enc> SpecializedDecoder<IntEncodedWithFixedSize> for opaque::Decoder<'enc> {
    fn specialized_decode(&mut self) -> Result<IntEncodedWithFixedSize, Self::Error> {
        let mut value: u64 = 0;
        let start_pos = self.position();

        for i in 0 .. IntEncodedWithFixedSize::ENCODED_SIZE {
            let byte: u8 = Decodable::decode(self)?;
            value |= (byte as u64) << (i * 8);
        }

        let end_pos = self.position();
        assert_eq!((end_pos - start_pos), IntEncodedWithFixedSize::ENCODED_SIZE);

        Ok(IntEncodedWithFixedSize(value))
    }
}

fn encode_query_results<'enc, 'a, 'tcx, Q, E>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                              encoder: &mut CacheEncoder<'enc, 'a, 'tcx, E>,
                                              query_result_index: &mut EncodedQueryResultIndex)
                                              -> Result<(), E::Error>
    where Q: super::plumbing::GetCacheInternal<'tcx>,
          E: 'enc + TyEncoder,
          Q::Value: Encodable,
{
    let desc = &format!("encode_query_results for {}",
        unsafe { ::std::intrinsics::type_name::<Q>() });

    time(tcx.sess, desc, || {

    for (key, entry) in Q::get_cache_internal(tcx).map.iter() {
        if Q::cache_on_disk(key.clone()) {
            let entry = match *entry {
                QueryResult::Complete(ref v) => v,
                _ => panic!("incomplete query"),
            };
            let dep_node = SerializedDepNodeIndex::new(entry.index.index());

            // Record position of the cache entry
            query_result_index.push((dep_node, AbsoluteBytePos::new(encoder.position())));

            // Encode the type check tables with the SerializedDepNodeIndex
            // as tag.
            encoder.encode_tagged(dep_node, &entry.value)?;
        }
    }

    Ok(())
    })
}
