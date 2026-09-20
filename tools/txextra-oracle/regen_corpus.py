# Regenerate the expected-output strings of SYNTHETIC_CORPUS / REAL_CORPUS in
# crates/explorer-core/src/tx_extra.rs from ./batch, so those expectations stay
# the C++ oracle's own output rather than a guess at it.
#
#   python3 tools/txextra-oracle/regen_corpus.py            # check only
#   python3 tools/txextra-oracle/regen_corpus.py --write    # rewrite in place
import os, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
RUST = os.path.join(HERE, '..', '..', 'crates', 'explorer-core', 'src', 'tx_extra.rs')
WIDTH = 100


def literals(text, start, end):
    """Every Rust string literal in text[start:end] as (lo, hi, value)."""
    out = []
    i = start
    while i < end:
        if text[i] != '"':
            i += 1
            continue
        lo = i
        i += 1
        val = []
        while text[i] != '"':
            if text[i] == '\\':
                nxt = text[i + 1]
                if nxt == '\n':
                    # Rust's line-continuation escape: newline plus all the
                    # leading whitespace that follows it disappears.
                    i += 2
                    while text[i] in ' \t':
                        i += 1
                    continue
                val.append({'n': '\n', 't': '\t', '\\': '\\', '"': '"', '0': '\0'}[nxt])
                i += 2
                continue
            val.append(text[i])
            i += 1
        i += 1
        out.append((lo, i, ''.join(val)))
    return out


def wrap(value, col):
    """A Rust literal for `value`, broken so no line exceeds WIDTH columns.

    Only breaks inside the tags= payload, which holds no spaces: a break
    elsewhere would swallow a significant space, since the continuation escape
    eats the leading whitespace of the next line.
    """
    head, sep, tail = value.partition('tags=')
    prefix = head + sep
    room = WIDTH - col - 1            # -1 for the opening quote
    if col + len(value) + 2 <= WIDTH:
        return '"%s"' % value
    lines = []
    cur = prefix
    for ch in tail:
        if len(cur) + 1 > room - 1:   # -1 for the trailing backslash
            lines.append(cur)
            cur = ''
            room = WIDTH - (col + 1)
        cur += ch
    lines.append(cur)
    pad = ' ' * (col + 1)
    return '"' + ('\\\n' + pad).join(lines) + '"'


def main():
    text = open(RUST).read()
    edits = []
    pairs = []
    for name in ('SYNTHETIC_CORPUS', 'REAL_CORPUS'):
        start = text.index('const %s:' % name)
        end = text.index('\n    ];', start)
        lits = literals(text, start, end)
        assert len(lits) % 2 == 0, name
        for k in range(0, len(lits), 2):
            hexlit, explit = lits[k], lits[k + 1]
            pairs.append((hexlit[2], explit))

    proc = subprocess.run([os.path.join(HERE, 'batch')],
                          input='\n'.join(h for h, _ in pairs) + '\n',
                          capture_output=True, text=True)
    lines = proc.stdout.splitlines()
    assert len(lines) == len(pairs), (len(lines), len(pairs))

    stale = 0
    for (h, explit), fresh in zip(pairs, lines):
        lo, hi, old = explit
        if old != fresh:
            stale += 1
            print('UPDATE %s\n  old: %s\n  new: %s' % (h[:60], old, fresh))
        col = lo - text.rindex('\n', 0, lo) - 1
        edits.append((lo, hi, wrap(fresh, col)))

    print('%d entries, %d stale' % (len(pairs), stale))
    if '--write' in sys.argv:
        for lo, hi, new in sorted(edits, reverse=True):
            text = text[:lo] + new + text[hi:]
        open(RUST, 'w').write(text)
        print('wrote', RUST)


main()
