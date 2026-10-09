//! Port of `checker/links.go` (tsgo#4329).
//!
//! PORT: Go adds `core.PagedLinkStore` (pages of 256 values, a page list for
//! low page indexes and a map for high ones) and the two stores below on top
//! of it. `core::LinkStore` is paged the same way for every store: node keys
//! use per-file page tables of 64-key pages, arena keys (symbols) use one
//! page table, and a page holds the values. So both Go stores map onto
//! `core::LinkStore`; a store of large values holds `Box<V>`, like Go
//! `symbolArenaLinkStore`. `has` and `try_get` keep the exact "a record
//! exists" answer. Go
//! `nodeLinkStore.TryGet` also answers a zero record for a key in an
//! allocated page; its one reader (`tryGetResolvedSymbolFromTypeNode`) then
//! reads a nil `resolvedSymbol`, which is the same result as no record.

use crate::prelude::*;

// Go: checker/links.go:10 nodeLinkStore
/// A links store keyed by node references (Go stores the values in the pages).
pub type NodeLinkStore<V> = LinkStore<Node, V>;

// Go: checker/links.go:28 symbolArenaLinkStore
/// A links store keyed by symbol references (Go stores the values in an arena;
/// here they sit in the pages, as most keys of a page have a record).
/// Read it with `SymbolArenaLinks`, which gives the symbol its id as Go does.
pub type SymbolArenaLinkStore<V> = LinkStore<SymbolId, V>;

/// The reads of `Checker::value_symbol_links`, Go's one
/// `symbolArenaLinkStore`. Go keys the store by `ast.GetSymbolId`, so each
/// `Get`, `TryGet` and `Has` gives the symbol its id first. The ids count up
/// in that order (`ast::get_symbol_id`), and a late-bound name holds the id
/// of its unique symbol (`__@k@<id>`, Go `getESSymbolLikeTypeForNode`). The
/// node builder counts the length of that name toward truncation, so the ids
/// must count as Go's do.
// PORT: the store is still keyed by the arena index; only the id order is
// Go's. A record notes that its symbol has its id (`has_id`), so later reads
// skip `get_symbol_id` (each read with the call cost 1.3% (query) to 4.9%
// (zod) more user instructions). Ids are given in Go's order: no other id is
// given between the read and the id.
pub trait SymbolArenaLinks {
    /// Go `symbolArenaLinkStore.Get`.
    fn get_by_id(&mut self, symbols: &SymbolArena, symbol: SymbolId) -> &mut ValueSymbolLinks;
    /// Go `symbolArenaLinkStore.Get` followed by writes to the new record
    /// (`LinkStore::insert_new`).
    fn insert_new_by_id(
        &mut self,
        symbols: &SymbolArena,
        symbol: SymbolId,
        value: ValueSymbolLinks,
    ) -> &mut ValueSymbolLinks;
    /// Go `symbolArenaLinkStore.TryGet`.
    fn try_get_by_id(&self, symbols: &SymbolArena, symbol: SymbolId) -> Option<&ValueSymbolLinks>;
    /// Go `symbolArenaLinkStore.Has`.
    fn has_by_id(&self, symbols: &SymbolArena, symbol: SymbolId) -> bool;
}

impl SymbolArenaLinks for SymbolArenaLinkStore<ValueSymbolLinks> {
    #[inline]
    fn get_by_id(&mut self, symbols: &SymbolArena, symbol: SymbolId) -> &mut ValueSymbolLinks {
        let links = self.get(symbol);
        if !links.has_id {
            give_id(symbols, symbol, links);
        }
        links
    }

    #[inline]
    fn insert_new_by_id(
        &mut self,
        symbols: &SymbolArena,
        symbol: SymbolId,
        value: ValueSymbolLinks,
    ) -> &mut ValueSymbolLinks {
        get_symbol_id(symbols, symbol);
        let links = self.insert_new(symbol, value);
        links.has_id = true;
        links
    }

    #[inline]
    fn try_get_by_id(&self, symbols: &SymbolArena, symbol: SymbolId) -> Option<&ValueSymbolLinks> {
        let links = self.try_get(symbol);
        if !links.is_some_and(|links| links.has_id) {
            give_id_without_record(symbols, symbol);
        }
        links
    }

    #[inline]
    fn has_by_id(&self, symbols: &SymbolArena, symbol: SymbolId) -> bool {
        self.try_get_by_id(symbols, symbol).is_some()
    }
}

/// The first read of `symbol` in `get_by_id`: gives it its id and notes it.
// PERF: out of line, so each inlined read adds only the test.
#[cold]
#[inline(never)]
fn give_id(symbols: &SymbolArena, symbol: SymbolId, links: &mut ValueSymbolLinks) {
    get_symbol_id(symbols, symbol);
    links.has_id = true;
}

/// `try_get_by_id` and `has_by_id` of `symbol` without a noted id: gives it
/// its id (a no-op when it has one).
#[cold]
#[inline(never)]
fn give_id_without_record(symbols: &SymbolArena, symbol: SymbolId) {
    get_symbol_id(symbols, symbol);
}
