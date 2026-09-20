import subprocess, sys, itertools
sys.path.insert(0, __import__('os').path.dirname(__file__) or '.')
from model import parse, fmt
cases=[]
for a in range(256): cases.append(bytes([a]))
for a in range(256):
    for b in range(256): cases.append(bytes([a,b]))
for a in (0,1,2,3,4,5,0xDE):
    for b in range(256):
        for c in range(256): cases.append(bytes([a,b,c]))
print('cases', len(cases))
inp='\n'.join(c.hex() for c in cases)
out=subprocess.run([__import__('os').path.join(__import__('os').path.dirname(__file__) or '.','batch')],input=inp,capture_output=True,text=True).stdout.split('\n')
bad=0
for c,line in zip(cases,out):
    o=line.split(); ctxt=o[0]+' '+o[1]+' '+o[3]
    m=fmt(parse(c))
    if ctxt!=m:
        bad+=1
        if bad<=10: print('MISMATCH',c.hex(),'| C++:',ctxt,'| model:',m)
print('mismatches',bad)
