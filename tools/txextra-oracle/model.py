# Model of cryptonote::parse_tx_extra, written from the C++ source.
# Returns (ok, fields) where fields is a list of tuples (partial results kept on failure).
MAXVARINT_BITS = 64

def read_varint(buf, pos):
    """returns (status, nread, value, newpos). status in {'ok','eofpartial','overflow','represent'}"""
    read = 0; write = 0; shift = 0
    while True:
        if pos >= len(buf):
            return ('eofpartial' if read else 'empty', read, write, pos)
        byte = buf[pos]; pos += 1; read += 1
        if shift + 7 >= MAXVARINT_BITS and byte >= (1 << (MAXVARINT_BITS - shift)):
            return ('overflow', -1, write, pos)
        if byte == 0 and shift != 0:
            return ('represent', -2, write, pos)
        write |= (byte & 0x7f) << shift
        if (byte & 0x80) == 0:
            return ('ok', read, write, pos)
        shift += 7

def parse(extra):
    fields = []
    if len(extra) == 0:
        return (True, fields)
    pos = 0
    n = len(extra)
    while True:
        # read_variant_tag: one raw byte (we are guaranteed pos<n by the eof loop condition)
        tag = extra[pos]; pos += 1
        if tag == 0x00:        # PADDING
            size = 1
            fail = False
            while size <= 255:
                if pos >= n: break
                b = extra[pos]; pos += 1
                if b != 0: return (False, fields)
                size += 1
            if size > 255: return (False, fields)
            fields.append(('P', size))
        elif tag == 0x01:      # PUBKEY
            if n - pos < 32:
                return (False, fields)
            fields.append(('K', extra[pos:pos+32].hex())); pos += 32
        elif tag in (0x02, 0xDE):   # length-prefixed byte string
            st, nread, ln, pos = read_varint(extra, pos)
            good = nread >= 1
            remaining = (n - pos) if good else 0
            if remaining < ln:
                return (False, fields)
            data = extra[pos:pos+ln]; pos += ln
            if not good:
                return (False, fields)
            if tag == 0x02:
                if ln > 255: return (False, fields)
                fields.append(('N', data.hex()))
            else:
                fields.append(('G', data.hex()))
        elif tag == 0x03:      # MERGE MINING
            st, nread, ln, pos = read_varint(extra, pos)
            good = nread >= 1
            remaining = (n - pos) if good else 0
            if remaining < ln:
                return (False, fields)
            inner = extra[pos:pos+ln]; pos += ln
            if not good:
                return (False, fields)
            # inner archive
            ist, inread, depth, ipos = read_varint(inner, 0)
            if inread < 1: return (False, fields)
            if len(inner) - ipos < 32: return (False, fields)
            root = inner[ipos:ipos+32]; ipos += 32
            if ipos != len(inner): return (False, fields)   # check_stream_state: must be eof
            fields.append(('M', depth, root.hex()))
        elif tag == 0x04:      # ADDITIONAL PUBKEYS
            st, nread, cnt, pos = read_varint(extra, pos)
            if nread < 1: return (False, fields)
            remaining = n - pos
            if remaining < cnt: return (False, fields)
            keys = []
            for i in range(cnt):
                if n - pos < 32: return (False, fields)
                keys.append(extra[pos:pos+32].hex()); pos += 32
            fields.append(('A', keys))
        else:
            return (False, fields)   # unknown variant tag -> set_fail
        if pos >= n:
            break
    return (True, fields)

# The rendering batch.cpp uses, byte for byte, so difffuzz.py and exhaustive.py
# compare VALUES and not merely shapes. A format that stops at the tag and the
# element count agrees with any parser that gets the framing right, including
# one that truncates a merge-mining depth or transposes two keys.
def fmt(res):
    ok, fields = res
    s = ('OK' if ok else 'FAIL') + ' n=%d tags=' % len(fields)
    for f in fields:
        if f[0]=='P': s += 'P%d,' % f[1]
        elif f[0]=='K': s += 'K:%s,' % f[1]
        elif f[0]=='N': s += 'N%d:%s,' % (len(f[1])//2, f[1])
        elif f[0]=='M': s += 'M:%d:%s,' % (f[1], f[2])
        elif f[0]=='A': s += 'A%d:%s,' % (len(f[1]), ''.join(f[1]))
        elif f[0]=='G': s += 'G%d:%s,' % (len(f[1])//2, f[1])
    return s
