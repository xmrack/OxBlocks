# oxblocks

Oxide Blocks is a Monero block explorer for `monerod`, written in Rust.

Server-rendered HTML plus a JSON API. No JavaScript, no cookies, no external assets,
no database.

## Why this exists

The established explorer, [onion-monero-blockchain-explorer][onion], is not an RPC
client — it is effectively a second Monero node. `src/MicroCore.h` embeds
`cryptonote::Blockchain` and `tx_memory_pool` in-process, links the Monero C++ core,
and opens monerod's LMDB directly. Several of its routes feed **user-supplied hex**
into those C++ deserializers, in the same address space as the chain database.

oxblocks takes the opposite position:

```
┌──────────────┐   HTTP/JSON   ┌──────────────┐   LMDB   ┌──────────┐
│   oxblocks   │ ─────────────>│   monerod    │ ────────>│ data.mdb │
│ axum+askama  │  loopback RPC │ (unmodified) │          └──────────┘
└──────────────┘               └──────────────┘
```

The explorer holds no keys, opens no database, and links no C++. A bug in the web
layer costs a response, not chain state.

## Design decisions

These were chosen deliberately. Each has a cheaper alternative that was rejected.

**RPC only, never the database.** Reading monerod's LMDB directly would be faster and
is what the C++ explorer does. It also means reimplementing a schema that changes
between releases, over a memory-mapped file a live daemon is writing. The process
boundary is the entire point of the project — treat it as load-bearing.

**Stateless.** No database, no background indexer, no disk state. Restarts are always
clean and there is no reorg-repair path to get wrong. The price is that anything
needing a cross-block aggregate — notably emission totals — is out of scope.

The one thing the process remembers is a bounded in-memory cache, and losing it
costs latency rather than correctness. Objects keyed by **hash** are cached
freely, because a hash names one object forever. Objects keyed by **height** are
cached only once buried deeper than `REORG_WINDOW` (60 blocks), because a reorg
reassigns a height to a different block. `/health` reports occupancy and hit
counts.

**No view-key or transaction-pusher features.** The C++ explorer offers `/myoutputs`,
`/prove`, `/rawtx` and `/checkandpush`. Those require users to paste a secret view key
into a server that could log it, and they were the only routes feeding attacker-supplied
hex into a deserializer. Omitting them removes the threat class rather than reimplementing
it more carefully.

**Onion-compatible JSON.** The `/api/*` responses match the C++ explorer field-for-field
where the endpoint exists in both, so existing consumers migrate without changes — and so
the two can be differentially tested against the same chain.

## Memory safety, stated honestly

Every crate in this workspace sets `#![forbid(unsafe_code)]`, enforced at compile time
rather than by review.

That covers **our** code. It does not cover the dependency tree: transitive crates
contain `unsafe`, and claiming otherwise would be false. What the project actually
guarantees is narrower and worth stating plainly — no consensus code, no database
handle, and no C++ in the web process, and no `unsafe` in the code we wrote.

The tree is **108 third-party crates**, of which 6 are proc-macros. That is ordinary
for an async HTTP service and it is not small in absolute terms; quoting the number
is more useful than calling it lean. Count it yourself with `cargo tree --workspace
-e normal`, deduplicated by name and version. Three things hold it in place:

* `deps-baseline.txt` lists every crate in the tree, and `tools/check-deps.sh` fails
  in CI when one enters or leaves without that file being updated in the same commit.
  It compares names rather than versions, so routine upgrades stay quiet.
* `cargo deny` runs in CI over advisories, licences, duplicate versions and source
  registries, and denies unmaintained crates outright.
* `monerod-rpc` speaks to the daemon through `hyper` rather than `reqwest`. reqwest
  enables `tower-http/follow-redirect`, which Cargo unifies across the workspace and
  which pulls `url` &rarr; `idna` &rarr; ~25 ICU crates of Unicode tables &mdash; all
  of it carried so the client could then set its redirect policy to `none`. Dropping
  it removed 28 crates and added none.
* `monerod-rpc` on its own builds without the `tls` feature, dropping 12 crates. The
  `oxblocks` binary always links TLS: `explorer-web` depends on `monerod-rpc` with
  default features and exposes no way to turn it off, which is deliberate — a
  production explorer reaching a remote daemon should not need a flag to get
  encryption.

`.github/workflows/ci.yml` also runs `tools/check-unsafe.sh`, which fails if any
crate stops inheriting the workspace lint. It is checked in a state where removing
`workspace = true` from one crate makes it exit non-zero — a gate that cannot fail
is not a gate.

`release` builds keep `overflow-checks` on. Explorer arithmetic is chain-derived u64s
(amounts, ring offsets, heights); a silent wrap is a wrong number on a page.

## Layout

| Crate | Role |
| --- | --- |
| `monerod-rpc` | Typed async monerod RPC client. The only crate that touches the network. |
| `explorer-core` | Domain model, chain access over RPC, `tx_extra` decoding, caching. |
| `explorer-web` | axum routes, askama templates, JSON API. Builds the `oxblocks` binary. |

The split is enforced by the compiler: `explorer-core` cannot depend on the web
framework, and `monerod-rpc` cannot depend on either.

## Requirements

monerod's **unrestricted** RPC. Under `--restricted-rpc` monerod blocks
`/get_transaction_pool`, `get_alternate_chains`, `get_coinbase_tx_sum` and
`/get_alt_blocks_hashes`, which removes the mempool and alt-block pages. Point oxblocks
at a loopback daemon (`127.0.0.1:18081`), not a public restricted port.

Pruned nodes are supported. Ring-member expansion is unaffected by pruning because the
output table is never pruned; what is unavailable is full raw transaction hex and
signature-level detail for most historical transactions, which render as a "pruned"
state rather than an error.

**Daemon versions.** Tested against monerod `master` and against released v0.18.x, on
the same chain, with identical output. master matters on its own: it removed the three
bootstrap-daemon fields from `get_info` in `a01b4c2a3` (2026-05-31), and an explorer
that requires them cannot talk to a current daemon at all. Every `get_info` field
oxblocks does not act on is optional for that reason.

## Bandwidth

**Transactions are never fetched pruned.** monerod will send the prefix and RingCT base
alone, which is everything a *summary* needs and about 29% of the bytes — on mainnet
block 3,708,864, 134 transactions are 2,175,016 bytes whole and 639,738 pruned. oxblocks
asks for them whole anyway. What comes back pruned is not the transaction that was
broadcast, its size is not the size anyone means, and an explorer that quietly served a
shortened copy would be lying about what the chain holds. `get_transactions` here is
always `prune: false`, and the flag is private with no way to set it.

What is done instead is **compressing the response to the reader**. Negotiated per
request — gzip, deflate and brotli, only for a client that sends `Accept-Encoding`. A
thousand-transaction anonymity set goes from 615,010 bytes to 206,802 gzipped and
189,703 brotli, **69% less**, and the whole transaction is still in it. zstd is
deliberately absent: that crate wraps the C library.

monerod itself never compresses — asked with `Accept-Encoding: gzip, deflate, br, zstd`
it answers with the same uncompressed bytes — so there is nothing to negotiate on the
daemon link.

## API

```
/api/block/<height|hash>              /api/transactions?page=&limit=
/api/transaction/<hash>               /api/mempool?page=&limit=
/api/rawblock/<height|hash>           /api/search/<height|hash>
/api/rawtransaction/<hash>            /api/networkinfo
/api/version                          /api/feeestimate?grace_blocks=

/api/blocks/<start>/<end>             k-anonymous block lookup
/api/transaction/private/<postfix>    k-anonymous transaction lookup
/api/transactions/recent
```

A running explorer documents its own API at **`/api`** — the same list, with
every parameter and limit, the response shapes, and two things a static page
cannot state: which postfix lengths *this* chain currently accepts, and whether
*this* daemon can serve the k-anonymous lookup at all. The limits on that page
are interpolated from the constants the handlers enforce, and a test fails the
build if a route is added without being documented or a cap is changed without
the page following it.

### Which upstream this tracks

The C++ explorer's `devel` and `master` branches have genuinely diverged — 44
commits each way — so "upstream compatible" needs saying precisely. oxblocks
takes what is better from each:

| from | what |
| --- | --- |
| devel | `unlock_time` on every transaction object |
| devel | `/api/blocks/<start>/<end>`, `/api/transaction/private/<postfix>`, `/api/transactions/recent` |
| master | `/api/feeestimate`, which devel does not have |

Compatibility is verified by `tests/upstream_compat.rs` against captures from a
real devel build **serving the same chain** as the daemon those captures came
from. Because both sides are one chain at one moment, nothing is tip-relative:
the comparison is exact, with no excused fields.

### k-anonymity

`/api/transaction/private/<postfix>` returns every transaction whose hash ends
with a given hex postfix. The caller picks the one it wanted locally, so the
explorer never learns which. `/api/blocks/<start>/<end>` does the same for
blocks: ask for a range, take the one you meant.

A postfix must be 2–12 characters, hex, and name a set this explorer can both
hide a transaction in and afford to serve. Both ends are expressed in *expected
matches* rather than in characters, so they scale with the chain: at least 20,
and at most 1000.

The floor is upstream's and it is the privacy property. Each extra character
divides the expected set by sixteen, and the count of actual matches is Poisson
around the expected one, so a floor of 2 would return a single transaction 40%
of the time, which is no anonymity at all.

The ceiling is ours. `get_txids_loose` walks the whole transaction index, so a
two-character postfix on mainnet is a full-index scan answering with about a
quarter of a million hashes — ten megabytes off the daemon, for a set far larger
than this explorer will expand, so the request ends in a refusal either way.
Refusing on the arithmetic costs the daemon nothing.

Which lengths that leaves depends on the chain. Mainnet today holds about
66,000,000 transactions counting coinbases, which admits five characters — 63
expected matches — and refuses four at 1,008. That four is only just outside
the band, so a slightly smaller chain would have admitted it too; the rule is
the expected count, not the length.

**This endpoint needs a daemon with `get_txids_loose`**, which is in monerod
`master` and `release-v0.19` but **in no release build** — v0.18.x answers
`Method not found`. oxblocks probes for it at startup and says so in the log;
where it is absent that one endpoint refuses and everything else, including
`/api/blocks`, works normally.

`/api/blocks` is capped at 100 blocks per request. Upstream imposes no cap
because it reads its own database; every block here costs two RPC calls, so an
uncapped range would let one request make millions of calls against the
operator's daemon.

Responses are byte-compatible with the C++ explorer: the same JSend-ish
envelope, always HTTP 200, alphabetically sorted keys, and the same field names
and types — including the details that are easy to get wrong, such as `mixin`
being the ring size, `inputs` being `null` rather than `[]` on a coinbase, and
block `size` being an integer in `/api/block` but a float in
`/api/transactions`.

The view-key and pusher endpoints (`/api/outputs`, `/api/outputsblocks`) are
deliberately absent; see above. `/api/emission` is out because it needs either a
background scanner or a full-chain scan, and this explorer is stateless.

### One known difference, on a pruned node

`tx_size` under-reports for any transaction outside the node's kept stripe. A
pruned daemon keeps only the transaction prefix, so we report 335 bytes where
upstream reports 1970 — and 335 plus the discarded 1635-byte prunable half is
exactly 1970. The bytes are not on the node; no rendering choice recovers them.

This is accepted rather than worked around. Run against an unpruned daemon if
`tx_size` must match upstream exactly for historical transactions. Every other
field matches on a pruned node, because ring expansion reads the output table,
which is never pruned.

## Web interface

Server-rendered pages at `/`, `/page/<n>`, `/block/<height|hash>`,
`/tx/<hash>`, `/mempool`, `/altblocks`, `/search` and `/api`.

No JavaScript, no cookies, no images, no web fonts, no external requests of any
kind. One stylesheet, compiled into the binary, so there is no asset directory
to deploy. The page follows the reader'''s light/dark preference through
`prefers-color-scheme` and is usable on a phone.

Markup is generated by [askama](https://github.com/askama-rs/askama), which
escapes every interpolation at compile time. That is the structural answer to
the class of bug that makes upstream'''s 7,178-line `page.h` risky: there,
markup is assembled by string concatenation, so a missed escape is invisible.
Here, emitting a value unescaped requires writing `|safe`, which greps.

## Status

The JSON API and the web interface are both complete. Alt-block and emission
pages are out of scope, as described above.

## Testing

Three layers:

1. **Unit and property tests**, plus a fuzz target for `tx_extra` — the one parser this
   project owns, and the one place attacker-influenced lengths meet our code.
2. **Fixture replay** against captured real RPC responses (`fixtures/`), so CI needs no node.
3. **Differential testing** against the C++ explorer on a shared chain, asserting the
   `/api/*` responses agree.

Live tests are `#[ignore]`d by default:

```bash
OXBLOCKS_TEST_RPC=http://127.0.0.1:28081 cargo test -- --ignored
```

## Deploying

`deploy/oxblocks.service` is a hardened systemd unit. The explorer holds no keys,
opens no database and writes nothing, so nearly every capability is removed: an
empty `CapabilityBoundingSet`, `ProtectSystem=strict` with no `ReadWritePaths`,
`MemoryDenyWriteExecute=yes` (Rust generates no code at runtime), a
`@system-service` syscall filter, and `RestrictAddressFamilies=AF_INET AF_INET6`
with `IPAddressAllow=localhost`. Widen the address rules only to the specific
hosts your daemon and proxy use.

`Dockerfile` builds a distroless image running as `nonroot`, with no shell and no
package manager. The stylesheet is compiled into the binary, so there is no asset
directory to mount.

Put a TLS-terminating reverse proxy in front of it. oxblocks speaks plain HTTP by
design; terminating TLS is a job with its own large attack surface and it does not
belong in the same process as the explorer.

## License

MIT. See [LICENSE](LICENSE).

Repository: <https://github.com/xmrack/oxblocks>

[onion]: https://github.com/moneroexamples/onion-monero-blockchain-explorer
