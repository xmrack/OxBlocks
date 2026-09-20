import random, subprocess, sys
sys.path.insert(0, __import__('os').path.dirname(__file__) or '.')
from model import parse, fmt
random.seed(int(sys.argv[1]) if len(sys.argv)>1 else 1)
N = int(sys.argv[2]) if len(sys.argv)>2 else 20000
TAGS=[0x00,0x01,0x02,0x03,0x04,0xDE,0x05,0xff,0x7f]
def gen():
    mode = random.randrange(5)
    if mode==0:
        return bytes(random.randrange(256) for _ in range(random.randrange(0,40)))
    if mode==1:
        out=bytearray()
        for _ in range(random.randrange(1,5)):
            out.append(random.choice(TAGS))
            k=random.randrange(4)
            if k==0: out += bytes(random.randrange(256) for _ in range(random.randrange(0,40)))
            elif k==1: out += bytes(32)
            elif k==2: out += bytes([random.choice([0,1,2,8,9,0x20,0x21,0x22,0x7f,0x80,0xff])])
            else: out += bytes([random.randrange(256) for _ in range(random.randrange(0,300))])
        return bytes(out)
    if mode==2:  # well-formed-ish
        out=bytearray()
        for _ in range(random.randrange(1,4)):
            t=random.choice([0x01,0x02,0x03,0x04,0xDE,0x00])
            if t==0x01: out.append(1); out+=bytes(random.randrange(256) for _ in range(32))
            elif t in (0x02,0xDE):
                l=random.choice([0,1,8,9,33,254,255,256])
                out.append(t); out+=varint(l); out+=bytes(random.randrange(256) for _ in range(l))
            elif t==0x03:
                d=random.choice([0,1,127,128,300])
                inner=varint(d)+bytes(32)
                l=len(inner)+random.choice([0,0,0,-1,1])
                out.append(3); out+=varint(max(l,0)); out+=inner
            elif t==0x04:
                c=random.choice([0,1,2,3,255])
                out.append(4); out+=varint(c); out+=bytes(random.randrange(256) for _ in range(32*min(c,4)))
            else:
                out.append(0); out+=bytes(random.choice([0,1,2,38,253,254,255]))
        return bytes(out)
    if mode==3:  # varint torture
        out=bytearray([random.choice([0x02,0x03,0x04,0xDE])])
        L=random.randrange(1,12)
        out += bytes(random.choice([0x00,0x01,0x02,0x7f,0x80,0x81,0xff]) for _ in range(L))
        out += bytes(random.randrange(256) for _ in range(random.randrange(0,40)))
        return bytes(out)
    # mode 4: padding torture
    z=random.choice([0,1,2,253,254,255,256])
    out=bytearray([0x00])+bytes(z)
    if random.random()<0.3: out+=bytes([random.randrange(1,256)])
    return bytes(out)
def varint(v):
    o=bytearray()
    while v>=0x80:
        o.append((v&0x7f)|0x80); v>>=7
    o.append(v); return bytes(o)
cases=[gen() for _ in range(N)]
inp='\n'.join(c.hex() for c in cases)
out=subprocess.run([__import__('os').path.join(__import__('os').path.dirname(__file__) or '.','batch')],input=inp,capture_output=True,text=True).stdout.split('\n')
bad=0
for c,line in zip(cases,out):
    if not c:
        continue
    o = line.split()
    ctxt = o[0]+' '+o[1]+' '+o[3]
    m = fmt(parse(c))
    if ctxt != m:
        bad += 1
        if bad<=12: print('MISMATCH', c.hex()[:200], '| C++:', ctxt, '| model:', m)
print('cases',len(cases),'mismatches',bad)
