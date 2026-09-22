# oxblocks

oxblocks is a Monero block explorer written in Rust.

The explorer does not open the blockchain database. It holds no keys. It writes
nothing to disk. For each request it asks the daemon over RPC and renders the answer.

The JSON API serves the same response bodies as
[xmrblocks](https://github.com/moneroexamples/onion-monero-blockchain-explorer),
so a client that reads the body works without changes. It differs on two
points, both listed under [JSON API](#json-api).

## Requirements

* A `monerod` node with **unrestricted** RPC. A restricted daemon blocks the
  calls behind the mempool and alt-chain pages.
* Rust 1.88 or later, to build from source. Use Docker instead if you prefer.

Point the explorer at a local daemon such as `http://127.0.0.1:18081`. Do not
point it at a public restricted port.

Pruned nodes work. Read [Pruned nodes](#pruned-nodes) for the one field that
differs.

## Build and run

```bash
git clone https://github.com/xmrack/oxblocks
cd oxblocks
cargo build --release
./target/release/oxblocks --daemon-url http://127.0.0.1:18081
```

The explorer listens on `127.0.0.1:8081`. Open `http://127.0.0.1:8081/` in a
browser.

To serve other machines, bind to an address they can reach:

```bash
./target/release/oxblocks --bind 0.0.0.0:8081 --daemon-url http://127.0.0.1:18081
```

### Options

Each option also reads an environment variable. The command line wins.

| Option | Variable | Default | Purpose |
| --- | --- | --- | --- |
| `--daemon-url` | `OXBLOCKS_DAEMON_URL` | `http://127.0.0.1:18081` | The monerod RPC address. |
| `--bind` | `OXBLOCKS_BIND` | `127.0.0.1:8081` | The address to listen on. |
| `--theme` | `OXBLOCKS_THEME` | `auto` | Colour scheme. Use `auto`, `light` or `dark`. |
| `--rpc-timeout-secs` | `OXBLOCKS_RPC_TIMEOUT` | `30` | Limit for one RPC call. |
| `--request-timeout-secs` | `OXBLOCKS_REQUEST_TIMEOUT` | `25` | Limit for one inbound request. |
| `--max-concurrent` | `OXBLOCKS_MAX_CONCURRENT` | `128` | Requests handled at the same time. |
| `--max-inflight-rpc` | `OXBLOCKS_MAX_INFLIGHT_RPC` | `24` | RPC calls open at the same time. |
| `--max-body-bytes` | `OXBLOCKS_MAX_BODY` | `8192` | Largest accepted request body. |
| `--log` | `OXBLOCKS_LOG` | `info` | Log filter, such as `oxblocks=debug`. |

Run `oxblocks --help` for the full text of each option.

## Run in Docker

```bash
docker build -t oxblocks .
docker run --rm -p 8081:8081 oxblocks \
  --bind 0.0.0.0:8081 --daemon-url http://DAEMON-HOST:18081
```

The image is distroless. It runs as the `nonroot` user. It holds one binary and
has no shell and no package manager. The stylesheet is compiled into the binary,
so you do not need to mount an asset directory.

Bind to `0.0.0.0` inside the container. The default of `127.0.0.1` is not
reachable from outside it.

To reach a daemon on the Docker host:

* **Linux.** Share the host network instead of publishing a port:

  ```bash
  docker run --rm --network host oxblocks \
    --bind 127.0.0.1:8081 --daemon-url http://127.0.0.1:18081
  ```

* **Docker Desktop.** Keep `-p 8081:8081` and use
  `--daemon-url http://host.docker.internal:18081`.

## Run as a service

`deploy/oxblocks.service` is a hardened systemd unit.

```bash
sudo install -m755 target/release/oxblocks /usr/local/bin/oxblocks
sudo install -m644 deploy/oxblocks.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now oxblocks
```

The unit runs the explorer under a dynamic user with no capabilities, a
read-only filesystem, no writable paths, no executable memory, and a syscall
filter. It allows IPv4 and IPv6 sockets to localhost only. Widen
`IPAddressAllow` only to the hosts that your daemon and proxy use.

Put a TLS reverse proxy in front of the explorer. oxblocks serves plain HTTP.

## Web interface

| Path | Page |
| --- | --- |
| `/` and `/page/<n>` | Recent blocks. |
| `/block/<height or hash>` | One block and the transactions in it. |
| `/tx/<hash>` | One transaction, with inputs, outputs and ring member ages. |
| `/mempool` | Transactions that wait to be mined. Click a column heading to sort. |
| `/altblocks` | Alternative chains that the daemon knows about. |
| `/search?q=` | Find a block or a transaction. |
| `/api` | API documentation for this deployment. |
| `/health` | Liveness and cache counters, as JSON. |

The server renders every page. There is no JavaScript, no cookie, no image, no
web font and no external request. One stylesheet ships inside the binary.

`--theme auto` follows the light or dark setting of each reader. `--theme light`
and `--theme dark` fix one palette for everyone.

## JSON API

Every response is wrapped. Keys are sorted alphabetically.

```json
{"data": { ... }, "status": "success"}
{"data": {"title": "Cant parse tx hash: deadbeef"}, "status": "fail"}
{"data": null, "message": "...", "status": "error"}
```

`fail` means the caller asked for something this explorer will not answer.
`error` means the explorer or its daemon could not answer.

The HTTP status says the same thing.

| Code | Meaning |
| --- | --- |
| 200 | The answer is in `data`. |
| 400 | The argument is not one this explorer reads, or a limit is over its cap. |
| 404 | The argument was well formed and the chain does not hold it. |
| 500 | A bug here. |
| 502 | The daemon could not be reached, or answered with something unusable. |
| 503 | This deployment cannot serve the endpoint, because of how its daemon is built or configured. |

Arguments are read as given. A height is decimal digits, a hash is 64 hex
characters of either case, and a postfix is hex. Anything else is a 400, and so
is a `page` or `limit` that is not a plain number.

Those two points are where the API departs from xmrblocks, which answers 200 to
everything and deletes the characters it does not recognise before it parses.

| Endpoint | Returns |
| --- | --- |
| `/api/block/<height or hash>` | One block with its transactions. |
| `/api/transaction/<hash>` | One transaction, with rings expanded. |
| `/api/rawblock/<height or hash>` | The block as the daemon holds it. |
| `/api/rawtransaction/<hash>` | The transaction as the daemon holds it. |
| `/api/transactions?page=&limit=` | Transactions by block, newest first. `limit` is at most 50. |
| `/api/mempool?page=&limit=` | Transactions in the mempool. `limit` is at most 500. |
| `/api/search/<height or hash>` | A block or a transaction, whichever matches. |
| `/api/networkinfo` | Height, difficulty, hash rate and peer counts. |
| `/api/feeestimate?grace_blocks=` | The current fee per byte. |
| `/api/version` | The explorer version and the daemon version. |
| `/api/blocks/<start>/<end>` | A range of blocks. 100 blocks at most. |
| `/api/transaction/private/<postfix>` | Every transaction whose hash ends with the postfix. |
| `/api/transactions/recent` | The mempool plus the last 30 blocks. |

A running explorer documents its own API at `/api`. That page lists each
parameter and each limit. It also states two facts that no static page can
state: which postfix lengths this chain accepts now, and whether this daemon can
serve the private lookup at all.

Endpoints that need a view key, a secret or raw transaction hex do not exist
here. Neither does an emission total, because that needs a full chain scan and
this explorer keeps no index.

### Private lookups and k-anonymity

Two endpoints let a caller fetch data without naming what it wants. The caller
hides in a set, and the explorer cannot tell which member of the set the caller
wanted.

`/api/transaction/private/<postfix>` returns every transaction whose hash ends
with the hex postfix that you give. The caller picks the one it wants on its own
machine. `/api/blocks/<start>/<end>` does the same for blocks. Ask for a range
and keep the block you meant.

A postfix is 2 to 12 hex characters. The explorer also checks the postfix
against the size of the chain, and accepts it only when it expects 20 to 1000
matches. Both bounds count transactions, not characters, so the lengths that
qualify change as the chain grows. On mainnet today, 5 characters qualify.

The lower bound is the privacy rule. Each added character divides the expected
set by 16. Real match counts vary around the expected count, so a small set
often returns one transaction and hides nothing.

The upper bound protects the daemon. The daemon walks its whole transaction
index to answer, and a short postfix makes it return hundreds of thousands of
hashes. The explorer refuses that before it makes the call.

`/api/blocks` serves 100 blocks at most, because each block costs two RPC calls.

This lookup needs a daemon that has `get_txids_loose`. monerod `master` and
`release-v0.19` have it. No released build has it, and v0.18.x answers
`Method not found`. The explorer tests for it at startup and writes the result
to the log. When the call is missing, this one endpoint refuses and everything
else works.

## Architecture

```
┌──────────────┐   HTTP/JSON   ┌──────────────┐   LMDB   ┌──────────┐
│   oxblocks   │ ────────────> │   monerod    │ ───────> │ data.mdb │
│              │      RPC      │ (unmodified) │          └──────────┘
└──────────────┘               └──────────────┘
```

| Crate | Role |
| --- | --- |
| `monerod-rpc` | Typed async RPC client. The only crate that touches the network. |
| `explorer-core` | Domain types, chain access, `tx_extra` decoding, caching. |
| `explorer-web` | Routes, templates and the JSON API. Builds the `oxblocks` binary. |

The compiler holds the split in place. `explorer-core` cannot use the web
framework, and `monerod-rpc` cannot use either of the other two crates.

The process keeps one thing in memory: a bounded cache. It caches an object
named by hash at once, because a hash names one object forever. It caches an
object named by height only when that height is more than 60 blocks deep,
because a reorg gives a height to a different block. Losing the cache costs
speed, not correctness. `/health` reports the size and the hit counts.

## Security design

**No unsafe code.** Every crate in this repository sets
`#![forbid(unsafe_code)]`, and the compiler enforces it. This covers the code
here. It does not cover the dependency tree, where some crates do use `unsafe`.
`tools/check-unsafe.sh` fails the build when a crate stops inheriting the rule.

**Escaped output.** Templates escape every value at compile time. To emit a raw
value a developer must write `|safe`.

**A strict browser policy.** Every response carries
`Content-Security-Policy: default-src 'none'; style-src 'self'; form-action
'self'; base-uri 'none'; frame-ancestors 'none'`, plus `nosniff`,
`Referrer-Policy: no-referrer` and `X-Frame-Options: DENY`. The middleware adds
these headers outside the router, so a timeout or a rejected body carries them
too.

**Backpressure.** Each page costs RPC calls, so a traffic spike could otherwise
overload the daemon. The explorer caps the requests it handles at once and the
RPC calls it opens at once. It also gives every inbound request a deadline that
is shorter than the RPC deadline.

**Checked arithmetic.** Release builds keep `overflow-checks` on. Chain values
are 64-bit integers, and a silent wrap would put a wrong number on a page.

**A controlled dependency tree.** `deps-baseline.txt` lists every third-party
crate, and `tools/check-deps.sh` fails the build when a crate enters or leaves
without an update to that file. `cargo deny` runs in CI over advisories,
licences, duplicate versions and source registries. Read the tree yourself with
`cargo tree --workspace -e normal`.

## Pruned nodes

On a pruned daemon, `tx_size` under-reports for a transaction outside the stripe
that the node keeps. The node holds only the prefix, so the missing bytes are
not there to count. Run an unpruned daemon if you need that field to be exact.
Every other field is correct on a pruned node, because ring expansion reads the
output table, and the daemon never prunes that table.

## Testing

```bash
cargo test --workspace
```

That needs no node, because `fixtures/` holds captured RPC responses to replay.
Four layers cover the code:

1. Unit tests in each crate.
2. Fixture replay of real RPC responses.
3. Differential tests that compare the `/api/*` output against captured
   xmrblocks output from the same chain.
4. `tools/txextra-oracle`, which compares the `tx_extra` parser against a C++
   oracle on random input. Run it by hand, not in CI.

Live tests need a node and stay off by default:

```bash
OXBLOCKS_TEST_RPC=http://127.0.0.1:28081 cargo test -- --ignored
```

## License

MIT. Read [LICENSE](LICENSE).
