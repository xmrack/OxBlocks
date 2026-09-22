# oxblocks

Oxide Blocks is a Monero block explorer for `monerod`, written in Rust.

Server-rendered HTML plus a JSON API. No JavaScript, no cookies, no external assets,
no database.

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

`/api/blocks` is capped at 100 blocks per request. Every block here costs two RPC calls, so an
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

## Testing

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
