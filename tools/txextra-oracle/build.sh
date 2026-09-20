#!/bin/sh
# Differential-test oracle for cryptonote::parse_tx_extra.
# oracle.cpp / batch.cpp include the REAL monero serialization headers and contain a
# verbatim copy of the parse_tx_extra loop (src/cryptonote_basic/cryptonote_format_utils.cpp:564).
# No monerod libraries are linked; only headers + system boost are needed.
set -e

# Point MONERO at a monerod source checkout. Headers only -- nothing from it is
# linked, so it does not need to have been built, except for the generated
# include directory below which a configured build produces.
MONERO=${MONERO:?set MONERO to a monerod source checkout, e.g. MONERO=~/src/monero ./build.sh}
for t in oracle batch; do
  g++ -std=c++17 -O1 -fsanitize=address,undefined -g -o "$t" "$t.cpp" \
    -I"$MONERO/src" -I"$MONERO/contrib/epee/include" \
    -I"$MONERO/external/easylogging++" -I"$MONERO/build/generated_include"
done
# usage: ./oracle <hex>          -> pretty-print one blob
#        ./batch < hexlines      -> one result line per input line
#        python3 difffuzz.py <seed> <n>   -> differential fuzz model.py vs batch
#        python3 exhaustive.py            -> all 1/2-byte + tag-led 3-byte inputs
#        python3 regen_corpus.py [--write] -> re-derive the Rust corpus expectations
#        OXBLOCKS_TXEXTRA_DIFF_SEEDS=2000 cargo test -p explorer-core --lib \
#            differential_against_the_cpp_oracle -- --ignored --nocapture
#                                         -> 50M-input soak, Rust vs this binary
#
# batch's line grammar -- kept in lockstep with `oracle_line` in
# crates/explorer-core/src/tx_extra.rs and with `fmt` in model.py, because a
# differential is only as strong as the weaker of the two renderings:
#
#   OK|FAIL n=<fields> consumed=<pos>/<len> tags=<field>,<field>,...
#
#     P<size>                padding; size includes the tag byte, and padding
#                            has no payload, so the size IS the value
#     K:<64 hex>             public key
#     N<len>:<hex>           nonce
#     M:<depth>:<64 hex>     merge mining: decimal depth, then merkle root
#     A<n>:<64 hex * n>      additional public keys, concatenated
#     G<len>:<hex>           mysterious minergate (0xDE)
#
# Every payload byte appears, so a parser that gets the framing right but a
# value wrong -- a depth truncated to 32 bits, a transposed key -- shows up as a
# mismatch. Counting elements instead would not.
