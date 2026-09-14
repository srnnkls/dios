Yes. After looking across current Linux/io_uring facilities, PostgreSQL 18 AIO/read streams, LeanStore/Umbra, DuckDB/xNVMe work, and Rust's io_uring ecosystem, I think there is a fairly clear modern shape.

The short version is:

**The elegant design is not “make frame reclamation faster.” It is “stop coupling I/O admission to synchronous frame reclamation.”**

Your pressured-regime failure is almost a textbook signal that the pool has one concept doing two jobs:

> “Can I issue this I/O?”
> and
> “Can I synchronously obtain an epoch-reclaimable demand frame right now?”

Those should become separate questions.

## The modern shape

I would model the pool as having **physical frames**, **residency identity**, and **admission credits** as separate resources.

Conceptually:

```text
                       ┌──────── demand get ────────┐
                       │                            │
PageId ── lookup ──> resident? ── yes ──> pin ─────┤
                       │                            │
                       no                           │
                       │                            │
                  pending miss? ─ yes ──> join ────┤
                       │                            │
                       no                           │
                       ▼                            │
                 reserve admission                 │
                       │                            │
                 obtain / assign frame             │
                       │                            │
                  issue async I/O                  │
                       │                            │
                    resident ──────────────────────┘
```

The important new thing is that **prefetch uses a separately bounded admission class**:

```text
total frame budget
├── ordinary demand population
└── prefetch-reserved headroom
       ├── free
       ├── in-flight
       └── resident-but-unconsumed
```

Not necessarily physically separate allocations. Logically separate capacity.

PostgreSQL already uses this general idea elsewhere: bulk operations such as VACUUM can operate through a bounded Buffer Access Strategy rather than being allowed to displace arbitrary amounts of the shared buffer pool; its current default VACUUM ring is explicitly capped. PostgreSQL 18's read-stream/AIO work also separates the predictable stream-producing consumer from the asynchronous I/O machinery underneath it. ([PostgreSQL][1])

That matches your emerging seam surprisingly well:

```rust
pool.prefetch(&[PageId]) -> PrefetchReport
```

Plan construction stays above the pool; capacity control stays inside it.

---

# 1. Don't make prefetch wait for the ordinary eviction lifecycle

This is the biggest conclusion I'd draw from your benchmark.

Right now, roughly:

```text
prefetch
   ↓
claim_frame()
   ↓
only epoch-matured victims available
   ↓
burst exhausts them
   ↓
Busy
   ↓
effective QD → 1
```

That's exactly the wrong feedback loop for NVMe.

Modern NVMe requires enough independent outstanding I/O to expose device parallelism; recent database work continues finding that the architecture around asynchronous I/O matters at least as much as swapping one syscall API for another. The 2025 io_uring DBMS study specifically finds that naïve io_uring adoption isn't automatically beneficial and studies registered buffers, passthrough, batching, and architecture as separable contributors. ([arXiv][2])

LeanStore's NVMe-oriented work goes even further: its goal is to have sufficient free-buffer supply and asynchronous write/reclamation machinery so foreground work isn't serially producing one reusable frame per I/O request. The broader LeanStore project explicitly targets fully exploiting fast NVMe while maintaining low buffer-manager overhead. ([Department of Computer Science][3])

So I would **not** solve your problem with:

```rust
while let Some(frame) = try_reclaim_epoch_matured_frame() {
    ...
}
```

plus clever retrying.

I'd introduce a bounded supply specifically capable of sustaining speculative I/O.

---

# 2. `staging_headroom` should really mean admission credits

This is where I'd tweak the terminology/design internally.

You don't fundamentally want:

```text
64 special prefetch frames
```

You want:

```text
at most 64 frames worth of speculative residency obligations
```

Those are subtly different.

Call the internal concept something like:

```rust
PrefetchBudget
PrefetchCredits
SpeculativeBudget
```

Even though the user-facing API remains `prefetch`.

A credit covers the entire speculative lifecycle:

```text
reserved
   ↓
in-flight
   ↓
resident, unconsumed
   ↓
consumed ───────────> becomes ordinary cache population
   │
   └── unused ──────> first-class eviction candidate
```

This gives you a hard invariant:

```text
prefetch_inflight
+ prefetch_resident_unconsumed
<= prefetch_headroom
```

That is considerably easier to reason about than trying to make the eviction policy “approximately avoid pollution.”

PostgreSQL's bounded buffer access strategies illustrate the same high-level principle: special streaming activity gets a bounded footprint rather than permission to churn the whole cache. ([PostgreSQL][1])

## Crucially

When a prefetched page becomes demanded:

```text
PrefetchedUnconsumed -> Normal
```

and the prefetch credit is returned.

So you're not reserving part of the buffer pool forever.

You're bounding **speculation**, not partitioning the cache.

That is elegant.

---

# 3. Make "prefetched" orthogonal metadata, not a `FrameState`

Your previous research correction was right.

I would resist:

```rust
enum FrameState {
    Free,
    Reading,
    Resident,
    Prefetched,
    ...
}
```

because "prefetched" answers a different question.

A frame can simultaneously be:

```text
Resident
+ prefetched
+ unconsumed
```

then:

```text
Resident
+ normal
```

without undergoing an I/O lifecycle transition.

So something closer to:

```rust
enum Residency {
    Free,
    Loading,
    Resident,
    Evicting,
}
```

plus:

```rust
enum AdmissionClass {
    Demand,
    PrefetchUnconsumed,
}
```

is conceptually cleaner.

Potentially even a bit:

```rust
prefetch_origin: bool
```

if the only special state is “arrived speculatively and hasn't yet been touched.”

This also resembles how modern cache designs increasingly separate **replacement metadata** from object/storage state instead of encoding every policy condition into the frame's lifecycle state.

---

# 4. Don't reserve the actual frame too early if you can avoid it

There's an interesting design fork here.

### Simple implementation

At `prefetch()` time:

```text
credit -> frame -> submit read(frame)
```

This is easiest and probably what I'd implement first.

### More sophisticated implementation

Reserve an admission credit first, but bind it to a concrete frame as late as practical:

```text
credit -> queued IO intent -> free frame -> submit
```

That can decouple submission planning from frame availability even further.

But there's a limit: an `O_DIRECT` read ultimately needs a valid aligned userspace destination when submitted, and io_uring fixed-buffer operations specifically identify registered memory at submission. ([Docs.rs][4])

So for your workload, I suspect **frame-at-submission is the right simplicity point**.

The important thing isn't delaying frame selection.

It's ensuring there is a replenished inventory of frames that prefetch is allowed to use without synchronously traversing the ordinary reclamation path.

---

# 5. Reclamation should replenish supply asynchronously

Your benchmark says the pool currently behaves like:

```text
consumer needs frame
→ consumer discovers none available
→ consumer initiates reclamation
```

A higher-performance design tends toward:

```text
reclaimer observes low watermark
→ prepares reusable frames ahead of demand
→ consumer obtains prepared frame in O(1)-ish path
```

Think:

```text
              background / cooperative
                    reclamation
                         │
                         ▼
                 ┌─────────────┐
                 │ free/ready  │
                 │ frame FIFO  │
                 └─────────────┘
                       ▲
                       │
demand claim ──────────┤
prefetch claim ────────┘
```

with watermarks:

```rust
low_watermark
target_watermark
```

rather than doing full victim discovery on every miss.

LeanStore's design history is highly relevant here. Its buffer manager deliberately uses a “cooling” stage between hot residency and eviction, so pages can be identified for replacement without making every foreground lookup perform a traditional global replacement-policy operation. ([db.in.tum.de][5])

I wouldn't copy LeanStore literally, especially given your epoch scheme. But the principle is valuable:

> **Prepare eviction candidates before the requester needs them.**

Your epoch reclamation remains useful for safety. It just shouldn't determine instantaneous queue depth.

---

# 6. In dios I'd probably use two free supplies, not two pools

My concrete recommendation:

```text
FrameArena
┌───────────────────────────────────┐
│ physical aligned frames           │
│                                   │
│ metadata says current ownership   │
└───────────────────────────────────┘

Ready queues:
    demand_ready
    prefetch_ready/reserved
```

But they share the same frame arena.

Something like:

```rust
struct FramePool {
    arena: FrameArena,

    demand: DemandAdmission,
    prefetch: PrefetchAdmission,

    ...
}
```

The prefetch reservation can have a guaranteed minimum / hard maximum.

For example:

```text
total frames:          2048
prefetch headroom:       64

ordinary population: <= 1984-ish while speculation full
speculation:          <=   64
```

But if speculation is idle, **don't physically waste those 64 frames**.

You can implement this via accounting rather than static partitioning.

The invariant is more important than physical ownership:

```rust
assert!(prefetch.obligations() <= config.prefetch_headroom);
```

---

# 7. Prefetch should lose eviction contests

I'd make eviction ordering intentionally asymmetric.

Roughly:

```text
1. resident + prefetch + unconsumed
2. normal cold candidates
3. ...
```

Consumption performs promotion:

```rust
get(page)
  -> sees resident prefetched page
  -> atomically mark consumed
  -> release prefetch credit
  -> ordinary policy touch
```

This is a much stronger semantic than merely giving prefetched pages low recency.

It gives you exactly the statistic you care about:

```text
prefetch_admitted
prefetch_consumed
prefetch_evicted_unconsumed
```

and a real pollution measure:

```text
accuracy =
    consumed / admitted
```

plus perhaps:

```text
waste_bytes
```

and:

```text
useful_prefetch_distance
```

This is close to the “separate staging FIFO / promote on consumption” family of designs you already found compelling.

---

# 8. `PrefetchReport` is more important than a ticket

I agree strongly with your earlier conclusion.

The caller needs **feedback**, not ownership.

I'd consider:

```rust
#[derive(Debug, Clone, Copy)]
pub struct PrefetchReport {
    pub requested: usize,
    pub resident: usize,
    pub pending: usize,
    pub admitted: usize,
    pub deferred: usize,
}
```

Possibly `rejected`, though I'd be careful about the semantics between deferred/rejected.

For adaptive lookahead, the consumer really wants to know:

```text
Did I ask for 32 and dios only accept 6?
```

not:

```text
Give me 26 lifecycle objects.
```

And demand `get()` remains the synchronization point.

That preserves your excellent seam:

```text
planning            dios
──────────────       ────────────────────
window formation
sorting
lookahead      --->  prefetch(PageIds)
adaptation      <--- PrefetchReport

consume         ---> get(PageId)
```

No plan semantics cross the boundary.

---

# 9. Rust-idiomatic buffer ownership: model the in-flight state in ownership

This is one place where Rust can materially improve the design.

io_uring's fundamental memory-safety problem is:

> The kernel may still be using this pointer after the submit call returns.

The Rust io_uring ecosystem generally handles that by making the I/O operation **own the buffer until completion**. `tokio-uring`, for example, explicitly transfers ownership of buffers to the runtime for an I/O and returns ownership at completion; its `IoBuf` contract requires the underlying pointer to stay stable while the runtime owns it. ([Docs.rs][6])

That's the Rust idiom I would copy even if dios doesn't use tokio-uring.

Don't expose:

```rust
submit_read(&mut [u8])
```

with an undocumented promise that nobody touches it.

Prefer internal state transitions that represent exclusive ownership:

```rust
FreeFrame
    -> IoFrame<ReadInFlight>
    -> ResidentFrame
```

Whether you implement that literally as types or just enforce it in the arena is a separate question.

I probably **wouldn't** make every state a generic typestate in the public implementation—it can get obnoxious inside a concurrent buffer pool.

But I'd absolutely make the ownership boundary explicit:

```rust
struct SubmittedRead {
    frame: FrameId,
    page: PageId,
    ...
}
```

and ensure no path can hand out mutable access to `frame` while the completion table owns it.

---

# 10. Use IDs internally, not long-lived Rust references

For a concurrent pool, I'd strongly favor:

```rust
FrameId
PageId
```

over moving `&mut Frame` through async machinery.

Something like:

```rust
#[repr(transparent)]
struct FrameId(u32);
```

Then:

```text
FrameArena[FrameId]
```

owns permanently allocated backing storage.

This has several benefits:

* buffer addresses remain stable;
* io_uring completion `user_data` can encode/point to stable operation IDs;
* you don't have lifetime parameters infecting the asynchronous scheduler;
* the ownership state lives in explicit metadata;
* it works naturally with registered-buffer indices.

This is where Rust's ordinary borrow checker is not the entire answer. Kernel asynchronous I/O outlives lexical borrows, so an **arena + stable IDs + state machine** is often more idiomatic than trying to express everything with references.

`tokio-uring` reaches a similar conclusion at its abstraction boundary: buffers must have stable backing memory even if the Rust value representing them moves. ([Docs.rs][6])

---

# 11. For aligned `O_DIRECT` memory: encapsulate the unsafe allocation once

Don't sprinkle alignment logic around the pool.

Create something like:

```rust
struct FrameArena {
    ptr: NonNull<u8>,
    frame_size: usize,
    frame_count: usize,
    layout: Layout,
}
```

or an allocator-backed equivalent.

Rust's `Layout` is specifically the standard mechanism for expressing allocation size/alignment constraints. ([Rust Documentation][7])

Then expose only:

```rust
fn frame_ptr(&self, id: FrameId) -> NonNull<u8>;
```

inside the unsafe backend layer.

Everything above that should believe:

```text
FrameId identifies one correctly sized,
correctly aligned, stable-address frame.
```

I would **not** reach for `Pin<Box<_>>` merely because this is async I/O.

`Pin` protects against moving the Rust *value*. Your actual requirement is that the backing allocation's address remain stable. Heap allocations already stay at a stable address when their owner object moves; tokio-uring's `IoBuf` contract reflects precisely this distinction. ([Docs.rs][6])

So an arena is cleaner than `Pin` everywhere.

---

# 12. Registered buffers should be an optimization layer, not the pool's ontology

This is important given your `RLIMIT_MEMLOCK` discovery.

io_uring supports registering fixed buffers ahead of time, avoiding repeated per-I/O setup, but registration has costs and lifecycle constraints. The Rust `io-uring` crate notes that registration itself is slow and exposes fixed-buffer operations separately from ordinary buffer I/O. ([Docs.rs][4])

So I would design:

```text
Frame
```

first, then:

```text
Frame ↔ optional registered-buffer slot
```

rather than defining:

```text
Frame == RegisteredBuffer
```

That gives you:

```rust
enum BufferBinding {
    Unregistered,
    Fixed(FixedBufId),
}
```

or a backend-owned table.

Then your benchmark can cleanly gate:

```text
normal aligned O_DIRECT
+ fixed buffers
+ fixed files
+ SQPOLL
+ passthrough
...
```

exactly as your research seed suggested.

The December 2025 DBMS/io_uring study is particularly supportive of treating these facilities as independently measurable optimizations rather than one indivisible “io_uring mode.” ([arXiv][2])

---

# 13. I would not start with provided-buffer rings for database reads

Linux's io_uring provided-buffer rings are elegant where the kernel receives data into **any interchangeable buffer** and reports which one it used. The kernel API explicitly supports selecting a buffer from a registered group. ([man7.org][8])

That's fantastic for network receive and similar workloads.

But your read is:

```text
PageId X must land in the frame that will represent PageId X.
```

You already have page→pending-miss identity and cache metadata.

So my default would be fixed/stable application-selected frames, not kernel-selected provided buffers.

Could provided buffers be made to work? Sure.

Would they simplify your buffer manager? I doubt it.

They solve a slightly different ownership problem.

---

# 14. Keep one singleflight entry per PageId

Your existing miss-table design is a major asset.

You already have:

```text
prefetch X starts
demand get(X) arrives
→ join existing operation
```

Keep that invariant.

The new prefetch admission path should ideally go through the **same page-level miss identity**, only with different admission class.

Something like:

```rust
enum MissOrigin {
    Demand,
    Prefetch,
}
```

Then:

```text
prefetch existing resident        → resident
prefetch existing pending demand  → pending
prefetch existing pending prefetch→ pending
prefetch absent + credit          → admitted
prefetch absent + no credit       → deferred
```

And:

```text
demand hits pending prefetch
→ join
→ optionally promote intent immediately
```

That last transition is interesting.

If a demand arrives while a prefetch is still in-flight, I'd probably treat it as **consumed/promoted at that point**, not after the CQE. The speculation has already proved useful.

That releases its speculative credit sooner or marks it as no longer evict-first once resident.

---

# 15. Separate queue-depth control from lookahead depth

Another subtle point.

The consumer should choose:

```text
lookahead distance
```

but dios must choose/enforce:

```text
actual outstanding speculative I/O
```

These aren't the same.

For example:

```text
consumer window: 64 pages
prefetch headroom: 16 frames
device target QD: 8
```

Then `prefetch(&64)` might say:

```text
8 already resident
3 already pending
16 admitted
37 deferred
```

That is healthy.

It means consumers express **opportunity**, while the pool controls resource usage.

That's cleaner than making the caller discover exact device queue-depth mechanics.

PostgreSQL's stream abstraction similarly aims to let callers describe predictable access while insulating them from evolving low-level I/O implementation details. ([PostgreSQL Wiki][9])

---

# 16. I'd use low/high watermarks for speculative capacity too

A good control loop could be:

```text
prefetch headroom = 64

high: 64 obligations
target: 48
low: 24
```

When speculative capacity falls below a target, reclaim more evictable speculative frames / prepare ordinary victims.

This avoids oscillating between:

```text
64 free
→ burst
→ 0 free
→ stall
→ reclaim
→ burst
```

You want inventory replenishment to happen while the device is busy.

This is an inference/design recommendation rather than something I'd claim is universally standardized, but it follows naturally from modern asynchronous buffer-manager designs and from your observed burst-collapse mechanism. LeanStore's recent NVMe direction similarly emphasizes having asynchronous cache replacement/writeback cooperate with high-concurrency I/O rather than letting foreground operations serialize behind frame production. ([Department of Computer Science][3])

---

# 17. Keep epochs for safety; remove them from admission latency

This may be the single most dios-specific recommendation.

I don't think your result says:

> epochs are wrong.

It says:

> **epoch maturity cannot be the only source of immediate I/O buffers.**

There's an architectural difference.

Use epochs to answer:

```text
Is this frame safe to reuse?
```

But arrange the system so a sufficient number of frames have **already crossed that boundary** before an I/O burst needs them.

So:

```text
epoch retirement
    ↓
background maturity
    ↓
ready-for-reuse inventory
    ↓
claim
```

rather than:

```text
claim
    ↓
scan retirees
    ↓
discover whether anything matured
```

That retains your correctness model without making queue depth a function of epoch timing.

---

# What I would build for dios

Given everything you've shown me, my v1 architecture would be approximately:

```rust
pub fn prefetch(&self, pages: &[PageId]) -> PrefetchReport;
```

Internally:

```rust
struct PrefetchAdmission {
    max_obligations: usize,
    obligations: AtomicUsize,
}
```

where an obligation means either:

```text
prefetch read in-flight
OR
resident but not yet consumed
```

Frame metadata:

```rust
struct FrameMeta {
    page: Option<PageId>,
    residency: Residency,
    admission: AdmissionClass,
    // existing pin/epoch/policy metadata
}
```

with:

```rust
enum AdmissionClass {
    Demand,
    PrefetchUnconsumed,
}
```

Miss metadata:

```rust
struct PendingRead {
    page: PageId,
    frame: FrameId,
    origin: ReadOrigin,
    // completion / waiter state
}
```

And transitions:

```text
PREFETCH

Absent
  │
  ├─ no speculative credit ─────────> Deferred
  │
  └─ reserve credit
        │
        ├─ no ready frame ──────────> Deferred + return credit
        │
        └─ frame
             ↓
           Loading[prefetch]
             ↓
           Resident[prefetch-unconsumed]


DEMAND GET

Resident[prefetch-unconsumed]
  ↓
promote
  ↓
Resident[demand]
  ↓
return speculative credit


Demand races in-flight prefetch
  ↓
join miss
  ↓
mark useful/promoted
  ↓
on completion Resident[demand]


EVICTION

Resident[prefetch-unconsumed]
  ↓
highest-priority clean victim
  ↓
return speculative credit
```

And separately:

```text
reclaimer
  ↓
maintains reusable-frame reserve
```

so the `prefetch()` fast path never performs expensive victim discovery.

---

# What I would *not* build

I would avoid all of these for v1:

```text
❌ StageTicket / PrefetchTicket
❌ ReaderCtx on prefetch()
❌ plan objects inside dios
❌ separate permanent prefetch frame allocation
❌ FrameState::Prefetched
❌ cancellation semantics
❌ per-prefetch futures
❌ Pin-heavy public types
❌ io_uring-specific types above the driver seam
❌ registered buffers being mandatory
```

Each one adds ontology without addressing the thing your benchmark actually found.

The one piece of extra API I **would** seriously consider is:

```rust
pub fn residency(&self, pages: &[PageId], out: &mut BitSet)
```

or some batch equivalent.

Not necessarily public initially.

Because your consumer-side trim-on-resident policy really does want a cheap, **non-touching** residency observation. Calling `get()` to discover residency contaminates recency/pinning semantics, exactly as you suspected.

---

# The broader 2026 direction

One interesting wrinkle in the research is that the field isn't converging exclusively on “classic userspace hash-table buffer pool + direct I/O.”

There is renewed work on VM-assisted buffer management. `vmcache` uses virtual-memory translation while retaining DBMS control over eviction/I/O, and 2026 work extends that idea to tiered memory. ([ibr.cs.tu-bs.de][10])

There is also brand-new 2026 work on array-based translation (`calico`) intended to retain DBMS-controlled I/O/eviction while reducing translation overhead across OLTP, scans, and vector workloads. ([arXiv][11])

So I would avoid baking the assumption:

> “A modern buffer pool means a traditional page-table/hash lookup forever.”

But that's largely orthogonal to your immediate frame-pressure problem.

Your current architecture can evolve toward a different translation scheme later without changing the clean seam:

```text
PageId → residency
prefetch(PageIds)
get(PageId)
```

And that's another argument for keeping `prefetch()` deliberately boring.

---

# My strongest recommendation

I'd phrase the design invariant like this:

> **Demand and speculative reads share page identity and physical frame storage, but not admission authority. Speculation is bounded by explicit credits spanning in-flight through resident-unconsumed state; consumption promotes the frame into ordinary cache semantics and releases the credit. Frame reclamation prepares reusable capacity ahead of admission rather than lying on the I/O submission critical path.**

That feels both **modern systems-design-wise** and **Rust-friendly**.

And it maps almost perfectly onto the experiment you just ran:

```text
5.5× proves concurrency is valuable
0.93× proves frame admission destroys concurrency
→ preserve concurrency by making speculative capacity explicit
```

Rather than adding sophistication around `get()`, you're fixing exactly the layer the experiment identified.

One final thing I'd measure before freezing this architecture: **whether a tiny reserved speculative inventory—say 8/16/32/64 frames—is sufficient to flip your pressured gate.** If 16 frames recover nearly all of the 5.5× win, that is strong evidence for credit/headroom semantics rather than a wholesale buffer-manager redesign. If you want, I can next do a more focused research pass specifically on **free-frame production/reclamation algorithms** (LeanStore cooling, CLOCK variants, SIEVE, PostgreSQL rings, asynchronous eviction) and turn that into a candidate state machine for dios.

I can also keep an eye on new buffer-pool/io_uring papers and surface anything that materially changes this design.

[1]: https://www.postgresql.org/docs/current/runtime-config-resource.html?utm_source=chatgpt.com "Documentation: 18: 19.4. Resource Consumption"
[2]: https://arxiv.org/abs/2512.04859?utm_source=chatgpt.com "High-Performance DBMSs with io_uring: When and How to use it"
[3]: https://www.cs.cit.tum.de/dis/research/leanstore/?utm_source=chatgpt.com "LeanStore - Decentralized Information Systems and Data Management"
[4]: https://docs.rs/io-uring/latest/io_uring/struct.Submitter.html?utm_source=chatgpt.com "Submitter in io_uring - Rust"
[5]: https://db.in.tum.de/~leis/papers/leanstore.pdf?utm_source=chatgpt.com "LeanStore: In-Memory Data Management"
[6]: https://docs.rs/tokio-uring/latest/tokio_uring/buf/trait.IoBuf.html?utm_source=chatgpt.com "IoBuf in tokio_uring::buf - Rust"
[7]: https://doc.rust-lang.org/stable/std/alloc/struct.Layout.html?utm_source=chatgpt.com "Layout in std::alloc - Rust"
[8]: https://man7.org/linux/man-pages/man3/io_uring_register_buf_ring.3.html?utm_source=chatgpt.com "io_uring_register_buf_ring(3) - Linux manual page"
[9]: https://wiki.postgresql.org/wiki/AIO?utm_source=chatgpt.com "AIO - PostgreSQL wiki"
[10]: https://ibr.cs.tu-bs.de/vss/Publications/2023/leis_23_sigmod.pdf?utm_source=chatgpt.com "Virtual-Memory Assisted Buffer Management"
[11]: https://arxiv.org/abs/2604.00423?utm_source=chatgpt.com "Making Array-Based Translation Practical for Modern, High-Performance Buffer Management"

