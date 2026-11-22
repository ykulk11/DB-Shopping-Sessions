#!/usr/bin/env python3
"""Adversarial dataset generator: gen.py SEED OUTDIR [EVENT_BYTES]"""
import os, random, sys, json, datetime
seed = int(sys.argv[1]); out = sys.argv[2]
EVB = int(sys.argv[3]) if len(sys.argv) > 3 else 2_400_000
R = random.Random(seed)
WS = 1790812800000  # 2026-10-01
WE = WS + 14*86400000
DAY = 86400000

def civil(ms):
    return datetime.datetime.fromtimestamp(ms/1000, datetime.timezone.utc)
def f1(ms): return civil(ms - ms % 1000).strftime('%Y-%m-%dT%H:%M:%SZ').encode()
def f2(ms):
    off = R.randint(-600, 720) // 15 * 15
    d = civil(ms - ms % 1000 + off*60000)
    s = '+' if off >= 0 else '-'; a = abs(off)
    return (d.strftime('%Y-%m-%dT%H:%M:%S') + '%s%02d:%02d' % (s, a//60, a%60)).encode()
def ts(ms):
    r = R.random()
    if ms % 1000 or r < .35:
        return (b'0'*R.choice([0,0,0,1,3])) + str(ms).encode()
    return f1(ms) if r < .8 else f2(ms)

def hexs(): return ''.join(R.choice('0123456789abcdef') for _ in range(32))
def mkid(p, i):
    s = '%s%s' % (p, format(i*7919 % 100003, 'x'))
    if R.random() < 1/8: s += hexs()
    return s.encode()

people = []
for i in range(120):
    n = R.choice([1,1,1,2,2,3,5])
    if i < 2: n = 60   # chain people
    people.append([mkid('u', i*100+j) for j in range(n)])
users = [x for p in people for x in p]
prods = [b'p%d' % i for i in range(30)]
cats = [b'shoes', b'books', b'toys', b'unknowm', b'Unknown']
camps = [b'c_%d' % i for i in range(12)]

def q(f):
    return b'"' + f + b'"' if R.random() < 0.2 else f

def nasty_field(f):
    k = R.randrange(10)
    return [b'"', f[:1]+b'"'+f[1:], b'"'+f, f+b'"', b'""'+f+b'""', b'"'+f+b'""', f+b'""', b'""""', b'"""', b'"'+f[:1]+b'""'+f[1:]+b'"'][k]

def junk_line(n):
    k = R.randrange(14)
    if k == 0: return b''
    if k == 1: return b',' * R.randint(1, 40)
    if k == 2: return b'"' * R.randint(1, 40)
    if k == 3: return b','.join([b'""'] * n)
    if k == 4: return b','.join([b'x'] * R.choice([m for m in range(1, 21) if m != n]))
    if k == 5: return b'",' * R.randint(1, 20)
    if k == 6: return b',"' * R.randint(1, 20)
    if k == 7: return b'"",' * (n-1) + b'"'
    if k == 8: return b'\r'
    if k == 9: return b'\x00' * R.randint(1, 9)
    if k == 10: return ','.join(['é'] * (n+1)).encode()
    if k == 11: return b','.join([b'"a'] + [b'b'] * (n-2) + [b'c"'])
    if k == 12: return b'"' + b','.join([b'a'] * n) + b'"'
    return b','.join([b'a""b'] * n)

def mangle(fields, protect=-1):
    fields = list(fields)
    keep = fields[protect] if protect >= 0 else None
    r = R.random()
    if r < 0.06:
        i = R.choice([0, len(fields)//2, len(fields)-1, R.randrange(len(fields))])
        if i == protect: i = 2
        fields[i] = nasty_field(fields[i])
        return b','.join(fields)
    if r < 0.08:
        return junk_line(len(fields))
    if r < 0.09:
        return b','.join(fields + [b'x'])
    if r < 0.10:
        return b','.join(fields[:-1])
    if protect >= 0: fields[protect] = keep
    return b','.join(q(f) for f in fields)

def vmut(fields, protect=-1):
    fields = list(fields)
    keep = fields[protect] if protect >= 0 else None
    r = R.random()
    if r < 0.005:
        fields[-1] = fields[-1] + b'\r'
    elif r < 0.01:
        i = R.randrange(len(fields)); fields[i] = b' ' + fields[i] if R.random() < .5 else fields[i] + b' '
    elif r < 0.015:
        i = R.randrange(len(fields)); fields[i] = fields[i] + R.choice([b'\x00', 'é'.encode(), b'\x01'])
    elif r < 0.02:
        i = R.randrange(len(fields)); fields[i] = fields[i] + b'z' * R.choice([300, 700, 5000, 40000])
    if protect >= 0: fields[protect] = keep
    return fields

def write(path, header, lines, final_nl=True):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    data = header + b'\n' + b'\n'.join(lines)
    if lines and final_nl: data += b'\n'
    if not lines and not final_nl: data = header
    open(path, 'wb').write(data)

ev = []; sess = []; eid = [0]
def new_eid():
    eid[0] += 1
    s = 'e%x' % (eid[0]*104729 % 10**8)
    if R.random() < 1/8: s += hexs()
    if R.random() < 0.05: s += 'q' * R.randint(1, 120)
    return s.encode()
size = 0
while size < EVB:
    p = R.choice(people)
    t = R.randrange(WS - 3600000, WE + 3600000)
    sess.append((p, t))
    for _ in range(R.randint(1, 8)):
        u = R.choice(p) if R.random() < .5 else p[0]
        ty = R.choice([b'view']*6 + [b'cart']*2 + [b'purchase']*2 + [b'Purchase', b'cart '])
        price = str(R.randint(1, 50000)).encode() if ty == b'purchase' else R.choice([b'', b'12', b'-5'])
        if ty == b'purchase' and R.random() < .1: price = R.choice([b'0', b'0250', b'10000000', b'10000001', b'1.5', b'+5', b''])
        prod = R.choice(prods + [b'P1', b'', b'p999'])
        ing = t + R.choice([1000, 5000, 60000, 3600000, DAY, DAY+1000, DAY-1000, 2*DAY, -5000])
        tsb = ts(t) if R.random() > 0.01 else R.choice([b'2026-10-03 10:00:00', b'2026-02-30T00:00:00Z', b'2026-10-03T10:00:00+0200', b'1.79e12', b''])
        e = [new_eid(), u if R.random() > .003 else b'', tsb, f1(ing), ty, prod, price]
        e = vmut(e, 3)
        ev.append(e)
        if R.random() < 0.05:
            for off in R.sample([0, 1000, 600000, -1000, 7000], R.randint(1, 3)):
                d = list(e)
                d[3] = f1(ing + off)
                if off == 0 or f1(ing + off) == e[3]:
                    ev.append(list(e)); continue
                if R.random() < 0.3: d[1] = R.choice(users)
                if R.random() < 0.2: d[2] = R.choice([b'bad', ts(t + 60000)])
                if R.random() < 0.2: d[4] = R.choice([b'view', b'xx'])
                ev.append(d)
        t += R.choice([0, 0, 1000, 60000, 1800000, 1800001, 1799999, 600000])
        size += 100
R.shuffle(ev)
lines = [mangle(e, 3) for e in ev]
H = b'event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents'
cut = R.randint(len(lines)//10, len(lines)//4)
write(f'{out}/events/part-00000.csv', H + b'x' * R.randint(0, 200), lines[cut:], final_nl=R.random() < .5)
write(f'{out}/events/part-00001.csv', H, lines[:cut], final_nl=R.random() < .5)
write(f'{out}/events/part-00002.csv', H, [], final_nl=R.random() < .5)
open(f'{out}/events/part-00003.csv', 'wb').write(b'')
write(f'{out}/events/part-00004.csv', H, [H, b'x1,u1,' + str(WS+5).encode() + b',2026-10-01T00:00:00Z,view,p1,'], final_nl=False)

cl = []; cid = 0
for (p, s) in sess:
    if R.random() < .6:
        for _ in range(R.randint(1, 4)):
            cid += 1
            off = R.choice([0, 1, -1, 7*DAY, 7*DAY+1, 7*DAY-1, R.randrange(7*DAY), 60000])
            t = s - off
            c = [b'k%x' % (cid*31 % 999983) + (hexs().encode() if R.random() < 1/8 else b''), R.choice(p), R.choice(camps + [b'C_1', b'c_99']), ts(t)]
            c = vmut(c)
            cl.append(c)
            if R.random() < .15:
                cid += 1
                cl.append([b'k%x' % (cid*31 % 999983), R.choice(p), R.choice(camps), ts(t)])
            if R.random() < .05:
                cl.append(list(c))
R.shuffle(cl)
write(f'{out}/clicks/part-00000.csv', b'click_id,user_id,campaign_id,ts' + b'y'*R.randint(0, 64), [mangle(c) for c in cl], final_nl=R.random() < .5)
write(f'{out}/clicks/part-00001.csv', b'click_id,user_id,campaign_id,ts', [], final_nl=R.random() < .5)

lk = []
for p in people:
    if len(p) > 1:
        order = list(p)
        if len(p) >= 60: R.shuffle(order)
        for a, b in zip(order, order[1:]):
            lk.append([a, b] if R.random() < .5 else [b, a])
            if R.random() < .08: lk.append([b, a])
    if R.random() < .05: lk.append([p[0], p[0]])
    if R.random() < .05: lk.append([p[0], b'a' + p[0]])
    if R.random() < .02: lk.append([p[0], p[0].upper()])
for _ in range(5):
    a, b = R.choice(people), R.choice(people)
    lk.append([a[0], b[-1]])
lk = [vmut(l) for l in lk]
R.shuffle(lk)
write(f'{out}/links/part-00000.csv', b'user_id_a,user_id_b', [mangle(l) for l in lk], final_nl=R.random() < .5)
write(f'{out}/links/part-00001.csv', b'user_id_a,user_id_b', [b'user_id_a,user_id_b'], final_nl=False)

write(f'{out}/products.csv', b'product_id,category', [p + b',' + R.choice(cats) for p in prods])
write(f'{out}/campaigns.csv', b'campaign_id,channel', [c + b',' + R.choice([b'search', b'social', b'email']) for c in camps])
json.dump({"window_start": "2026-10-01T00:00:00Z", "window_end": "2026-10-15T00:00:00Z"}, open(f'{out}/manifest.json', 'w'))
