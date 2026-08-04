---
id: 0003
title: Per-module COW, no centralized abstraction
date: unknown
status: Accepted
---

# 0003. Per-module COW, no centralized abstraction

**Context:** Multiple modules need copy-on-write semantics: the handle table (radix tree) must clone the path from root to a modified leaf, the freemap rewrites itself on every commit's `persist_freemap`, data pages reuse the same page across commits via `claim_page`. A centralized COW abstraction (a trait, a generic page-mutation type) would seem to factor out repeated logic.

**Decision:** Each module implements its own COW. `handle_table.rs` clones the root-to-leaf path. `freemap.rs` allocates a new freemap page in `persist_freemap`. `data_page.rs` mutates in place via `claim_page` (which takes `&mut PageBytes` from `PageCache::write_page`). No `trait Cow` or `enum CowStrategy`.

**Alternatives considered:**

- *Centralized `trait Cow` over all page-type modules.* Would require uniform interface (e.g., `fn cow_root(&mut self, cache, root_id) -> Result<u64>`). Rejected because the modules' actual COW shapes differ enough that the trait would either be too generic (lose useful information) or too specific (have variants that work for only one module).
- *Generic page-mutation type that wraps a strategy.* Same problem as above plus the ergonomic cost of generics in the public API.

**Consequences:**

- *Positive:* Each module's COW logic is co-located with the page-type logic it serves. Reading `handle_table.rs` shows you both the radix tree algorithm and the COW it implements.
- *Positive:* Freedom to evolve. The handle table's COW grew several optimizations (`grow()`, short-circuit at depth boundaries) without affecting other modules.
- *Negative:* Repeated boilerplate across 3-4 modules. Each writes its own "allocate a new page, write the new state, return the new page ID."
- *Negative:* Onboarding cost. A new contributor wonders "where is the COW abstraction?" and the answer is "there isn't one, and that's deliberate."

---
